//! Integration tests for the insurance-claim-settlement example
//! (`examples/05_insurance_claim_settlement/`).
//!
//! Covers policy and claim plumbing, the actor authority gate, the
//! cumulative aggregate-limit gate (`running + proposed <= aggregate`), the
//! invariants, the `PolicyLimitUsage` projection, headroom conservation,
//! and the per-claim deductible-and-limit layer.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::OnceLock;

use common::{Example, claim_instance, dec, dec_str, has_claim, subj};
use morpholog_core::{
    ClaimInstance, Outcome, RejectionReason, State, enumerate_derived, eval_invariant,
};
use morpholog_examples::insurance_claim_settlement;

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&insurance_claim_settlement::program()))
}

fn issue(state: State, policy_id: &str, aggregate_limit: i64) -> State {
    ex().must_accept(
        &insurance_claim_settlement::issue_policy(),
        vec![subj(policy_id), dec(aggregate_limit)],
        state,
    )
}

fn report(state: State, claim_id: &str, policy_id: &str, claimed_amount: i64) -> State {
    ex().must_accept(
        &insurance_claim_settlement::report_claim(),
        vec![subj(claim_id), subj(policy_id), dec(claimed_amount)],
        state,
    )
}

fn grant(state: State, actor: &str, limit: i64) -> State {
    ex().must_accept(
        &insurance_claim_settlement::grant_settlement_authority(),
        vec![subj(actor), dec(limit)],
        state,
    )
}

// ============================================================
// Policy and claim plumbing
// ============================================================

#[test]
fn issue_policy_admits_policy_claim_with_aggregate_limit() {
    let post = issue(State::default(), "policy_001", 100_000);
    assert!(has_claim(
        &post,
        "Policy",
        &[subj("policy_001"), dec(100_000)]
    ));
}

/// `issue_policy` also admits the remaining-capacity counter
/// `PolicyHeadroom(policy_id, aggregate_limit)`, starting at the full limit.
#[test]
fn issue_policy_admits_initial_headroom_equal_to_aggregate_limit() {
    let post = issue(State::default(), "policy_001", 100_000);
    assert!(
        has_claim(&post, "PolicyHeadroom", &[subj("policy_001"), dec(100_000)]),
        "issue_policy must admit initial PolicyHeadroom equal to aggregate_limit"
    );
}

#[test]
fn duplicate_policy_id_violates_uniqueness_invariant() {
    let pre = issue(State::default(), "policy_001", 100_000);
    let reason = ex().must_reject(
        &insurance_claim_settlement::issue_policy(),
        vec![subj("policy_001"), dec(50_000)],
        &pre,
    );
    assert!(
        reason.to_string().contains("policy_unique_by_policy_id"),
        "expected policy_unique_by_policy_id invariant violation, got: {reason}"
    );
}

#[test]
fn report_claim_without_policy_is_rejected_at_require() {
    let pre = State::default();
    let reason = ex().must_reject(
        &insurance_claim_settlement::report_claim(),
        vec![subj("claim_001"), subj("policy_001"), dec(20_000)],
        &pre,
    );
    assert!(
        reason.to_string().contains("require"),
        "got reason: {reason}"
    );
}

#[test]
fn report_claim_with_policy_admits_claim_reported() {
    let pre = issue(State::default(), "policy_001", 100_000);
    let post = report(pre, "claim_001", "policy_001", 20_000);
    assert!(has_claim(
        &post,
        "ClaimReported",
        &[subj("claim_001"), subj("policy_001"), dec(20_000)]
    ));
}

#[test]
fn duplicate_claim_id_violates_uniqueness_invariant() {
    let pre = issue(State::default(), "policy_001", 100_000);
    let pre = report(pre, "claim_001", "policy_001", 20_000);
    let reason = ex().must_reject(
        &insurance_claim_settlement::report_claim(),
        vec![subj("claim_001"), subj("policy_001"), dec(30_000)],
        &pre,
    );
    assert!(
        reason
            .to_string()
            .contains("claim_reported_unique_by_claim_id"),
        "expected claim_reported_unique_by_claim_id invariant violation, got: {reason}"
    );
}

// ============================================================
// Actor authority gate
// ============================================================

