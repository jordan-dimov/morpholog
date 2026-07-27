//! Integration tests for the bilateral settlement netting example
//! (`examples/01_settlement_netting/`).
//!
//! Covers IR-shape tests (the example's invariants and
//! transformation look like what we expect), evaluator tests (the
//! `net_amount_equals_lines` invariant holds when arithmetic checks
//! out and fails when it doesn't), and full-chain `propose()` tests
//! (well-formed netting commits; pre-state `Netted` violation is
//! rejected at admission time; candidate-state double-netting is
//! caught by the invariant on the post-state).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::OnceLock;

use common::{Example, claim_instance, dec, subj};
use morpholog_core::{
    ClaimInstance, EvalValue, Outcome, Prop, State, Stmt, Subject, ValueExpr, eval_invariant,
};
use morpholog_examples::settlement_netting;

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&settlement_netting::program()))
}

// ============================================================
// IR shape - the example's invariants and transformation look
// like what we expect at the IR level.
// ============================================================

#[test]
fn invariant_round_trips_through_equality() {
    assert_eq!(
        settlement_netting::net_settlement_has_lines(),
        settlement_netting::net_settlement_has_lines()
    );
}

#[test]
fn invariant_has_expected_top_level_shape() {
    let inv = settlement_netting::net_settlement_has_lines();
    assert_eq!(inv.name, "net_settlement_has_lines");
    assert_eq!(inv.version, 1);
    assert!(matches!(inv.body, Prop::Implies { .. }));
}

#[test]
fn no_double_netting_round_trips() {
    assert_eq!(
        settlement_netting::no_double_netting(),
        settlement_netting::no_double_netting()
    );
}

#[test]
fn no_double_netting_has_expected_shape() {
    let inv = settlement_netting::no_double_netting();
    assert_eq!(inv.name, "no_double_netting");
    assert_eq!(inv.version, 1);
    let Prop::Implies { left, right } = inv.body else {
        panic!("body should be Implies");
    };
    assert!(matches!(*left, Prop::Claim { .. }));
    assert!(matches!(*right, Prop::Not(_)));
}

#[test]
fn net_amount_equals_lines_round_trips() {
    assert_eq!(
        settlement_netting::net_amount_equals_lines(),
        settlement_netting::net_amount_equals_lines()
    );
}

#[test]
fn net_amount_equals_lines_has_expected_shape() {
    let inv = settlement_netting::net_amount_equals_lines();
    assert_eq!(inv.name, "net_amount_equals_lines");
    let Prop::Implies { left, right } = inv.body else {
        panic!("body should be Implies");
    };
    assert!(matches!(*left, Prop::Claim { .. }));
    let Prop::Eq(lhs, rhs) = *right else {
        panic!("right should be Eq");
    };
    assert!(matches!(*lhs, ValueExpr::Term(_)));
    assert!(matches!(*rhs, ValueExpr::Sum { .. }));
}

#[test]
fn create_net_settlement_round_trips() {
    assert_eq!(
        settlement_netting::create_net_settlement(),
        settlement_netting::create_net_settlement()
    );
}

#[test]
fn create_net_settlement_has_expected_shape() {
    let t = settlement_netting::create_net_settlement();
    assert_eq!(t.name, "create_net_settlement");
    assert_eq!(t.parameters, vec!["party_a", "party_b", "lines"]);
    assert_eq!(t.body.len(), 6);
    assert!(matches!(t.body[0], Stmt::Require { .. }));
    assert!(matches!(t.body[1], Stmt::LetNewSubject { .. }));
    assert!(matches!(t.body[2], Stmt::Let { .. }));
    assert!(matches!(t.body[3], Stmt::Assert(_)));
    assert!(matches!(t.body[4], Stmt::For { .. }));
    assert!(matches!(t.body[5], Stmt::Emit(_)));
}

