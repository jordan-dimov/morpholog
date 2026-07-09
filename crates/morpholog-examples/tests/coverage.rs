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

// A pre-read nested inside a comparison operand is still a pre-read:
// the tracker must carry pre-state and exempt the antecedent from
// delta pruning, exactly as it does for a prop-level pre(...). The
// comparison direction makes the fire depend on the real previous
// state (an empty pre-state sums to 0 and fails it), and the firing
// transition's delta misses the footprint (only the prune exemption
// reaches it).
#[test]
fn pre_nested_in_a_comparison_operand_still_cues_pre_state() {
    let source = r#"
program pre_in_compare

predicate Count(slot: Subject, n: Decimal)
predicate Audit(slot: Subject)

invariant growth_is_audited:
    (Count(s, n) and (n <= sum(1 | pre(Count(s, _))))) implies Audit(s)

transformation tick(slot, n):
    admit Count(slot, n)

transformation audit(slot):
    admit Audit(slot)
"#;
    let program = parse_program(source).expect("parses");
    program.validate().expect("validates");
    let mut tracker = CoverageTracker::new(&program);
    assert!(tracker.needs_pre_state());

    let count = ClaimInstance {
        predicate: "Count".into(),
        args: vec![
            EvalValue::Subject(Subject::from("s1")),
            EvalValue::Decimal("1".parse().unwrap()),
        ],
    };
    let audit = subject_claim("Audit", &["s1"]);
    let empty = State::from_claims(vec![]);
    let s1 = State::from_claims(vec![count.clone()]);
    let s2 = State::from_claims(vec![count, audit]);

    // Empty pre-state: the sum is 0, the comparison fails, no fire.
    tracker
        .observe(&s1, &empty, &delta(&["Count"]), "t1", "tick")
        .unwrap();
    // The audit step touches nothing in the antecedent's footprint;
    // only the never-pruned exemption evaluates it, and it fires only
    // because the carried pre-state holds the Count.
    tracker
        .observe(&s2, &s1, &delta(&["Audit"]), "t2", "audit")
        .unwrap();

    let report = tracker.into_report();
    let inv = report
        .invariants
        .iter()
        .find(|i| i.invariant == "growth_is_audited")
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
    let mut tracker = CoverageTracker::new(&program);
    // The refusal goes to an implication-shaped rule so every verdict
    // stays represented: ghosts_never_fire stays NEVER FIRED and the
    // prohibition stays always on.
    tracker.observe_rejection(Some("flagged_accounts_exist"), "flag_account", "r1");
    let report = tracker.into_report();
    let prose = morpholog_core::render_coverage(&report);
    assert!(prose.contains("NEVER FIRED"));
    assert!(prose.contains("always on"));
    assert!(prose.contains("never used"));
    assert!(prose.contains("CONSTRAINED"));
    assert!(prose.contains("refused: 1 proposal(s)"));
    assert!(
        prose.contains("a floor, not a census"),
        "the legend states the rejection log's at-most-once bound"
    );
}

#[test]
fn a_refusal_beats_fired_and_records_first_and_last_ids() {
    let program = parsed();
    let empty = State::from_claims(Vec::new());
    let mut tracker = CoverageTracker::new(&program);
    let s1 = State::from_claims(vec![
        subject_claim("Account", &["a1"]),
        subject_claim("Flag", &["a1"]),
    ]);
    tracker
        .observe(
            &s1,
            &empty,
            &delta(&["Account", "Flag"]),
            "t1",
            "flag_account",
        )
        .unwrap();
    tracker.observe_rejection(Some("flagged_accounts_exist"), "flag_account", "r1");
    tracker.observe_rejection(Some("flagged_accounts_exist"), "flag_account", "r2");

    let report = tracker.into_report();
    let inv = report
        .invariants
        .iter()
        .find(|i| i.invariant == "flagged_accounts_exist")
        .unwrap();
    assert_eq!(inv.verdict, CoverageVerdict::Constrained);
    assert_eq!(inv.transitions_fired, 1, "firing stats survive the upgrade");
    assert_eq!(inv.proposals_refused, 2);
    assert_eq!(inv.first_refused.as_deref(), Some("r1"));
    assert_eq!(inv.last_refused.as_deref(), Some("r2"));
}