fn happy_pre() -> State {
    let s = issue(State::default(), "policy_001", 100_000);
    let s = report(s, "claim_001", "policy_001", 20_000);
    grant(s, "alex", 50_000)
}

#[test]
fn authorise_settlement_happy_path_admits_authorisation_and_payment() {
    let pre = happy_pre();
    let post = ex().must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_001"), subj("settlement_001"), dec(30_000)],
        "alex",
        pre,
    );
    // The authorisation records who decided.
    assert!(has_claim(
        &post,
        "SettlementAuthorised",
        &[
            subj("claim_001"),
            subj("settlement_001"),
            dec(30_000),
            subj("alex"),
        ]
    ));
    // The payment claim is what cumulative-cap reads from.
    assert!(has_claim(
        &post,
        "SettlementPaid",
        &[
            subj("policy_001"),
            subj("claim_001"),
            subj("settlement_001"),
            dec(30_000),
        ]
    ));
}

#[test]
fn authorise_settlement_without_authority_is_rejected_at_require() {
    // The trace shows which gate refused, not just that one did: all three
    // bind_ones succeeded, then the authority gate rejected. Prefer this to
    // `reason.contains(...)`.
    use morpholog_core::{
        BindOneOutcome, RequireOutcome, Subject, TraceEntry, TracedProposal, Transition,
        propose_with_trace,
    };
    let pre = {
        let s = issue(State::default(), "policy_001", 100_000);
        report(s, "claim_001", "policy_001", 20_000)
    };
    let t = insurance_claim_settlement::authorise_settlement();
    let transition = Transition {
        transformation_name: t.name.clone(),
        args: vec![subj("claim_001"), subj("settlement_001"), dec(30_000)],
        actor: Subject::from("alex"),
    };
    let TracedProposal::Completed { outcome, trace } = propose_with_trace(
        &t,
        &transition,
        &pre,
        &insurance_claim_settlement::all_invariants(),
        &insurance_claim_settlement::definitions(),
    ) else {
        panic!("expected Completed");
    };
    assert!(
        matches!(outcome, Outcome::Rejected { .. }),
        "expected Rejected, got {outcome:?}"
    );

    // Step 1: all three bind_ones succeeded.
    let bound_count = trace
        .iter()
        .filter(|e| {
            matches!(
                e,
                TraceEntry::BindOne {
                    outcome: BindOneOutcome::Bound { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(
        bound_count, 3,
        "expected all three bind_ones (ClaimReported, Policy, PolicyHeadroom) to succeed before the require fails; trace: {trace:#?}"
    );

    // Step 2: which gate rejected, by name. The name survives a rewording
    // of the condition; matching rendered text would not.
    let failing = trace.iter().find_map(|e| match e {
        TraceEntry::Require {
            name,
            outcome: RequireOutcome::Rejected { .. },
            ..
        } => Some(name.clone()),
        _ => None,
    });
    assert_eq!(
        failing.expect("expected exactly one failing require entry"),
        Some("actor_has_authority_for_amount".to_string()),
        "trace: {trace:#?}"
    );
}

#[test]
fn authorise_settlement_above_actor_limit_is_rejected_at_require() {
    let pre = happy_pre(); // alex has 50k limit
    let reason = ex().must_reject_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_001"), subj("settlement_001"), dec(60_000)],
        "alex",
        &pre,
    );
    // The gate name tells an authority refusal apart from the other ways
    // this transformation can refuse.
    assert!(
        matches!(
            &reason,
            RejectionReason::Require { name: Some(n), .. } if n == "actor_has_authority_for_amount"
        ),
        "got reason: {reason:?}"
    );
}

#[test]
fn authorise_settlement_at_actor_boundary_admits() {
    let pre = happy_pre(); // alex has 50k limit
    let post = ex().must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_001"), subj("settlement_001"), dec(50_000)],
        "alex",
        pre,
    );
    assert!(has_claim(
        &post,
        "SettlementPaid",
        &[
            subj("policy_001"),
            subj("claim_001"),
            subj("settlement_001"),
            dec(50_000),
        ]
    ));
}

// ============================================================
// Cumulative aggregate-limit gate
// ============================================================

/// Setup for cumulative-cap tests: authority high enough that only the
/// aggregate gate can refuse. Same policy and claim shape as `happy_pre`.
fn cap_pre() -> State {
    let s = issue(State::default(), "policy_001", 100_000);
    let s = report(s, "claim_001", "policy_001", 60_000);
    grant(s, "alex", 100_000)
}

fn after_first_settlement(amount: i64) -> State {
    let pre = cap_pre();
    ex().must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_001"), subj("settlement_001"), dec(amount)],
        "alex",
        pre,
    )
}

#[test]
fn second_settlement_under_remaining_aggregate_admits() {
    let pre = after_first_settlement(40_000); // 40k of 100k consumed
    let pre = report(pre, "claim_002", "policy_001", 30_000);
    let post = ex().must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_002"), subj("settlement_002"), dec(30_000)],
        "alex",
        pre,
    );
    assert!(has_claim(
        &post,
        "SettlementPaid",
        &[
            subj("policy_001"),
            subj("claim_002"),
            subj("settlement_002"),
            dec(30_000),
        ]
    ));
}

#[test]
fn second_settlement_at_aggregate_boundary_admits() {
    // 60 + 40 = 100 (exact fill).
    let pre = after_first_settlement(60_000);
    let pre = report(pre, "claim_002", "policy_001", 40_000);
    let post = ex().must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_002"), subj("settlement_002"), dec(40_000)],
        "alex",
        pre,
    );
    assert!(has_claim(
        &post,
        "SettlementPaid",
        &[
            subj("policy_001"),
            subj("claim_002"),
            subj("settlement_002"),
            dec(40_000),
        ]
    ));
}

