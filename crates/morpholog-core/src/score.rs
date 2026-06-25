//! Scoring a candidate programme against committed history - the
//! evaluator pointed backward.
//!
//! `coverage` replays the *committed* programme, whose invariants held on
//! every transition (they gated each commit), so its signal is "did the
//! antecedent ever fire". A *candidate* programme was never enforced, so
//! its invariants CAN be violated by committed history - and that is the
//! whole point of scoring it. This module asks, per candidate invariant:
//! which already-admitted commits would it have refused?
//!
//! **Fresh-violation semantics.** A transition "would be refused" iff the
//! invariant's post-state violates it AND its pre-state held - the commit
//! *introduced* a fresh violation. This is the commit-gate counterfactual:
//! a rule is not charged for a violation it inherited from earlier state
//! (the case-bound, inconsistency-tolerant rule the runtime already obeys).
//! A bad subject's claim would otherwise violate a `forall` at every later
//! transition; flagging only the introducing one is what correlates with
//! the bad record.
//!
//! **v1 scores state invariants only.** The fresh-violation check carries
//! "held entering the next transition" forward as the previous transition's
//! post-state result - valid only when the invariant is a state predicate.
//! An invariant using `pre(...)` is transition-relational (it compares two
//! states), so "held on the prior state" is not the same proposition, and
//! the carry would manufacture a false signal. Such candidates are rejected
//! up front; scoring them is deferred to a distinct transition-relational
//! semantics.

use serde::Serialize;

use crate::coverage::mentions_pre;
use crate::derive::eval_invariant;
use crate::eval::EvalError;
use crate::format::canonical_hash;
use crate::ir::{Definition, Invariant, Program};
use crate::state::State;

/// Bumped when the report shape or scoring semantics change, so a stored
/// experiment result is never silently misread.
pub const SCORE_FORMAT_VERSION: u32 = 1;
/// Names the exact scoring rule, so the report is self-describing.
pub const SCORE_SEMANTICS: &str = "fresh_state_violation_v1";

/// A candidate the scorer cannot evaluate under v1 semantics.
#[derive(Debug, thiserror::Error)]
pub enum ScoreError {
    /// One or more candidate invariants use `pre(...)` - transition-
    /// relational, not scorable under the state fresh-violation rule.
    #[error(
        "`evaluate` v1 scores state invariants only; these use pre(...) \
         (transition-relational, deferred): {}",
        .0.join(", ")
    )]
    PreUnsupported(Vec<String>),
    /// A kernel evaluation error while seeding the scorer.
    #[error(transparent)]
    Eval(#[from] EvalError),
}

/// The candidate invariants that use `pre(...)`, by name. Empty for a
/// purely state-predicate programme. Lets a caller reject early with a
/// clear message before opening a database connection.
pub fn invariants_using_pre(program: &Program) -> Vec<String> {
    program
        .invariants
        .iter()
        .filter(|inv| mentions_pre(&inv.body))
        .map(|inv| inv.name.to_string())
        .collect()
}

/// The fitness of a candidate programme over committed history. JSON is the
/// contract a scoring loop consumes; the version/semantics/hash header makes
/// a stored result reproducible and unambiguous.
#[derive(Debug, Clone, Serialize)]
pub struct CandidateScore {
    pub score_format_version: u32,
    pub semantics: String,
    pub program: String,
    /// Canonical rules-identity hash of the scored candidate, equal to
    /// `morpholog hash` - which exact candidate produced this result.
    pub program_hash: String,
    pub transitions_replayed: u64,
    pub invariants: Vec<InvariantScore>,
}

/// Per candidate invariant: the commits it would have refused. This is an
/// invariant-level counterfactual - the candidate's `require` gates and
/// transformations are not replayed, only whether each invariant holds.
#[derive(Debug, Clone, Serialize)]
pub struct InvariantScore {
    pub invariant: String,
    pub version: u32,
    /// Whether the invariant held on the empty initial state. A candidate
    /// that is `initially_holds: false` with `would_refuse: 0` was violated
    /// from the start and never recovered - distinct from a vacuously-held
    /// candidate, which a discovery loop would otherwise read alike.
    pub initially_holds: bool,
    pub would_refuse: u64,
    /// The `transition_id`s of the fresh violations, in replay order. The
    /// harness joins these against its own labels. Small for a good
    /// candidate; bounding it is a deferred throughput concern.
    pub refused_transitions: Vec<String>,
}

