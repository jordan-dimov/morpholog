//! Rule coverage over replayed history: which of these rules has
//! ever actually done work?
//!
//! Two logs feed the verdicts. Replaying the audit log shows whether a
//! rule's condition ever matched a committed state. The rejection log
//! shows whether the rule ever refused a proposal. The verdicts, strongest
//! first:
//!
//! - **constrained** - the rule refused at least one proposal. This is the
//!   only verdict an always-on prohibition can earn, since refusals never
//!   reach committed history.
//! - **fired** - the rule is an implication and its condition matched in
//!   at least one committed state, but it never refused anything.
//! - **never fired** - an implication whose condition never matched and
//!   that never refused anything. The rule has never been more than
//!   trivially true.
//! - **always on** - no implication to fire (a prohibition like
//!   `not (Retired(c, _) and HeldBy(c, _))`, a bare comparison). It holds
//!   in every committed state by construction.
//!
//! `constrained` is a floor, not a census: the rejection log records at
//! most once, and a crash after rollback loses that row. There is no "dead
//! antecedent" verdict; proving a condition can never match is a static
//! question this module does not ask.
//!
//! Coverage measures today's rules over the whole history.
//!
//! Classification looks through definition calls, so an implication behind
//! a named condition still counts. An antecedent inside a definition is
//! evaluated with its call's arguments, so a literal or already-bound
//! argument narrows it. Known gap: a call repeating an unbound variable
//! (`f(x, x)`) loses that equality and can overcount `fired`.
//!
//! The PostgreSQL driver replays the audit log, calling
//! [`CoverageTracker::observe`] per transition. Only invariants whose
//! antecedent predicates the transition touched are evaluated, which keeps
//! a long replay fast. It then calls [`CoverageTracker::observe_rejection`]
//! per rejection-log row, which only counts.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::definitions::DefinitionTable;
use crate::eval::{EvalContext, EvalError, definition_call_frame, find_matches};
use crate::fold::mentions_pre;
use crate::ir::{Definition, InvariantOrigin, PredicateName, Program, Prop};
use crate::lint::implications_of;
use crate::predicates_referenced_by_prop;
use crate::state::{Bindings, State};

/// Coverage verdict for one invariant, strongest first. The module doc
/// defines each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageVerdict {
    Constrained,
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
    /// Proposals this invariant refused, per the rejection log. A
    /// floor, not a census - the log records at-most-once.
    #[serde(skip_serializing_if = "is_zero")]
    pub proposals_refused: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_refused: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_refused: Option<String>,
    /// True when the rejection log names an invariant the current
    /// programme no longer declares.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub not_in_programme: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Usage of one transformation over the replayed history. Declared
/// transformations appear even when never used. A name seen in history but
/// no longer declared appears too, flagged.
#[derive(Debug, Clone, Serialize)]
pub struct TransformationUsage {
    pub transformation: String,
    pub transitions: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last: Option<String>,
    /// Proposals of this transformation that were refused, by one of its
    /// gates or by any invariant.
    #[serde(skip_serializing_if = "is_zero")]
    pub proposals_refused: u64,
    /// True when history names a transformation the current programme
    /// no longer declares.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub not_in_programme: bool,
}

/// The full coverage report: one entry per invariant in declaration
/// order (rejection-log-only names after, flagged), one per
/// transformation (declared first, historical-only after).
#[derive(Debug, Clone, Serialize)]
pub struct CoverageReport {
    pub program: String,
    pub transitions_replayed: u64,
    pub rejections_replayed: u64,
    pub invariants: Vec<InvariantCoverage>,
    pub transformations: Vec<TransformationUsage>,
}

/// One antecedent to test for firing, with the chain of definition calls
/// (outermost first) it was found inside. It is evaluated with those
/// calls' arguments bound.
struct Antecedent<'p> {
    prop: &'p Prop,
    calls: Vec<crate::lint::DefinedCall<'p>>,
}