#[test]
fn second_settlement_over_aggregate_is_rejected_at_require() {
    // 60 + 50 = 110 > 100. The trace shows the authority gate held (alex's
    // 100k limit covers 50k) and the aggregate gate rejected.
    use morpholog_core::{
        RequireOutcome, Subject, TraceEntry, TracedProposal, Transition, propose_with_trace,
    };
    let pre = after_first_settlement(60_000);
    let pre = report(pre, "claim_002", "policy_001", 50_000);
    let t = insurance_claim_settlement::authorise_settlement();
    let transition = Transition {
        transformation_name: t.name.clone(),
        args: vec![subj("claim_002"), subj("settlement_002"), dec(50_000)],
        actor: Subject::from("alex"),
    };
    let TracedProposal::Completed { outcome, trace } = propose_with_trace(
        &t,
        &transition,
        &pre,
        &insurance_claim_settlement::all_invariants(),
        &insurance_claim_settlement::definitions(),
    ) else {
        panic!("expected Completed");
    };
    assert!(
        matches!(outcome, Outcome::Rejected { .. }),
        "expected Rejected, got {outcome:?}"
    );

    let require_outcomes: Vec<(Option<&str>, &RequireOutcome)> = trace
        .iter()
        .filter_map(|e| match e {
            TraceEntry::Require { name, outcome, .. } => Some((name.as_deref(), outcome)),
            _ => None,
        })
        .collect();
    // Told apart by name, which survives renaming a variable inside a gate.
    let outcome_of = |rule: &str| {
        require_outcomes
            .iter()
            .find(|(name, _)| *name == Some(rule))
            .map(|(_, out)| *out)
    };
    assert!(
        matches!(
            outcome_of("actor_has_authority_for_amount"),
            Some(RequireOutcome::Held { .. })
        ),
        "expected the authority gate to hold; trace: {trace:#?}"
    );
    assert!(
        matches!(
            outcome_of("cumulative_spend_within_aggregate_cap"),
            Some(RequireOutcome::Rejected { .. })
        ),
        "expected the aggregate cap to reject; trace: {trace:#?}"
    );
}

