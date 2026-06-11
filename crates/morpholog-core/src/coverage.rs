//! Rule coverage over replayed history: which of these rules has
//! ever actually done work?
//!
//! One fact bounds the whole feature: the audit log records only
//! COMMITTED transitions. A rejected proposal never commits and
//! leaves no row, so "this rule refused N proposals" is unknowable
//! from history. The verdicts here are the honest ones that committed
//! history supports:
//!
//! - **fired** - the invariant has implication shape and at least one
//!   antecedent bound at least one witness in the post-state of at
//!   least one replayed transition. The rule has evaluated something
//!   real.
//! - **never fired** - implication shape, antecedent never bound
//!   across the whole history: dynamically vacuous. The rule has
//!   never evaluated anything beyond trivially-true, whatever its
//!   text promises. The headline verdict.
//! - **always on** - no positive-polarity implication (a prohibition
//!   like `not (Retired(c, _) and HeldBy(c, _))`, a bare comparison):
//!   the rule holds over every committed state by construction, and
//!   its work - refusing proposals - is structurally invisible in
//!   committed history. Its own verdict, never conflated with fired.
//!
//! Two further verdicts are deliberately absent, named in the report
//! legend: *constrained* (refused a real proposal - needs a rejection
//! log that does not exist yet) and *dead antecedent* (an antecedent
//! that CANNOT bind - static satisfiability, the offline-oracle tier).
//!
//! Coverage measures the CURRENT programme's rules over history: "has
//! this rule ever done work" is a question about today's rules. The
//! audit rows' `invariants_checked` column is the substrate for a
//! later "when did this rule enter service" tier, not consulted here.
//!
//! The driver (the PG adapter's `coverage_replay`) folds the audit
//! log transition by transition and calls [`CoverageTracker::observe`]
//! with each post-state and the transition's delta predicates; the
//! tracker evaluates only the invariants whose antecedent footprint
//! intersects the delta, which is what keeps a long replay tractable.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::definitions::DefinitionIndex;
use crate::eval::{EvalContext, EvalError, find_matches};
use crate::ir::{Definition, InvariantOrigin, PredicateName, Program, Prop};
use crate::lint::collect_implications;
use crate::predicates_referenced_by_prop;
use crate::state::{Bindings, State};

/// Coverage verdict for one invariant. See the module doc for the
/// precise meaning of each - and for the two verdicts deliberately
/// absent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageVerdict {
    Fired,
    NeverFired,
    AlwaysOn,
}

/// Coverage of one invariant over the replayed history.
#[derive(Debug, Clone, Serialize)]
pub struct InvariantCoverage {
    pub invariant: String,
    /// Discipline provenance, for generated invariants - the same
    /// `from:` line `inspect guarantees` shows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub verdict: CoverageVerdict,
    /// Transitions whose post-state bound at least one antecedent
    /// witness. Always 0 for `always_on`.
    pub transitions_fired: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_fired: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_fired: Option<String>,
}

/// Usage of one transformation over the replayed history. Declared
/// transformations appear even at zero - a never-used transformation
/// is the same auditor question one level up. A name seen in history
/// but absent from the current programme appears too, flagged.
#[derive(Debug, Clone, Serialize)]
pub struct TransformationUsage {
    pub transformation: String,
    pub transitions: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
    /// True when history names a transformation the current programme
    /// no longer declares - vocabulary drift the auditor should see.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub not_in_programme: bool,
}

/// The full coverage report: one entry per invariant in declaration
/// order, one per transformation (declared first, historical-only
/// after).
#[derive(Debug, Clone, Serialize)]
pub struct CoverageReport {
    pub program: String,
    pub transitions_replayed: u64,
    pub invariants: Vec<InvariantCoverage>,
    pub transformations: Vec<TransformationUsage>,
}

/// How one invariant participates in coverage.
enum Shape<'p> {
    /// One or more positive-polarity implications; coverage asks
    /// whether any antecedent ever binds. `footprint` is the union of
    /// the antecedents' referenced predicates (transitive through
    /// definitions), the delta-pruning key.
    Implication {
        antecedents: Vec<&'p Prop>,
        footprint: BTreeSet<PredicateName>,
    },
    /// No positive-polarity implication: holds over every committed
    /// state by construction; nothing to measure here.
    AlwaysOn,
}