// THE headline payoff: an always-on prohibition's enforcement work is
// invisible in committed history, but the rejection log shows it
// refusing - the first time such a rule becomes measurable at all.
#[test]
fn an_always_on_prohibition_with_a_refusal_is_constrained() {
    let program = parsed();
    let mut tracker = CoverageTracker::new(&program);
    tracker.observe_rejection(Some("no_flag_without_account_ever"), "flag_account", "r1");
    let report = tracker.into_report();
    let inv = report
        .invariants
        .iter()
        .find(|i| i.invariant == "no_flag_without_account_ever")
        .unwrap();
    assert_eq!(inv.verdict, CoverageVerdict::Constrained);
    assert_eq!(inv.proposals_refused, 1);
    assert_eq!(report.rejections_replayed, 1);
}

#[test]
fn a_gate_refusal_counts_for_the_transformation_not_any_invariant() {
    let program = parsed();
    let mut tracker = CoverageTracker::new(&program);
    tracker.observe_rejection(None, "flag_account", "r1");
    let report = tracker.into_report();
    assert!(
        report
            .invariants
            .iter()
            .all(|i| i.verdict != CoverageVerdict::Constrained && i.proposals_refused == 0),
        "a require/bind refusal belongs to its transformation, not a rule"
    );
    let usage = report
        .transformations
        .iter()
        .find(|t| t.transformation == "flag_account")
        .unwrap();
    assert_eq!(usage.proposals_refused, 1);
    assert_eq!(usage.transitions, 0);
    assert!(!usage.not_in_programme);
}

#[test]
fn refusals_naming_undeclared_rules_and_transformations_surface_flagged() {
    let program = parsed();
    let mut tracker = CoverageTracker::new(&program);
    tracker.observe_rejection(Some("retired_rule"), "renamed_long_ago", "r1");
    let report = tracker.into_report();

    let drifted = report
        .invariants
        .iter()
        .find(|i| i.invariant == "retired_rule")
        .expect("a rejection-log-only rule name appears, flagged");
    assert_eq!(drifted.verdict, CoverageVerdict::Constrained);
    assert!(drifted.not_in_programme);
    assert_eq!(drifted.proposals_refused, 1);

    let usage = report
        .transformations
        .iter()
        .find(|t| t.transformation == "renamed_long_ago")
        .expect("the undeclared transformation appears");
    assert!(usage.not_in_programme);
    assert_eq!(usage.proposals_refused, 1);
}

// REGRESSION (review catch): a pre(...) antecedent's firing
// opportunity lags the delta by one transition - the claim asserted
// at T sits in the PRE-state only from T+1. The prune must therefore
// never skip a pre-reading invariant, even when the current delta
// misses its footprint entirely.
#[test]
fn pre_antecedent_fires_on_a_transition_whose_delta_misses_its_footprint() {
    let source = r#"
program pre_lag

predicate Count(slot: Subject)
predicate Marker(slot: Subject)

invariant previous_count_requires_marker:
    pre(Count(s)) implies Marker(s)

transformation start(slot):
    admit Count(slot)

transformation mark(slot):
    admit Marker(slot)
"#;
    let program = parse_program(source).expect("parses");
    program.validate().expect("validates");
    let mut tracker = CoverageTracker::new(&program);

    let empty = State::from_claims(vec![]);
    let s1 = State::from_claims(vec![subject_claim("Count", &["s1"])]);
    let s2 = State::from_claims(vec![
        subject_claim("Count", &["s1"]),
        subject_claim("Marker", &["s1"]),
    ]);

    // t1 asserts Count; the PRE-state is empty, nothing binds.
    tracker
        .observe(&s1, &empty, &delta(&["Count"]), "t1", "start")
        .unwrap();
    // t2's delta is ONLY Marker - outside the antecedent's {Count}
    // footprint - yet the previous state now holds Count, so this is
    // exactly the transition where the antecedent first binds.
    tracker
        .observe(&s2, &s1, &delta(&["Marker"]), "t2", "mark")
        .unwrap();

    let report = tracker.into_report();
    let inv = report
        .invariants
        .iter()
        .find(|i| i.invariant == "previous_count_requires_marker")
        .unwrap();
    assert_eq!(inv.verdict, CoverageVerdict::Fired);
    assert_eq!(inv.transitions_fired, 1);
    assert_eq!(
        inv.first_fired.as_deref(),
        Some("t2"),
        "the pre-lagged firing lands on the delta-mismatched transition"
    );
}