#[test]
fn aggregate_limit_scoped_per_policy() {
    // policy_001 fully consumed at 100k; policy_002 still empty.
    let s = issue(State::default(), "policy_001", 100_000);
    let s = issue(s, "policy_002", 100_000);
    let s = report(s, "claim_001", "policy_001", 100_000);
    let s = grant(s, "alex", 100_000);
    let s = ex().must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_001"), subj("settlement_001"), dec(100_000)],
        "alex",
        s,
    );
    // policy_002 should still accept a fresh settlement up to its own limit.
    let s = report(s, "claim_002", "policy_002", 80_000);
    let post = ex().must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_002"), subj("settlement_002"), dec(80_000)],
        "alex",
        s,
    );
    assert!(has_claim(
        &post,
        "SettlementPaid",
        &[
            subj("policy_002"),
            subj("claim_002"),
            subj("settlement_002"),
            dec(80_000),
        ]
    ));
}

// ============================================================
// Invariants
// ============================================================

#[test]
fn settlement_id_must_be_unique_across_payments() {
    // One settlement_id on two different claims violates its uniqueness.
    let s = issue(State::default(), "policy_001", 100_000);
    let s = report(s, "claim_001", "policy_001", 10_000);
    let s = report(s, "claim_002", "policy_001", 10_000);
    let s = grant(s, "alex", 100_000);

    let s = ex().must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_001"), subj("settlement_001"), dec(10_000)],
        "alex",
        s,
    );
    // Second settlement reusing settlement_001 against a different claim.
    let reason = ex().must_reject_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_002"), subj("settlement_001"), dec(10_000)],
        "alex",
        &s,
    );
    assert!(
        reason
            .to_string()
            .contains("settlement_paid_unique_by_settlement_id"),
        "expected settlement-id uniqueness invariant violation, got: {reason}"
    );
}

#[test]
fn paid_without_authorised_violates_invariant() {
    // A payment with no matching authorisation. No transformation produces
    // this; the invariant refuses it however it arrives.
    let orphan_payment = claim_instance(
        "SettlementPaid",
        &[
            subj("policy_001"),
            subj("claim_001"),
            subj("settlement_001"),
            dec(30_000),
        ],
    );
    let state = State::from_claims(vec![orphan_payment]);
    let inv = insurance_claim_settlement::paid_implies_authorised();
    let holds = eval_invariant(&inv, &state, None, &[]).expect("eval should not error");
    assert!(
        !holds,
        "paid_implies_authorised should not hold when an orphan payment is admitted"
    );
}

#[test]
fn paid_without_headroom_violates_invariant() {
    // Without this rule, a payment on a policy with no PolicyHeadroom would
    // slip past headroom_consumed_by_payment, which is vacuously true there.
    let orphan_payment = claim_instance(
        "SettlementPaid",
        &[
            subj("policy_001"),
            subj("claim_001"),
            subj("settlement_001"),
            dec(30_000),
        ],
    );
    let state = State::from_claims(vec![orphan_payment]);
    let inv = insurance_claim_settlement::paid_implies_headroom();
    let holds = eval_invariant(&inv, &state, None, &[]).expect("eval should not error");
    assert!(
        !holds,
        "paid_implies_headroom should not hold when a payment exists with no PolicyHeadroom for that policy"
    );
}

// ============================================================
// Derived claim: PolicyLimitUsage
// ============================================================

#[test]
fn policy_limit_usage_sums_admitted_settlements_per_policy() {
    let s = issue(State::default(), "policy_001", 200_000);
    let s = issue(s, "policy_002", 200_000);
    let s = report(s, "claim_001", "policy_001", 50_000);
    let s = report(s, "claim_002", "policy_001", 25_000);
    let s = report(s, "claim_003", "policy_002", 100_000);
    let s = grant(s, "alex", 200_000);

    let s = ex().must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_001"), subj("settlement_001"), dec(50_000)],
        "alex",
        s,
    );
    let s = ex().must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_002"), subj("settlement_002"), dec(25_000)],
        "alex",
        s,
    );
    let s = ex().must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_003"), subj("settlement_003"), dec(100_000)],
        "alex",
        s,
    );

    let rows = enumerate_derived(
        &insurance_claim_settlement::policy_limit_usage(),
        &s,
        &insurance_claim_settlement::definitions(),
    )
    .expect("enumerate_derived should not error");

    // One row per distinct policy that has at least one payment.
    assert_eq!(rows.len(), 2, "expected one row per policy, got {rows:?}");

    let row_for = |policy: &str| -> &ClaimInstance {
        rows.iter()
            .find(|r| r.args.first() == Some(&subj(policy)))
            .unwrap_or_else(|| panic!("no PolicyLimitUsage row for {policy}"))
    };

    assert_eq!(row_for("policy_001").args[1], dec_str("75000"));
    assert_eq!(row_for("policy_002").args[1], dec_str("100000"));
}