struct Entry<'p> {
    name: String,
    from: Option<String>,
    shape: Shape<'p>,
    transitions_fired: u64,
    first_fired: Option<String>,
    last_fired: Option<String>,
}

#[derive(Default)]
struct Usage {
    transitions: u64,
    first: Option<String>,
    last: Option<String>,
}

/// Accumulates coverage over a transition-by-transition replay.
pub struct CoverageTracker<'p> {
    program_name: String,
    definitions: &'p [Definition],
    entries: Vec<Entry<'p>>,
    declared_transformations: Vec<String>,
    usage: BTreeMap<String, Usage>,
    transitions: u64,
}

impl<'p> CoverageTracker<'p> {
    /// Classify every invariant of `program` and seed the usage table
    /// with its declared transformations (so dead ones appear at
    /// zero).
    pub fn new(program: &'p Program) -> Self {
        let provenance = crate::disciplines::discipline_provenance(program);
        let entries = program
            .invariants
            .iter()
            .map(|inv| {
                let mut implications = Vec::new();
                collect_implications(&inv.body, true, &mut implications);
                let shape = if implications.is_empty() {
                    Shape::AlwaysOn
                } else {
                    let antecedents: Vec<&Prop> = implications
                        .iter()
                        .map(|(antecedent, _)| *antecedent)
                        .collect();
                    let mut footprint = BTreeSet::new();
                    for antecedent in &antecedents {
                        predicates_referenced_by_prop(
                            antecedent,
                            &program.definitions,
                            &mut footprint,
                        );
                    }
                    Shape::Implication {
                        antecedents,
                        footprint,
                    }
                };
                let from = match inv.origin {
                    InvariantOrigin::Discipline => provenance.get(inv.name.as_str()).cloned(),
                    InvariantOrigin::Authored => None,
                };
                Entry {
                    name: inv.name.to_string(),
                    from,
                    shape,
                    transitions_fired: 0,
                    first_fired: None,
                    last_fired: None,
                }
            })
            .collect();
        Self {
            program_name: program.name.clone(),
            definitions: &program.definitions,
            entries,
            declared_transformations: program
                .transformations
                .iter()
                .map(|t| t.name.to_string())
                .collect(),
            usage: BTreeMap::new(),
            transitions: 0,
        }
    }

    /// Record one replayed transition: `post_state` is the state after
    /// it committed, `pre_state` the state before (the empty state for
    /// the first transition - never `None`, so `pre(...)` antecedents
    /// evaluate instead of erroring), `delta` the predicates its
    /// asserted and retracted claims touch.
    ///
    /// Only invariants whose antecedent footprint intersects `delta`
    /// are evaluated - an antecedent that gained no new claims cannot
    /// have started binding, and one that lost claims either still
    /// binds (counted earlier) or stopped (nothing new to count).
    pub fn observe(
        &mut self,
        post_state: &State,
        pre_state: &State,
        delta: &BTreeSet<PredicateName>,
        transition_id: &str,
        transformation: &str,
    ) -> Result<(), EvalError> {
        self.transitions += 1;
        let usage = self.usage.entry(transformation.to_string()).or_default();
        usage.transitions += 1;
        usage.first.get_or_insert_with(|| transition_id.to_string());
        usage.last = Some(transition_id.to_string());

        let bindings = Bindings::new();
        for entry in &mut self.entries {
            let Shape::Implication {
                antecedents,
                footprint,
            } = &entry.shape
            else {
                continue;
            };
            if footprint.intersection(delta).next().is_none() {
                continue;
            }
            let ctx = EvalContext::new(
                post_state,
                Some(pre_state),
                &bindings,
                None,
                DefinitionIndex::new(self.definitions),
            );
            let mut fired = false;
            for antecedent in antecedents {
                if !find_matches(antecedent, &ctx)?.is_empty() {
                    fired = true;
                    break;
                }
            }
            if fired {
                entry.transitions_fired += 1;
                entry
                    .first_fired
                    .get_or_insert_with(|| transition_id.to_string());
                entry.last_fired = Some(transition_id.to_string());
            }
        }
        Ok(())
    }