#[test]
fn for_body_contains_bind_one_and_two_asserts() {
    let t = settlement_netting::create_net_settlement();
    let Stmt::For { body, .. } = &t.body[4] else {
        panic!("body[4] should be Stmt::For");
    };
    assert_eq!(body.len(), 3);
    assert!(matches!(body[0], Stmt::BindOne { .. }));
    assert!(matches!(body[1], Stmt::Assert(_)));
    assert!(matches!(body[2], Stmt::Assert(_)));
}

// ============================================================
// Evaluator - `net_amount_equals_lines` against curated state.
// ============================================================

fn netting_state(amount: i64) -> State {
    State::from_claims(vec![
        claim_instance(
            "NetSettlement",
            &[subj("net1"), subj("party_a"), subj("party_b"), dec(amount)],
        ),
        claim_instance("SettlementLine", &[subj("l1"), subj("net1"), dec(60)]),
        claim_instance("SettlementLine", &[subj("l2"), subj("net1"), dec(40)]),
    ])
}

#[test]
fn net_amount_equals_lines_holds_when_amount_matches() {
    let state = netting_state(100);
    let inv = settlement_netting::net_amount_equals_lines();
    let result = eval_invariant(&inv, &state, None, &[]).expect("evaluation should not error");
    assert!(result, "invariant should hold for amount = 60 + 40 = 100");
}

#[test]
fn net_amount_equals_lines_fails_when_amount_mismatches() {
    let state = netting_state(101);
    let inv = settlement_netting::net_amount_equals_lines();
    let result = eval_invariant(&inv, &state, None, &[]).expect("evaluation should not error");
    assert!(
        !result,
        "invariant should fail for amount = 101 vs lines = 100"
    );
}

// ============================================================
// Chain - `propose(create_net_settlement, ...)`.
// ============================================================

/// Build a pre-state with l1 (60) and l2 (40), both approved, between
/// party_a and party_b, neither netted. `extra` lets a test add extra
/// claims (e.g. a pre-existing `SettlementLine` to provoke an invariant
/// violation).
fn netting_pre_state(extra: Vec<ClaimInstance>) -> State {
    let mut claims = vec![
        claim_instance("ApprovedSettlementLine", &[subj("l1")]),
        claim_instance("Between", &[subj("l1"), subj("party_a"), subj("party_b")]),
        claim_instance("LineAmount", &[subj("l1"), dec(60)]),
        claim_instance("ApprovedSettlementLine", &[subj("l2")]),
        claim_instance("Between", &[subj("l2"), subj("party_a"), subj("party_b")]),
        claim_instance("LineAmount", &[subj("l2"), dec(40)]),
    ];
    claims.extend(extra);
    State::from_claims(claims)
}

fn netting_args() -> Vec<EvalValue> {
    vec![
        subj("party_a"),
        subj("party_b"),
        EvalValue::Collection(vec![subj("l1"), subj("l2")]),
    ]
}

#[test]
fn propose_accepts_well_formed_netting() {
    let pre = netting_pre_state(vec![]);
    let t = settlement_netting::create_net_settlement();
    let outcome = ex()
        .propose(&t, netting_args(), &pre)
        .expect("propose should not error");

    let Outcome::Accepted {
        asserted_claims,
        retracted_claims,
        emitted_intents,
        ..
    } = outcome
    else {
        panic!("expected Accepted, got {outcome:?}");
    };

    // Five asserts: NetSettlement + (SettlementLine + Netted) * 2
    assert_eq!(asserted_claims.len(), 5);
    assert_eq!(retracted_claims.len(), 0);
    assert_eq!(emitted_intents.len(), 1);
    assert_eq!(emitted_intents[0].name, "NetSettlementCreated");

    // Exactly one NetSettlement assertion with the expected total.
    let net_settlement = asserted_claims
        .iter()
        .find(|f| f.predicate.as_str() == "NetSettlement")
        .expect("should have asserted a NetSettlement");
    assert_eq!(net_settlement.args[3], dec(100));
}