#[test]
fn policy_limit_usage_empty_when_no_settlements_paid() {
    let s = issue(State::default(), "policy_001", 100_000);
    let s = report(s, "claim_001", "policy_001", 20_000);
    let rows = enumerate_derived(
        &insurance_claim_settlement::policy_limit_usage(),
        &s,
        &insurance_claim_settlement::definitions(),
    )
    .expect("enumerate_derived should not error");
    assert!(rows.is_empty(), "expected no rows, got {rows:?}");
}

// ============================================================
// PolicyHeadroom conservation
//
// Every payment must consume exactly its amount of headroom
// (`headroom_consumed_by_payment`). The gate asks "is there enough?"; the
// invariant asks "did the payment actually consume it?". Both are tested.
// ============================================================

/// An authorised settlement reduces PolicyHeadroom by exactly its amount:
/// 100k less a 30k payment leaves 70k.
#[test]
fn authorised_settlement_decrements_policy_headroom_by_payment_amount() {
    let pre = happy_pre();
    let post = ex().must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_001"), subj("settlement_001"), dec(30_000)],
        "alex",
        pre,
    );
    assert!(
        has_claim(&post, "PolicyHeadroom", &[subj("policy_001"), dec(70_000)]),
        "PolicyHeadroom must reflect aggregate - amount after settlement"
    );
    // And the pre-state headroom claim is gone.
    assert!(
        !has_claim(&post, "PolicyHeadroom", &[subj("policy_001"), dec(100_000)]),
        "pre-state PolicyHeadroom must be retracted"
    );
}

/// The conservation invariant catches a payment that leaves headroom
/// untouched. The gates still pass; only comparing the state before and
/// after reveals the bug, which a state invariant alone could not catch.
#[test]
fn conservation_invariant_catches_payment_that_skips_headroom_update() {
    // Built with the IR builder, not `.morph`: it is the real
    // transformation minus one statement, to show the invariant has teeth.
    use morpholog_core::ir_builder;

    let pre = happy_pre();

    // Everything the real one does except update PolicyHeadroom. The
    // gates still hold; the conservation invariant must reject.
    let buggy = ir_builder::transformation(
        "buggy_authorise_settlement",
        ir_builder::params(&["claim_id", "settlement_id", "amount"]),
        vec![
            ir_builder::bind_one(ir_builder::claim(
                "ClaimReported",
                vec![
                    ir_builder::var("claim_id"),
                    ir_builder::var("policy_id"),
                    ir_builder::wildcard(),
                ],
            )),
            ir_builder::bind_one(ir_builder::claim(
                "Policy",
                vec![
                    ir_builder::var("policy_id"),
                    ir_builder::var("aggregate_limit"),
                ],
            )),
            ir_builder::require(ir_builder::and(vec![
                ir_builder::claim(
                    "SettlementAuthority",
                    vec![ir_builder::actor(), ir_builder::var("actor_limit")],
                ),
                ir_builder::le(
                    ir_builder::term(ir_builder::var("amount")),
                    ir_builder::term(ir_builder::var("actor_limit")),
                ),
            ])),
            ir_builder::require(ir_builder::le(
                ir_builder::add(
                    ir_builder::sum(
                        ir_builder::var("paid"),
                        ir_builder::claim(
                            "SettlementPaid",
                            vec![
                                ir_builder::var("policy_id"),
                                ir_builder::wildcard(),
                                ir_builder::wildcard(),
                                ir_builder::var("paid"),
                            ],
                        ),
                    ),
                    ir_builder::term(ir_builder::var("amount")),
                ),
                ir_builder::term(ir_builder::var("aggregate_limit")),
            )),
            // Conspicuously missing: the let/retract/assert chain
            // that maintains PolicyHeadroom.
            ir_builder::assert_(
                "SettlementAuthorised",
                vec![
                    ir_builder::var("claim_id"),
                    ir_builder::var("settlement_id"),
                    ir_builder::var("amount"),
                    ir_builder::actor(),
                ],
            ),
            ir_builder::assert_(
                "SettlementPaid",
                vec![
                    ir_builder::var("policy_id"),
                    ir_builder::var("claim_id"),
                    ir_builder::var("settlement_id"),
                    ir_builder::var("amount"),
                ],
            ),
        ],
    );

    let outcome = ex()
        .propose_as(
            &buggy,
            vec![subj("claim_001"), subj("settlement_001"), dec(30_000)],
            "alex",
            &pre,
        )
        .expect("kernel must not error");

    match outcome {
        Outcome::Rejected { reason } => {
            assert!(
                reason.to_string().contains("headroom_consumed_by_payment"),
                "expected rejection to name the conservation invariant, got: {reason}"
            );
        }
        Outcome::Accepted { .. } => {
            panic!(
                "a buggy authorise_settlement that admits SettlementPaid \
                 without consuming PolicyHeadroom must be rejected"
            )
        }
    }
}