    /// Finish: verdicts from the accumulated stats, transformations in
    /// declaration order first, historical-only names after.
    pub fn into_report(mut self) -> CoverageReport {
        let invariants = self
            .entries
            .into_iter()
            .map(|entry| {
                let verdict = match entry.shape {
                    Shape::AlwaysOn => CoverageVerdict::AlwaysOn,
                    Shape::Implication { .. } if entry.transitions_fired > 0 => {
                        CoverageVerdict::Fired
                    }
                    Shape::Implication { .. } => CoverageVerdict::NeverFired,
                };
                InvariantCoverage {
                    invariant: entry.name,
                    from: entry.from,
                    verdict,
                    transitions_fired: entry.transitions_fired,
                    first_fired: entry.first_fired,
                    last_fired: entry.last_fired,
                }
            })
            .collect();

        let mut transformations = Vec::new();
        for name in &self.declared_transformations {
            let usage = self.usage.remove(name).unwrap_or_default();
            transformations.push(TransformationUsage {
                transformation: name.clone(),
                transitions: usage.transitions,
                first: usage.first,
                last: usage.last,
                not_in_programme: false,
            });
        }
        // Whatever remains was seen in history but is not declared
        // today - vocabulary drift, surfaced rather than dropped.
        for (name, usage) in self.usage {
            transformations.push(TransformationUsage {
                transformation: name,
                transitions: usage.transitions,
                first: usage.first,
                last: usage.last,
                not_in_programme: true,
            });
        }

        CoverageReport {
            program: self.program_name,
            transitions_replayed: self.transitions,
            invariants,
            transformations,
        }
    }
}

/// Render a coverage report as auditor-readable prose, with the
/// legend that says what each verdict means and what committed
/// history structurally cannot show.
pub fn render_coverage(report: &CoverageReport) -> String {
    let mut out = format!(
        "Rule coverage of `{}` over {} committed transition(s):\n",
        report.program, report.transitions_replayed
    );

    out.push_str("\ninvariants:\n");
    for inv in &report.invariants {
        match inv.verdict {
            CoverageVerdict::Fired => {
                out.push_str(&format!(
                    "\n  {} - fired in {} transition(s)\n",
                    inv.invariant, inv.transitions_fired
                ));
                if let (Some(first), Some(last)) = (&inv.first_fired, &inv.last_fired) {
                    out.push_str(&format!("    first: {first}\n    last:  {last}\n"));
                }
            }
            CoverageVerdict::NeverFired => {
                out.push_str(&format!(
                    "\n  {} - NEVER FIRED: its condition never matched anything across \
                     the whole history; the rule has not yet done any work\n",
                    inv.invariant
                ));
            }
            CoverageVerdict::AlwaysOn => {
                out.push_str(&format!(
                    "\n  {} - always on: holds over every committed state; its work \
                     (refusing proposals) is not visible in committed history\n",
                    inv.invariant
                ));
            }
        }
        if let Some(from) = &inv.from {
            out.push_str(&format!("    from: {from}\n"));
        }
    }

    out.push_str("\ntransformations:\n");
    for t in &report.transformations {
        if t.transitions == 0 {
            out.push_str(&format!("\n  {} - never used\n", t.transformation));
        } else {
            out.push_str(&format!(
                "\n  {} - {} transition(s)\n",
                t.transformation, t.transitions
            ));
        }
        if t.not_in_programme {
            out.push_str(
                "    note: appears in history but the current programme does not declare it\n",
            );
        }
    }

    out.push_str(
        "\nHow to read this: `fired` means the rule's condition matched real records \
         and the rule was genuinely evaluated; `never fired` means it has only ever \
         been trivially true. Committed history cannot show how often a rule REFUSED \
         a proposal (rejections never commit, so they leave no audit row), and it \
         cannot prove a condition could never match (that is static analysis, not \
         replay). Coverage evaluates the current programme's rules over the recorded \
         history.",
    );
    out
}