/// How one invariant participates in coverage.
enum Shape<'p> {
    /// One or more positive implications; coverage asks whether any
    /// antecedent ever binds. `footprint` is the predicates the
    /// antecedents read, used to skip transitions that cannot matter.
    /// `uses_pre` turns that skipping off: a claim admitted at step T is
    /// in the pre-state only from T+1, so a `pre(...)` antecedent can
    /// first bind on a transition that did not touch it.
    Implication {
        antecedents: Vec<Antecedent<'p>>,
        footprint: BTreeSet<PredicateName>,
        uses_pre: bool,
    },
    /// No positive implication, even through definitions. It holds in
    /// every committed state, so only refusals can show it working.
    AlwaysOn,
}

struct Entry<'p> {
    name: String,
    from: Option<String>,
    shape: Shape<'p>,
    transitions_fired: u64,
    first_fired: Option<String>,
    last_fired: Option<String>,
    refusals: Refusals,
}

/// Refusal counts from the rejection log, for declared and undeclared
/// invariant names alike.
#[derive(Default)]
struct Refusals {
    count: u64,
    first: Option<String>,
    last: Option<String>,
}

impl Refusals {
    fn record(&mut self, rejection_id: &str) {
        self.count += 1;
        self.first.get_or_insert_with(|| rejection_id.to_string());
        self.last = Some(rejection_id.to_string());
    }
}

#[derive(Default)]
struct Usage {
    transitions: u64,
    first: Option<String>,
    last: Option<String>,
    proposals_refused: u64,
}

/// Accumulates coverage over a transition-by-transition replay.
pub struct CoverageTracker<'p> {
    program_name: String,
    definitions: &'p [Definition],
    entries: Vec<Entry<'p>>,
    /// Invariant name -> position in `entries`; the rejection log can be
    /// long.
    entry_index: BTreeMap<String, usize>,
    declared_transformations: Vec<String>,
    usage: BTreeMap<String, Usage>,
    /// Refusals by invariant names the current programme does not
    /// declare, reported rather than dropped.
    unmatched_refusals: BTreeMap<String, Refusals>,
    transitions: u64,
    rejections: u64,
}