// REGRESSION (review catch): an implication hidden behind a `define`
// call is still an implication - the every-walker-transitive red
// line. It must classify (and fire) as implication-shaped, never
// always-on.
#[test]
fn an_implication_inside_a_define_is_implication_shaped_and_fires() {
    let source = r#"
program defined_implication

predicate Account(account_id: Subject)
predicate Flag(account_id: Subject)
predicate Audited(account_id: Subject)

define audited_when_flagged(a):
    Account(a) and (Flag(a) implies Audited(a))

invariant accounts_audited_when_flagged:
    audited_when_flagged(a)
"#;
    let program = parse_program(source).expect("parses");
    program.validate().expect("validates");
    let mut tracker = CoverageTracker::new(&program);

    // Zero history: the verdict must be never-fired, NOT always-on -
    // the implication is visible through the call.
    let report = CoverageTracker::new(&program).into_report();
    assert_eq!(
        report.invariants[0].verdict,
        CoverageVerdict::NeverFired,
        "the hidden implication must classify as implication-shaped"
    );

    // And it fires when the hidden antecedent (Flag) binds.
    let empty = State::from_claims(vec![]);
    let state = State::from_claims(vec![
        subject_claim("Account", &["a1"]),
        subject_claim("Flag", &["a1"]),
        subject_claim("Audited", &["a1"]),
    ]);
    tracker
        .observe(&state, &empty, &delta(&["Flag"]), "t1", "ignored")
        .unwrap();
    let report = tracker.into_report();
    assert_eq!(report.invariants[0].verdict, CoverageVerdict::Fired);
    assert_eq!(report.invariants[0].transitions_fired, 1);
}

// REGRESSION (review catch): the definition-descent guard is a
// recursion STACK, not a visited set - polarity is part of the
// meaning. The same define called at negative polarity FIRST and
// positive polarity second must still surface its implication on the
// positive call; a visited-set would mark it seen on the negative
// pass and silently skip the positive one.
#[test]
fn a_define_seen_at_negative_polarity_first_still_classifies_on_the_positive_call() {
    let source = r#"
program polarity_revisit

predicate Account(account_id: Subject)
predicate Flag(account_id: Subject)
predicate Audited(account_id: Subject)

define audited_when_flagged(a):
    Account(a) and (Flag(a) implies Audited(a))

invariant tautological_guard:
    not audited_when_flagged(x) or audited_when_flagged(x)
"#;
    let program = parse_program(source).expect("parses");
    program.validate().expect("validates");
    let report = CoverageTracker::new(&program).into_report();
    assert_eq!(
        report.invariants[0].verdict,
        CoverageVerdict::NeverFired,
        "the positive call's implication must be collected even though the \
         negative call walked the definition first"
    );
}