/// Accumulates fresh-violation counts as committed history is replayed
/// forward. The driver folds the audit log and calls [`observe`] with each
/// transition's post- and pre-state; the kernel evaluation lives here so it
/// is testable without a database.
///
/// [`observe`]: CandidateScorer::observe
pub struct CandidateScorer<'p> {
    program_name: String,
    program_hash: String,
    invariants: &'p [Invariant],
    definitions: &'p [Definition],
    /// Whether each invariant held on the empty initial state, reported as
    /// `initially_holds`.
    initially_held: Vec<bool>,
    /// Whether each invariant held entering the next transition. Seeded on
    /// the empty pre-state; carried forward so each transition costs one
    /// evaluation per invariant (valid because v1 rejects `pre(...)`, so
    /// every invariant is a state predicate, and the driver observes every
    /// transition contiguously).
    held: Vec<bool>,
    refused: Vec<Vec<String>>,
    transitions: u64,
}

impl<'p> CandidateScorer<'p> {
    pub fn new(program: &'p Program) -> Result<Self, ScoreError> {
        let pre = invariants_using_pre(program);
        if !pre.is_empty() {
            return Err(ScoreError::PreUnsupported(pre));
        }
        let empty = State::from_claims(Vec::new());
        let held = program
            .invariants
            .iter()
            .map(|inv| eval_invariant(inv, &empty, None, &program.definitions))
            .collect::<Result<Vec<_>, _>>()?;
        let refused = vec![Vec::new(); program.invariants.len()];
        Ok(Self {
            program_name: program.name.clone(),
            program_hash: canonical_hash(program),
            invariants: &program.invariants,
            definitions: &program.definitions,
            initially_held: held.clone(),
            held,
            refused,
            transitions: 0,
        })
    }

    /// Observe one replayed transition: `pre` is the state before it, `post`
    /// the state after. Records a fresh violation for any invariant that
    /// holds on `pre` but not on `post`.
    pub fn observe(
        &mut self,
        post: &State,
        pre: &State,
        transition_id: &str,
    ) -> Result<(), EvalError> {
        self.transitions += 1;
        for (i, inv) in self.invariants.iter().enumerate() {
            let post_holds = eval_invariant(inv, post, Some(pre), self.definitions)?;
            if !post_holds && self.held[i] {
                self.refused[i].push(transition_id.to_string());
            }
            self.held[i] = post_holds;
        }
        Ok(())
    }