impl<'p> CoverageTracker<'p> {
    /// Classify every invariant of `program` and seed the usage table
    /// with its declared transformations (so dead ones appear at
    /// zero).
    pub fn new(program: &'p Program) -> Self {
        let provenance = crate::disciplines::discipline_provenance(program);
        let entries: Vec<Entry<'p>> = program
            .invariants
            .iter()
            .map(|inv| {
                let implications =
                    implications_of(&inv.body, DefinitionTable::new(&program.definitions));
                let shape = if implications.is_empty() {
                    Shape::AlwaysOn
                } else {
                    let antecedents: Vec<Antecedent<'p>> = implications
                        .into_iter()
                        .map(|i| Antecedent {
                            prop: i.antecedent,
                            calls: i.calls,
                        })
                        .collect();
                    let mut footprint = BTreeSet::new();
                    for antecedent in &antecedents {
                        predicates_referenced_by_prop(
                            antecedent.prop,
                            &program.definitions,
                            &mut footprint,
                        );
                    }
                    let uses_pre = antecedents.iter().any(|a| mentions_pre(a.prop));
                    Shape::Implication {
                        antecedents,
                        footprint,
                        uses_pre,
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
                    refusals: Refusals::default(),
                }
            })
            .collect();
        Self {
            program_name: program.name.clone(),
            definitions: &program.definitions,
            entry_index: entries
                .iter()
                .enumerate()
                .map(|(i, e)| (e.name.clone(), i))
                .collect(),
            entries,
            declared_transformations: program
                .transformations
                .iter()
                .map(|t| t.name.to_string())
                .collect(),
            usage: BTreeMap::new(),
            unmatched_refusals: BTreeMap::new(),
            transitions: 0,
            rejections: 0,
        }
    }

    /// Record one rejection-log row. Counts only; evaluates nothing.
    ///
    /// `invariant` names the refusing invariant, or is `None` for a gate
    /// (`require` / `bind`) refusal. A gate refusal counts for the
    /// transformation only; an invariant refusal counts for both. Unknown
    /// invariant names are kept separately and reported flagged.
    pub fn observe_rejection(
        &mut self,
        invariant: Option<&str>,
        transformation: &str,
        rejection_id: &str,
    ) {
        self.rejections += 1;
        self.usage
            .entry(transformation.to_string())
            .or_default()
            .proposals_refused += 1;
        if let Some(name) = invariant {
            if let Some(&i) = self.entry_index.get(name) {
                self.entries[i].refusals.record(rejection_id);
            } else {
                self.unmatched_refusals
                    .entry(name.to_string())
                    .or_default()
                    .record(rejection_id);
            }
        }
    }

    /// True when this transition needs a state snapshot: `delta` touches
    /// a tracked antecedent's predicates, or some antecedent reads the
    /// pre-state. An irrelevant transition is still counted but evaluates
    /// nothing, so the driver may pass any state.
    pub fn delta_is_relevant(&self, delta: &BTreeSet<PredicateName>) -> bool {
        self.entries.iter().any(|entry| match &entry.shape {
            Shape::Implication {
                footprint,
                uses_pre,
                ..
            } => *uses_pre || footprint.intersection(delta).next().is_some(),
            Shape::AlwaysOn => false,
        })
    }

    /// True when any tracked antecedent contains `pre(...)`, so the driver
    /// must keep the previous state at every step. When false, the
    /// pre-state argument is never read.
    pub fn needs_pre_state(&self) -> bool {
        self.entries.iter().any(|entry| match &entry.shape {
            Shape::Implication { uses_pre, .. } => *uses_pre,
            Shape::AlwaysOn => false,
        })
    }

    /// Record one replayed transition. `post_state` is the state after it
    /// committed; `pre_state` the state before (the empty state for the
    /// first transition, so `pre(...)` antecedents still evaluate);
    /// `delta` the predicates it admitted or retracted.
    ///
    /// Only invariants whose antecedent reads a predicate in `delta` are
    /// evaluated: an antecedent that gained no claims cannot have started
    /// binding. Antecedents that read the pre-state are always evaluated.
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
                uses_pre,
            } = &entry.shape
            else {
                continue;
            };
            if !uses_pre && footprint.intersection(delta).next().is_none() {
                continue;
            }
            let index = DefinitionTable::new(self.definitions);
            let mut fired = false;
            for antecedent in antecedents {
                // Bind each call's arguments in turn, so the antecedent
                // answers for the arguments the call site passed, not for
                // any arguments at all.
                let mut scope = bindings.clone();
                for (name, args) in &antecedent.calls {
                    let def = index
                        .get(name)
                        .ok_or_else(|| EvalError::UnknownDefinition(name.to_string()))?;
                    let ctx = EvalContext::new(post_state, Some(pre_state), &scope, None, index);
                    scope = definition_call_frame(def, args, &ctx)?;
                }
                let ctx = EvalContext::new(post_state, Some(pre_state), &scope, None, index);
                if !find_matches(antecedent.prop, &ctx)?.is_empty() {
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

    /// Build the report. Any refusal makes an invariant `constrained`,
    /// whatever its shape. Declared names come first, in declaration
    /// order; names seen only in history follow.
    pub fn into_report(mut self) -> CoverageReport {
        let mut invariants: Vec<InvariantCoverage> = self
            .entries
            .into_iter()
            .map(|entry| {
                let verdict = if entry.refusals.count > 0 {
                    CoverageVerdict::Constrained
                } else {
                    match entry.shape {
                        Shape::AlwaysOn => CoverageVerdict::AlwaysOn,
                        Shape::Implication { .. } if entry.transitions_fired > 0 => {
                            CoverageVerdict::Fired
                        }
                        Shape::Implication { .. } => CoverageVerdict::NeverFired,
                    }
                };
                InvariantCoverage {
                    invariant: entry.name,
                    from: entry.from,
                    verdict,
                    transitions_fired: entry.transitions_fired,
                    first_fired: entry.first_fired,
                    last_fired: entry.last_fired,
                    proposals_refused: entry.refusals.count,
                    first_refused: entry.refusals.first,
                    last_refused: entry.refusals.last,
                    not_in_programme: false,
                }
            })
            .collect();
        // Invariant names the programme no longer declares.
        for (name, refusals) in self.unmatched_refusals {
            invariants.push(InvariantCoverage {
                invariant: name,
                from: None,
                verdict: CoverageVerdict::Constrained,
                transitions_fired: 0,
                first_fired: None,
                last_fired: None,
                proposals_refused: refusals.count,
                first_refused: refusals.first,
                last_refused: refusals.last,
                not_in_programme: true,
            });
        }

        let mut transformations = Vec::new();
        for name in &self.declared_transformations {
            let usage = self.usage.remove(name).unwrap_or_default();
            transformations.push(TransformationUsage {
                transformation: name.clone(),
                transitions: usage.transitions,
                first: usage.first,
                last: usage.last,
                proposals_refused: usage.proposals_refused,
                not_in_programme: false,
            });
        }
        // Whatever remains was seen in history but is not declared today.
        for (name, usage) in self.usage {
            transformations.push(TransformationUsage {
                transformation: name,
                transitions: usage.transitions,
                first: usage.first,
                last: usage.last,
                proposals_refused: usage.proposals_refused,
                not_in_programme: true,
            });
        }

        CoverageReport {
            program: self.program_name,
            transitions_replayed: self.transitions,
            rejections_replayed: self.rejections,
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
        "Rule coverage of `{}` over {} committed transition(s) and {} recorded rejection(s):\n",
        report.program, report.transitions_replayed, report.rejections_replayed
    );

    out.push_str("\ninvariants:\n");
    for inv in &report.invariants {
        match inv.verdict {
            CoverageVerdict::Constrained => {
                out.push_str(&format!(
                    "\n  {} - CONSTRAINED: refused {} proposal(s); the rule has \
                     demonstrably done its job\n",
                    inv.invariant, inv.proposals_refused
                ));
                if let (Some(first), Some(last)) = (&inv.first_refused, &inv.last_refused) {
                    out.push_str(&format!(
                        "    first refusal: {first}\n    last refusal:  {last}\n"
                    ));
                }
                if inv.transitions_fired > 0 {
                    out.push_str(&format!(
                        "    also fired in {} committed transition(s)\n",
                        inv.transitions_fired
                    ));
                }
                if inv.not_in_programme {
                    out.push_str(
                        "    note: appears in the rejection log but the current programme \
                         does not declare it\n",
                    );
                }
            }
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
                    "\n  {} - always on: holds over every committed state; no recorded \
                     refusals yet\n",
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
        if t.transitions == 0 && t.proposals_refused == 0 {
            out.push_str(&format!("\n  {} - never used\n", t.transformation));
        } else if t.transitions == 0 {
            // Only ever refused: "0 transition(s)" would read as never
            // proposed.
            out.push_str(&format!(
                "\n  {} - never committed a transition\n",
                t.transformation
            ));
        } else {
            out.push_str(&format!(
                "\n  {} - {} transition(s)\n",
                t.transformation, t.transitions
            ));
        }
        if t.proposals_refused > 0 {
            out.push_str(&format!(
                "    refused: {} proposal(s)\n",
                t.proposals_refused
            ));
        }
        if t.not_in_programme {
            out.push_str(
                "    note: appears in history but the current programme does not declare it\n",
            );
        }
    }

    out.push_str(
        "\nHow to read this: `constrained` means the rule refused at least one real \
         proposal, per the operational rejection log - a floor, not a census, because \
         that log is recorded after each rollback, at-most-once, outside the \
         legitimacy-grade audit record. `fired` means the rule's condition matched \
         real records and the rule was genuinely evaluated (but never refused); \
         `never fired` means it has only ever been trivially true. Replay cannot \
         prove a condition could never match (that is static analysis, not replay). \
         Coverage evaluates the current programme's rules over the recorded history.",
    );
    out
}
