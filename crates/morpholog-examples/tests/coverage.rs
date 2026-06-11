//! Rule coverage: shape classification and the tracker's verdicts,
//! driven through parsed programmes and hand-built states - the same
//! ingredients the PG replay feeds it, without a database.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;

use morpholog_core::{
    ClaimInstance, CoverageTracker, CoverageVerdict, EvalValue, PredicateName, State, Subject,
};
use morpholog_surface::parse_program;

fn delta(predicates: &[&str]) -> BTreeSet<PredicateName> {
    predicates.iter().map(|p| PredicateName::from(*p)).collect()
}

fn subject_claim(predicate: &str, subjects: &[&str]) -> ClaimInstance {
    ClaimInstance {
        predicate: predicate.into(),
        args: subjects
            .iter()
            .map(|s| EvalValue::Subject(Subject::from(*s)))
            .collect(),
    }
}

const PROGRAM: &str = r#"
program coverage_demo

predicate Account(account_id: Subject)
predicate Flag(account_id: Subject)
predicate Ghost(ghost_id: Subject)

invariant flagged_accounts_exist:
    Flag(a) implies Account(a)

invariant ghosts_never_fire:
    Ghost(g) implies Account(g)

invariant no_flag_without_account_ever:
    not (Flag(a) and not Account(a))

transformation open_account(account_id):
    admit Account(account_id)

transformation flag_account(account_id):
    require Account(account_id)
    admit Flag(account_id)

transformation never_called(account_id):
    admit Account(account_id)
"#;

fn parsed() -> morpholog_core::Program {
    let program = parse_program(PROGRAM).expect("parses");
    program.validate().expect("validates");
    program
}

// With zero history, every implication is never-fired and every
// prohibition is always-on - the two shapes are never conflated.
#[test]
fn empty_history_distinguishes_never_fired_from_always_on() {
    let program = parsed();
    let report = CoverageTracker::new(&program).into_report();
    assert_eq!(report.transitions_replayed, 0);
    let verdict = |name: &str| {
        report
            .invariants
            .iter()
            .find(|i| i.invariant == name)
            .unwrap_or_else(|| panic!("{name} in report"))
            .verdict
    };
    assert_eq!(
        verdict("flagged_accounts_exist"),
        CoverageVerdict::NeverFired
    );
    assert_eq!(verdict("ghosts_never_fire"), CoverageVerdict::NeverFired);
    assert_eq!(
        verdict("no_flag_without_account_ever"),
        CoverageVerdict::AlwaysOn
    );
    // Declared transformations appear at zero even with no history.
    assert!(
        report
            .transformations
            .iter()
            .any(|t| t.transformation == "never_called" && t.transitions == 0)
    );
}

#[test]
fn firing_counts_first_and_last_accumulate_per_transition() {
    let program = parsed();
    let mut tracker = CoverageTracker::new(&program);
    let empty = State::from_claims(vec![]);

    // t1: an account opens. Flag's antecedent has nothing to bind.
    let s1 = State::from_claims(vec![subject_claim("Account", &["a1"])]);
    tracker
        .observe(&s1, &empty, &delta(&["Account"]), "t1", "open_account")
        .unwrap();
    // t2: the account is flagged - the invariant fires.
    let s2 = State::from_claims(vec![
        subject_claim("Account", &["a1"]),
        subject_claim("Flag", &["a1"]),
    ]);
    tracker
        .observe(&s2, &s1, &delta(&["Flag"]), "t2", "flag_account")
        .unwrap();
    // t3: another account opens; the Flag invariant's footprint does
    // not intersect the delta, so it is not re-counted.
    let s3 = State::from_claims(vec![
        subject_claim("Account", &["a1"]),
        subject_claim("Account", &["a2"]),
        subject_claim("Flag", &["a1"]),
    ]);
    tracker
        .observe(&s3, &s2, &delta(&["Account"]), "t3", "open_account")
        .unwrap();

    let report = tracker.into_report();
    assert_eq!(report.transitions_replayed, 3);
    let flagged = report
        .invariants
        .iter()
        .find(|i| i.invariant == "flagged_accounts_exist")
        .unwrap();
    assert_eq!(flagged.verdict, CoverageVerdict::Fired);
    assert_eq!(flagged.transitions_fired, 1);
    assert_eq!(flagged.first_fired.as_deref(), Some("t2"));
    assert_eq!(flagged.last_fired.as_deref(), Some("t2"));

    let ghosts = report
        .invariants
        .iter()
        .find(|i| i.invariant == "ghosts_never_fire")
        .unwrap();
    assert_eq!(ghosts.verdict, CoverageVerdict::NeverFired);

    let usage = |name: &str| {
        report
            .transformations
            .iter()
            .find(|t| t.transformation == name)
            .unwrap()
            .transitions
    };
    assert_eq!(usage("open_account"), 2);
    assert_eq!(usage("flag_account"), 1);
    assert_eq!(usage("never_called"), 0);
}