/// Two 30k payments with a single 30k headroom decrement. A per-payment
/// check (`70 = 100 - 30`) would pass; the sum-based rule refuses it:
/// 70 != 100 - (30 + 30) = 40.
#[test]
fn conservation_invariant_catches_multi_payment_with_single_decrement() {
    use morpholog_core::ir_builder;

    // Two reported claims, so the two payments have real claim ids.
    let pre = {
        let s = issue(State::default(), "policy_001", 100_000);
        let s = report(s, "claim_a", "policy_001", 20_000);
        let s = report(s, "claim_b", "policy_001", 20_000);
        grant(s, "alex", 50_000)
    };

    // Two 30k payments, one headroom decrement. The aggregate gate passes
    // (0 + 60 <= 100k); only the conservation invariant catches it.
    let buggy = ir_builder::transformation(
        "buggy_multi_payment",
        ir_builder::params(&["amount"]),
        vec![
            ir_builder::bind_one(ir_builder::claim(
                "PolicyHeadroom",
                vec![
                    ir_builder::subj("policy_001"),
                    ir_builder::var("current_headroom"),
                ],
            )),
            ir_builder::require(ir_builder::claim(
                "SettlementAuthority",
                vec![ir_builder::actor(), ir_builder::wildcard()],
            )),
            ir_builder::let_(
                "new_headroom",
                ir_builder::sub(
                    ir_builder::term(ir_builder::var("current_headroom")),
                    ir_builder::term(ir_builder::var("amount")),
                ),
            ),
            ir_builder::retract(
                "PolicyHeadroom",
                vec![
                    ir_builder::subj("policy_001"),
                    ir_builder::var("current_headroom"),
                ],
            ),
            ir_builder::assert_(
                "PolicyHeadroom",
                vec![
                    ir_builder::subj("policy_001"),
                    ir_builder::var("new_headroom"),
                ],
            ),
            // Two authorised payments of `amount`: 2*amount paid, but only
            // 1*amount of headroom consumed.
            ir_builder::assert_(
                "SettlementAuthorised",
                vec![
                    ir_builder::subj("claim_a"),
                    ir_builder::subj("settlement_a"),
                    ir_builder::var("amount"),
                    ir_builder::actor(),
                ],
            ),
            ir_builder::assert_(
                "SettlementPaid",
                vec![
                    ir_builder::subj("policy_001"),
                    ir_builder::subj("claim_a"),
                    ir_builder::subj("settlement_a"),
                    ir_builder::var("amount"),
                ],
            ),
            ir_builder::assert_(
                "SettlementAuthorised",
                vec![
                    ir_builder::subj("claim_b"),
                    ir_builder::subj("settlement_b"),
                    ir_builder::var("amount"),
                    ir_builder::actor(),
                ],
            ),
            ir_builder::assert_(
                "SettlementPaid",
                vec![
                    ir_builder::subj("policy_001"),
                    ir_builder::subj("claim_b"),
                    ir_builder::subj("settlement_b"),
                    ir_builder::var("amount"),
                ],
            ),
        ],
    );

    let outcome = ex()
        .propose_as(&buggy, vec![dec(30_000)], "alex", &pre)
        .expect("kernel must not error");

    match outcome {
        Outcome::Rejected { reason } => {
            assert!(
                reason.to_string().contains("headroom_consumed_by_payment"),
                "expected rejection to name the conservation invariant, got: {reason}"
            );
        }
        Outcome::Accepted { .. } => {
            panic!(
                "a buggy multi-payment transition that consumes headroom only \
                 once while admitting two SettlementPaid claims must be rejected"
            )
        }
    }
}