// REGRESSION (review catch): an antecedent extracted from a
// definition body must be evaluated under the CALL's constraints. A
// literal argument pins the matching parameter; a claim that matches
// the raw body but not the call must not count as firing.
#[test]
fn a_literal_constrained_define_call_does_not_fire_on_unrelated_claims() {
    let source = r#"
program defcall_precision

predicate RiskLevel(case_id: Subject, level: Decimal)
predicate Reviewed(case_id: Subject)

define reviewed_at(level):
    RiskLevel(c, level) implies Reviewed(c)

invariant high_risk_is_reviewed:
    reviewed_at(3)

transformation rate(case_id, level):
    admit RiskLevel(case_id, level)
"#;
    let program = parse_program(source).expect("parses");
    program.validate().expect("validates");

    let risk = |case: &str, level: &str| ClaimInstance {
        predicate: "RiskLevel".into(),
        args: vec![
            EvalValue::Subject(Subject::from(case)),
            EvalValue::Decimal(level.parse().unwrap()),
        ],
    };
    let empty = State::from_claims(vec![]);

    // A level-2 rating matches the definition body's raw antecedent
    // (RiskLevel(c, level) with level free) but NOT the call
    // reviewed_at(3) - the invariant has nothing at stake yet.
    let mut tracker = CoverageTracker::new(&program);
    let s1 = State::from_claims(vec![risk("case_1", "2")]);
    tracker
        .observe(&s1, &empty, &delta(&["RiskLevel"]), "t1", "rate")
        .unwrap();
    let report = tracker.into_report();
    let inv = report
        .invariants
        .iter()
        .find(|i| i.invariant == "high_risk_is_reviewed")
        .unwrap();
    assert_eq!(
        inv.verdict,
        CoverageVerdict::NeverFired,
        "a claim outside the call's constraint must not fire the rule"
    );

    // A level-3 rating is what the call asks about: fired.
    let mut tracker = CoverageTracker::new(&program);
    let s1 = State::from_claims(vec![risk("case_1", "3")]);
    tracker
        .observe(&s1, &empty, &delta(&["RiskLevel"]), "t1", "rate")
        .unwrap();
    let report = tracker.into_report();
    let inv = report
        .invariants
        .iter()
        .find(|i| i.invariant == "high_risk_is_reviewed")
        .unwrap();
    assert_eq!(inv.verdict, CoverageVerdict::Fired);
    assert_eq!(inv.transitions_fired, 1);
}

// The call chain composes: an outer define forwards its (literal)
// argument to an inner define whose body carries the implication.
// The frames replay outermost-first, so the constraint survives the
// hop.
#[test]
fn a_nested_define_chain_carries_the_call_constraint_through() {
    let source = r#"
program defcall_nesting

predicate RiskLevel(case_id: Subject, level: Decimal)
predicate Reviewed(case_id: Subject)

define reviewed_at(level):
    RiskLevel(c, level) implies Reviewed(c)

define escalation_policy(threshold):
    reviewed_at(threshold)

invariant policy_holds:
    escalation_policy(5)

transformation rate(case_id, level):
    admit RiskLevel(case_id, level)
"#;
    let program = parse_program(source).expect("parses");
    program.validate().expect("validates");

    let risk = |case: &str, level: &str| ClaimInstance {
        predicate: "RiskLevel".into(),
        args: vec![
            EvalValue::Subject(Subject::from(case)),
            EvalValue::Decimal(level.parse().unwrap()),
        ],
    };
    let empty = State::from_claims(vec![]);

    let verdict_for = |level: &str| {
        let mut tracker = CoverageTracker::new(&program);
        let s1 = State::from_claims(vec![risk("case_1", level)]);
        tracker
            .observe(&s1, &empty, &delta(&["RiskLevel"]), "t1", "rate")
            .unwrap();
        let report = tracker.into_report();
        report
            .invariants
            .iter()
            .find(|i| i.invariant == "policy_holds")
            .unwrap()
            .verdict
    };
    assert_eq!(verdict_for("4"), CoverageVerdict::NeverFired);
    assert_eq!(verdict_for("5"), CoverageVerdict::Fired);
}