// The delta prune is load-bearing: a state that WOULD bind is not
// evaluated when the transition's delta does not touch the antecedent.
#[test]
fn delta_pruning_skips_untouched_invariants() {
    let program = parsed();
    let mut tracker = CoverageTracker::new(&program);
    let empty = State::from_claims(vec![]);
    let binding_state = State::from_claims(vec![
        subject_claim("Account", &["a1"]),
        subject_claim("Flag", &["a1"]),
    ]);
    tracker
        .observe(
            &binding_state,
            &empty,
            &delta(&["Ghost"]),
            "t1",
            "open_account",
        )
        .unwrap();
    let report = tracker.into_report();
    let flagged = report
        .invariants
        .iter()
        .find(|i| i.invariant == "flagged_accounts_exist")
        .unwrap();
    assert_eq!(
        flagged.verdict,
        CoverageVerdict::NeverFired,
        "the prune must skip an invariant whose footprint missed the delta"
    );
}

// A transformation seen in history but absent from today's programme
// is surfaced as drift, never silently dropped.
#[test]
fn historical_only_transformations_are_flagged() {
    let program = parsed();
    let mut tracker = CoverageTracker::new(&program);
    let empty = State::from_claims(vec![]);
    tracker
        .observe(&empty, &empty, &delta(&[]), "t1", "renamed_long_ago")
        .unwrap();
    let report = tracker.into_report();
    let drifted = report
        .transformations
        .iter()
        .find(|t| t.transformation == "renamed_long_ago")
        .unwrap();
    assert!(drifted.not_in_programme);
    assert_eq!(drifted.transitions, 1);
}

// A pre(...) antecedent evaluates against the previous state the
// replay supplies - the first transition gets the empty state, so a
// transition invariant never errors with PreStateUnavailable.
#[test]
fn pre_state_antecedents_fire_against_the_previous_state() {
    let source = r#"
program with_pre

predicate Count(slot: Subject, n: Decimal)

invariant count_never_resets:
    pre(Count(s, _)) implies Count(s, _)

transformation tick(slot, n):
    retract Count(slot, _)
    admit Count(slot, n)
"#;
    let program = parse_program(source).expect("parses");
    program.validate().expect("validates");
    let mut tracker = CoverageTracker::new(&program);

    let count = |n: &str| ClaimInstance {
        predicate: "Count".into(),
        args: vec![
            EvalValue::Subject(Subject::from("s1")),
            EvalValue::Decimal(n.parse().unwrap()),
        ],
    };
    let empty = State::from_claims(vec![]);
    let s1 = State::from_claims(vec![count("1")]);
    let s2 = State::from_claims(vec![count("2")]);

    // First transition: pre-state is empty, the antecedent has nothing
    // to bind - no fire, and crucially no error.
    tracker
        .observe(&s1, &empty, &delta(&["Count"]), "t1", "tick")
        .unwrap();
    // Second: the previous state holds a Count, the antecedent binds.
    tracker
        .observe(&s2, &s1, &delta(&["Count"]), "t2", "tick")
        .unwrap();

    let report = tracker.into_report();
    let inv = report
        .invariants
        .iter()
        .find(|i| i.invariant == "count_never_resets")
        .unwrap();
    assert_eq!(inv.verdict, CoverageVerdict::Fired);
    assert_eq!(inv.transitions_fired, 1);
    assert_eq!(inv.first_fired.as_deref(), Some("t2"));
}

// Generated discipline invariants are implication-shaped, carry their
// provenance, and participate in coverage like authored rules; the
// carbon example's prohibitions classify always-on.
#[test]
fn worked_example_shapes_classify_as_documented() {
    let carbon = parse_program(include_str!(
        "../../../examples/09_carbon_credit_provenance/carbon_credit_provenance.morph"
    ))
    .expect("carbon parses");
    let report = CoverageTracker::new(&carbon).into_report();
    let always_on: Vec<&str> = report
        .invariants
        .iter()
        .filter(|i| i.verdict == CoverageVerdict::AlwaysOn)
        .map(|i| i.invariant.as_str())
        .collect();
    assert!(
        !always_on.is_empty(),
        "carbon's prohibition rules classify always-on; got none"
    );

    let trade = parse_program(include_str!(
        "../../../examples/10_trade_lifecycle/trade_lifecycle.morph"
    ))
    .expect("trade parses");
    let report = CoverageTracker::new(&trade).into_report();
    for inv in &report.invariants {
        if let Some(from) = &inv.from {
            assert_ne!(
                inv.verdict,
                CoverageVerdict::AlwaysOn,
                "generated discipline invariant `{}` ({from}) must be implication-shaped",
                inv.invariant
            );
        }
    }
    assert!(
        report.invariants.iter().any(|i| i.from.is_some()),
        "the trade example carries generated invariants with provenance"
    );
}

#[test]
fn the_prose_render_carries_verdicts_and_the_legend() {
    let program = parsed();
    let report = CoverageTracker::new(&program).into_report();
    let prose = morpholog_core::render_coverage(&report);
    assert!(prose.contains("NEVER FIRED"));
    assert!(prose.contains("always on"));
    assert!(prose.contains("never used"));
    assert!(
        prose.contains("rejections never commit"),
        "the legend states what committed history cannot show"
    );
}
