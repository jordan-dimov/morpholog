//! Rule coverage over replayed history: which of these rules has
//! ever actually done work?
//!
//! Two histories feed the verdicts. The audit log records committed
//! transitions; replaying it answers whether a rule's condition ever
//! matched anything real. The rejection log records refused
//! proposals - operational evidence written after each rollback,
//! at-most-once, outside the legitimacy-grade audit record - and
//! counting it answers the sharper question: did this rule ever
//! actually REFUSE something? The verdicts, strongest first:
//!
//! - **constrained** - the rule refused at least one real proposal,
//!   per the rejection log. The strongest evidence a rule can have,
//!   and the only verdict that can reach an always-on prohibition:
//!   refusals are exactly the work that committed history cannot
//!   show.
//! - **fired** - the invariant has implication shape and at least one
//!   antecedent bound at least one witness in the post-state of at
//!   least one replayed transition. The rule has evaluated something
//!   real (but never refused).
//! - **never fired** - implication shape, antecedent never bound
//!   across the whole history, no recorded refusals: dynamically
//!   vacuous. The rule has never evaluated anything beyond
//!   trivially-true, whatever its text promises. The headline
//!   verdict.
//! - **always on** - no positive-polarity implication (a prohibition
//!   like `not (Retired(c, _) and HeldBy(c, _))`, a bare comparison):
//!   the rule holds over every committed state by construction. Its
//!   own verdict, never conflated with fired - and superseded by
//!   `constrained` the moment the rejection log shows it refusing.
//!
//! One verdict remains deliberately absent, named in the report
//! legend: *dead antecedent* (an antecedent that CANNOT bind -
//! static satisfiability, the offline-oracle tier). And the
//! rejection log's at-most-once bound keeps `constrained` honest as
//! a floor, not a census: a crash between rollback and insert loses
//! that record.
//!
//! Coverage measures the CURRENT programme's rules over history: "has
//! this rule ever done work" is a question about today's rules. The
//! audit rows' `invariants_checked` column is the substrate for a
//! later "when did this rule enter service" tier, not consulted here.
//!
//! Shape classification descends through definition calls (the
//! every-walker-transitive red line): an implication hidden behind a
//! named condition is still an implication, and only a body with no
//! positive-polarity implication anywhere - including through its
//! definitions - classifies always-on.
//!
//! The driver (the PG adapter's `coverage_replay`) folds the audit
//! log transition by transition and calls [`CoverageTracker::observe`]
//! with each post-state and the transition's delta predicates; the
//! tracker evaluates only the invariants whose antecedent footprint
//! intersects the delta, which is what keeps a long replay tractable.
//! It then walks the rejection log and calls
//! [`CoverageTracker::observe_rejection`] per row - pure counting,
//! no state evaluation.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::definitions::DefinitionIndex;
use crate::eval::{EvalContext, EvalError, find_matches};
use crate::ir::{Definition, InvariantOrigin, PredicateName, Program, Prop};
use crate::lint::collect_implications;
use crate::predicates_referenced_by_prop;
use crate::state::{Bindings, State};