#[test]
fn propose_rejects_when_line_already_netted() {
    // PR-G migration: the create_net_settlement transformation's
    // single require is `forall(line in lines): and(approved, between,
    // not(Netted(line)))`. With Netted(l1) admitted, the forall fails
    // at the l1 iteration, and the failure-walk drills past forall and
    // through the inner And to the failing `not(Netted(line))`
    // conjunct. The old test could only prove "some require failed";
    // this version pins which sub-expression rejected.
    use morpholog_core::{
        RequireOutcome, TraceEntry, TracedProposal, Transition, propose_with_trace,
    };
    let extra = vec![claim_instance("Netted", &[subj("l1")])];
    let pre = netting_pre_state(extra);
    let t = settlement_netting::create_net_settlement();
    let transition = Transition {
        transformation_name: t.name.clone(),
        args: netting_args(),
        actor: Subject::from("test_actor"),
    };
    let TracedProposal::Completed { outcome, trace } = propose_with_trace(
        &t,
        &transition,
        &pre,
        &settlement_netting::all_invariants(),
        &settlement_netting::definitions(),
    ) else {
        panic!("expected Completed");
    };
    assert!(
        matches!(outcome, Outcome::Rejected { .. }),
        "expected Rejected, got {outcome:?}"
    );
    let failing = trace.iter().find_map(|e| match e {
        TraceEntry::Require {
            outcome:
                RequireOutcome::Rejected {
                    failing_sub_expression,
                    ..
                },
            ..
        } => failing_sub_expression.as_deref(),
        _ => None,
    });
    let failing = failing.expect("expected failing_sub_expression on the require");
    // The failure-walk should drill past forall + the inner And to
    // the negated Netted clause. Pinning "Netted" alone is enough -
    // the broader And contains other conjuncts (ApprovedSettlementLine,
    // Between) and asserting on Netted specifically rules out them.
    assert!(
        failing.contains("Netted"),
        "expected drill-down to the negated Netted clause; got: {failing}"
    );
    assert!(
        !failing.starts_with("forall"),
        "expected drill past the forall wrapper; got: {failing}"
    );
}

#[test]
fn propose_rejects_when_candidate_state_violates_no_double_netting() {
    // l1 already participates in an older settlement, but Netted(l1)
    // is missing from pre-state (inconsistent legacy data). The require
    // check passes, the transformation stages a second SettlementLine
    // for l1, and the invariant catches it on the candidate state.
    let extra = vec![claim_instance(
        "SettlementLine",
        &[subj("l1"), subj("old_net"), dec(60)],
    )];
    let pre = netting_pre_state(extra);
    let t = settlement_netting::create_net_settlement();
    let reason = ex().must_reject(&t, netting_args(), &pre);
    assert!(
        reason.to_string().contains("no_double_netting"),
        "got: {reason}"
    );
}

// Pins the propose() guard that a Transition's transformation_name must
// match the Transformation it is being evaluated against. Without this,
// a misuse where the caller passes a transformation whose `name`
// disagrees with the audit-recorded `transformation_name` could commit
// with a misleading audit row. The guard surfaces as EvalError so the
// adapter rolls back rather than committing inconsistent state.
#[test]
fn propose_rejects_transition_name_mismatch() {
    use morpholog_core::{EvalError, Transition, propose};

    let t = settlement_netting::create_net_settlement();
    let pre = netting_pre_state(vec![]);
    let transition = Transition {
        transformation_name: "some_other_name".into(),
        args: netting_args(),
        actor: common::test_actor(),
    };

    let err = propose(
        &t,
        &transition,
        &pre,
        &settlement_netting::all_invariants(),
        &settlement_netting::definitions(),
    )
    .expect_err("name mismatch should be an EvalError, not Rejected");

    match err {
        EvalError::TypeMismatch(msg) => {
            assert!(
                msg.contains("some_other_name") && msg.contains(t.name.as_str()),
                "error message should name both sides: got `{msg}`"
            );
        }
        other => panic!("expected TypeMismatch, got {other:?}"),
    }
}