    pub fn into_report(self) -> CandidateScore {
        let invariants = self
            .invariants
            .iter()
            .zip(self.initially_held)
            .zip(self.refused)
            .map(|((inv, initially_holds), refused)| InvariantScore {
                invariant: inv.name.to_string(),
                version: inv.version,
                initially_holds,
                would_refuse: refused.len() as u64,
                refused_transitions: refused,
            })
            .collect();
        CandidateScore {
            score_format_version: SCORE_FORMAT_VERSION,
            semantics: SCORE_SEMANTICS.to_string(),
            program: self.program_name,
            program_hash: self.program_hash,
            transitions_replayed: self.transitions,
            invariants,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir_builder::{claim, exists, invariant, not, pre, var};
    use crate::state::{ClaimInstance, EvalValue};
    use rust_decimal::Decimal;

    /// `NoFlag` holds while no `Flagged(_)` claim exists, and is violated
    /// the moment one does.
    fn no_flag_program() -> Program {
        let inv = invariant("NoFlag", not(exists("x", claim("Flagged", vec![var("x")]))));
        program_with(vec![inv])
    }

    fn program_with(invariants: Vec<Invariant>) -> Program {
        Program {
            name: "candidate".into(),
            predicates: vec![],
            intents: vec![],
            definitions: vec![],
            invariants,
            transformations: vec![],
            derived_claims: vec![],
        }
    }

    fn flagged() -> State {
        State::from_claims(vec![ClaimInstance {
            predicate: "Flagged".into(),
            args: vec![EvalValue::Decimal(Decimal::new(1, 0))],
        }])
    }

    fn empty() -> State {
        State::from_claims(Vec::new())
    }

    #[test]
    fn fresh_violation_counted_once_then_again_when_it_reappears() {
        let program = no_flag_program();
        let mut scorer = CandidateScorer::new(&program).unwrap();
        // holds -> fails -> fails -> holds -> fails: only the two
        // introducing transitions (t1, t4) count, never the inherited t2.
        scorer.observe(&flagged(), &empty(), "t1").unwrap();
        scorer.observe(&flagged(), &flagged(), "t2").unwrap();
        scorer.observe(&empty(), &flagged(), "t3").unwrap();
        scorer.observe(&flagged(), &empty(), "t4").unwrap();
        let report = scorer.into_report();

        assert_eq!(report.transitions_replayed, 4);
        let inv = &report.invariants[0];
        assert_eq!(inv.invariant, "NoFlag");
        assert_eq!(inv.would_refuse, 2);
        assert_eq!(inv.refused_transitions, vec!["t1", "t4"]);
    }

    #[test]
    fn a_candidate_that_always_holds_refuses_nothing() {
        let program = no_flag_program();
        let mut scorer = CandidateScorer::new(&program).unwrap();
        scorer.observe(&empty(), &empty(), "t1").unwrap();
        scorer.observe(&empty(), &empty(), "t2").unwrap();
        let report = scorer.into_report();
        assert_eq!(report.invariants[0].would_refuse, 0);
        assert!(report.invariants[0].refused_transitions.is_empty());
    }

    #[test]
    fn a_vacuous_candidate_over_absent_predicates_refuses_nothing() {
        let program = no_flag_program();
        let other = State::from_claims(vec![ClaimInstance {
            predicate: "Other".into(),
            args: vec![EvalValue::Decimal(Decimal::new(9, 0))],
        }]);
        let mut scorer = CandidateScorer::new(&program).unwrap();
        scorer.observe(&other, &empty(), "t1").unwrap();
        let report = scorer.into_report();
        assert_eq!(report.invariants[0].would_refuse, 0);
    }

    #[test]
    fn a_candidate_using_pre_is_rejected() {
        let inv = invariant(
            "UsesPre",
            pre(exists("x", claim("Flagged", vec![var("x")]))),
        );
        let program = program_with(vec![inv]);
        assert!(invariants_using_pre(&program).contains(&"UsesPre".to_string()));
        match CandidateScorer::new(&program) {
            Err(ScoreError::PreUnsupported(names)) => assert_eq!(names, vec!["UsesPre"]),
            Err(e) => panic!("expected PreUnsupported, got {e:?}"),
            Ok(_) => panic!("expected PreUnsupported, got a scorer"),
        }
    }

    #[test]
    fn initially_holds_distinguishes_violated_from_start_from_vacuous() {
        // `NeedsRequired` is violated on the empty state (nothing Required
        // exists); `NoFlag` holds vacuously. Both can show would_refuse 0,
        // so the flag is what tells them apart.
        let needs = invariant(
            "NeedsRequired",
            exists("x", claim("Required", vec![var("x")])),
        );
        let violated = CandidateScorer::new(&program_with(vec![needs]))
            .unwrap()
            .into_report();
        assert!(!violated.invariants[0].initially_holds);

        let vacuous = CandidateScorer::new(&no_flag_program())
            .unwrap()
            .into_report();
        assert!(vacuous.invariants[0].initially_holds);
    }

    #[test]
    fn report_carries_version_semantics_and_a_stable_hash() {
        let program = no_flag_program();
        let report = CandidateScorer::new(&program).unwrap().into_report();
        assert_eq!(report.score_format_version, SCORE_FORMAT_VERSION);
        assert_eq!(report.semantics, "fresh_state_violation_v1");
        assert!(report.program_hash.starts_with("sha256:"));
        // Stable: the same programme hashes identically.
        let again = CandidateScorer::new(&no_flag_program())
            .unwrap()
            .into_report();
        assert_eq!(report.program_hash, again.program_hash);
    }
}