// ============================================================
// Per-claim coverage layer (min / max)
// ============================================================
//
// When coverage terms are set, a settlement may not exceed the eligible
// payout - min(per_claim_limit, max(0, loss - deductible)). These tests
// exercise the cap (min), the deductible floor (max(0, ...)), and the
// net-of-deductible amount in between.

fn set_terms(state: State, policy_id: &str, deductible: i64, per_claim_limit: i64) -> State {
    ex().must_accept(
        &insurance_claim_settlement::set_coverage_terms(),
        vec![subj(policy_id), dec(deductible), dec(per_claim_limit)],
        state,
    )
}

#[test]
fn settlement_capped_at_per_claim_limit() {
    // deductible 1_000, per-claim limit 50_000, loss 70_000:
    // eligible = min(50_000, max(0, 70_000 - 1_000)) = 50_000 (the min cap).
    let mut s = issue(State::default(), "p1", 1_000_000);
    s = grant(s, "alex", 1_000_000);
    s = set_terms(s, "p1", 1_000, 50_000);
    s = report(s, "c1", "p1", 70_000);

    ex().must_reject_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("c1"), subj("s_over"), dec(50_001)],
        "alex",
        &s,
    );

    let ok = ex().must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("c1"), subj("s_ok"), dec(50_000)],
        "alex",
        s,
    );
    assert!(has_claim(
        &ok,
        "SettlementAuthorised",
        &[subj("c1"), subj("s_ok"), dec(50_000), subj("alex")]
    ));
}

#[test]
fn loss_below_deductible_yields_no_payout() {
    // loss 500, deductible 1_000: eligible = min(50_000, max(0, -500)) = 0
    // (the max(0, ...) floor). Any positive settlement is rejected.
    let mut s = issue(State::default(), "p1", 1_000_000);
    s = grant(s, "alex", 1_000_000);
    s = set_terms(s, "p1", 1_000, 50_000);
    s = report(s, "c1", "p1", 500);

    ex().must_reject_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("c1"), subj("s1"), dec(1)],
        "alex",
        &s,
    );
}

#[test]
fn settlement_pays_loss_net_of_deductible() {
    // loss 30_000, deductible 1_000, limit 50_000:
    // eligible = min(50_000, max(0, 29_000)) = 29_000. 29_000 admits, 29_001 not.
    let mut s = issue(State::default(), "p1", 1_000_000);
    s = grant(s, "alex", 1_000_000);
    s = set_terms(s, "p1", 1_000, 50_000);
    s = report(s, "c1", "p1", 30_000);

    ex().must_reject_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("c1"), subj("s_over"), dec(29_001)],
        "alex",
        &s,
    );

    let ok = ex().must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("c1"), subj("s_ok"), dec(29_000)],
        "alex",
        s,
    );
    assert!(has_claim(
        &ok,
        "SettlementAuthorised",
        &[subj("c1"), subj("s_ok"), dec(29_000), subj("alex")]
    ));
}

#[test]
fn nonsensical_coverage_terms_are_rejected() {
    // coverage_terms_within_range refuses a negative deductible or a
    // non-positive per-claim limit, so the layer cannot be set up on
    // terms that would make the eligible-payout rule meaningless.
    let s = issue(State::default(), "p1", 1_000_000);

    ex().must_reject(
        &insurance_claim_settlement::set_coverage_terms(),
        vec![subj("p1"), dec(-1), dec(50_000)],
        &s,
    );

    ex().must_reject(
        &insurance_claim_settlement::set_coverage_terms(),
        vec![subj("p1"), dec(1_000), dec(0)],
        &s,
    );
}