/// Coverage verdict for one invariant, strongest first. See the
/// module doc for the precise meaning of each - and for the verdict
/// deliberately absent.
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
    /// programme no longer declares - vocabulary drift the auditor
    /// should see, mirroring the transformation flag.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub not_in_programme: bool,
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_zero(n: &u64) -> bool {
    *n == 0
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
    /// Proposals of this transformation that were refused - by a
    /// gate of its own or by any invariant. A refusal is still a
    /// refused proposal OF the transformation, whichever rule said no.
    #[serde(skip_serializing_if = "is_zero")]
    pub proposals_refused: u64,
    /// True when history names a transformation the current programme
    /// no longer declares - vocabulary drift the auditor should see.
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

/// How one invariant participates in coverage.
enum Shape<'p> {
    /// One or more positive-polarity implications (the collector
    /// descends through definition calls, so an implication hidden
    /// behind a named condition still counts); coverage asks whether
    /// any antecedent ever binds. `footprint` is the union of the
    /// antecedents' referenced predicates (transitive through
    /// definitions), the delta-pruning key. `uses_pre` disables the
    /// prune: a `pre(...)` antecedent's firing opportunity lags the
    /// delta by one transition (the claim asserted at T sits in the
    /// PRE-state only from T+1), so pruning by the current delta
    /// would skip exactly the transition where it first binds.
    Implication {
        antecedents: Vec<&'p Prop>,
        footprint: BTreeSet<PredicateName>,
        uses_pre: bool,
    },
    /// No positive-polarity implication, even through definitions:
    /// holds over every committed state by construction. "Always on"
    /// means *not measurable by antecedent firing* - its enforcement
    /// work is invisible in committed history.
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

/// Refusal stats accumulated from the rejection log - shared by
/// declared invariants and rejection-log-only names.
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
    declared_transformations: Vec<String>,
    usage: BTreeMap<String, Usage>,
    /// Refusals attributed to invariant names the current programme
    /// does not declare - drift, surfaced rather than dropped.
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
        let entries = program
            .invariants
            .iter()
            .map(|inv| {
                let mut implications = Vec::new();
                collect_implications(
                    &inv.body,
                    true,
                    DefinitionIndex::new(&program.definitions),
                    &mut BTreeSet::new(),
                    &mut implications,
                );
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
                    let uses_pre = antecedents.iter().any(|a| mentions_pre(a));
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

    /// Record one rejection-log row: pure counting, no state
    /// evaluation. `invariant` is the refusing invariant's name when
    /// the rejection's kind is `invariant`, `None` for the gate kinds
    /// (`require`/`bind`) - gates belong to their transformation, so
    /// a gate refusal counts only there. An invariant refusal counts
    /// for the invariant AND the transformation: it is still a
    /// refused proposal of that transformation, whichever rule said
    /// no. Invariant names the current programme does not declare
    /// accumulate separately and surface flagged.
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
            if let Some(entry) = self.entries.iter_mut().find(|e| e.name == name) {
                entry.refusals.record(rejection_id);
            } else {
                self.unmatched_refusals
                    .entry(name.to_string())
                    .or_default()
                    .record(rejection_id);
            }
        }
    }

    /// True when this transition needs a state snapshot at all:
    /// `delta` touches a tracked antecedent's footprint, or some
    /// antecedent reads pre-state (those are never pruned). A
    /// transition that is not relevant still counts (transitions,
    /// usage) but evaluates nothing, so the driver may pass any
    /// state.
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

    /// True when any tracked antecedent contains `pre(...)` - the
    /// driver's cue that it must carry the previous state forward on
    /// every step. When false, the pre-state argument is never read
    /// and the driver can skip the bookkeeping entirely.
    pub fn needs_pre_state(&self) -> bool {
        self.entries.iter().any(|entry| match &entry.shape {
            Shape::Implication { uses_pre, .. } => *uses_pre,
            Shape::AlwaysOn => false,
        })
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
    /// Antecedents that read pre-state are exempt from the prune:
    /// their firing opportunity lags the delta by one transition.
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

    /// Finish: verdicts from the accumulated stats (a refusal beats
    /// everything - `constrained` is the strongest verdict for any
    /// shape, including always-on), invariants in declaration order
    /// with rejection-log-only names after, transformations in
    /// declaration order first, historical-only names after.
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
        // Refusals attributed to invariant names the programme no
        // longer declares - drift, surfaced rather than dropped.
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
        // Whatever remains was seen in history but is not declared
        // today - vocabulary drift, surfaced rather than dropped.
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

/// Does this proposition contain `pre(...)`? Definitions cannot (a
/// validated programme bans `pre` inside bodies), so the walk does
/// not descend through calls.
fn mentions_pre(prop: &Prop) -> bool {
    match prop {
        Prop::Pre(_) => true,
        Prop::Claim { .. }
        | Prop::Defined { .. }
        | Prop::In(_, _)
        | Prop::Eq(_, _)
        | Prop::Neq(_, _)
        | Prop::Compare { .. } => false,
        Prop::And(props) | Prop::Or(props) => props.iter().any(mentions_pre),
        Prop::Xor(left, right) | Prop::Implies { left, right } => {
            mentions_pre(left) || mentions_pre(right)
        }
        Prop::Not(inner) => mentions_pre(inner),
        Prop::Exists { body, .. } => mentions_pre(body),
        Prop::Forall { source, body, .. } => mentions_pre(source) || mentions_pre(body),
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
            // Proposed but only ever refused - "0 transition(s)" would
            // read as never-proposed, which is the opposite of true.
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
