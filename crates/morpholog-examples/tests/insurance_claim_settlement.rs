//! Integration tests for the insurance-claim-settlement example
//! (`examples/05_insurance_claim_settlement/`).
//!
//! Four-section coverage:
//!
//! - **Policy and claim plumbing.** `issue_policy` admits a `Policy`;
//!   `report_claim` requires the policy to exist.
//!
//! - **Actor authority gate.** `authorise_settlement` rejects without
//!   a covering `SettlementAuthority`; rejects when proposed amount
//!   exceeds the actor's limit; admits at the boundary.
//!
//! - **Cumulative aggregate-limit gate.** This is the load-bearing
//!   `Expr::Add` shape. Pins under-cap admission, exact-fill boundary
//!   equality, and over-cap rejection that surfaces from the
//!   `Le(Add(running, proposed), aggregate)` require.
//!
//! - **Read-side projection.** `PolicyLimitUsage` enumeration matches
//!   the sum of admitted `SettlementPaid` per policy.
//!
//! Plus a kernel-level guard: the `paid_implies_authorised` invariant
//! rejects a hand-constructed state where a payment exists without a
//! matching authorisation.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{
    dec, dec_str, has_claim, must_accept, must_accept_as, propose_as, propose_with_test_actor, subj,
};
use morpholog_core::{ClaimInstance, Invariant, Outcome, State, enumerate_derived, eval_invariant};
use morpholog_examples::insurance_claim_settlement;

fn invariants() -> Vec<Invariant> {
    insurance_claim_settlement::all_invariants()
}

fn issue(state: State, policy_id: &str, aggregate_limit: i64) -> State {
    must_accept(
        &insurance_claim_settlement::issue_policy(),
        vec![subj(policy_id), dec(aggregate_limit)],
        state,
        &invariants(),
    )
}

fn report(state: State, claim_id: &str, policy_id: &str, claimed_amount: i64) -> State {
    must_accept(
        &insurance_claim_settlement::report_claim(),
        vec![subj(claim_id), subj(policy_id), dec(claimed_amount)],
        state,
        &invariants(),
    )
}

fn grant(state: State, actor: &str, limit: i64) -> State {
    must_accept(
        &insurance_claim_settlement::grant_settlement_authority(),
        vec![subj(actor), dec(limit)],
        state,
        &invariants(),
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

/// `issue_policy` also admits initial `PolicyHeadroom(policy_id,
/// aggregate_limit)` - the operational remaining-capacity counter
/// that the conservation invariant (added in the next commit) will
/// constrain. At issuance, remaining equals the aggregate limit.
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
    let outcome = propose_with_test_actor(
        &insurance_claim_settlement::issue_policy(),
        vec![subj("policy_001"), dec(50_000)],
        &pre,
        &invariants(),
    )
    .expect("propose should not error");
    let Outcome::Rejected { reason } = outcome else {
        panic!("expected Rejected, got {outcome:?}");
    };
    assert!(
        reason.contains("at_most_one_policy_per_id"),
        "expected at_most_one_policy_per_id invariant violation, got: {reason}"
    );
}

#[test]
fn report_claim_without_policy_is_rejected_at_require() {
    let pre = State::default();
    let outcome = propose_with_test_actor(
        &insurance_claim_settlement::report_claim(),
        vec![subj("claim_001"), subj("policy_001"), dec(20_000)],
        &pre,
        &invariants(),
    )
    .expect("propose should not error");
    let Outcome::Rejected { reason } = outcome else {
        panic!("expected Rejected, got {outcome:?}");
    };
    assert!(reason.contains("require"), "got reason: {reason}");
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
    let outcome = propose_with_test_actor(
        &insurance_claim_settlement::report_claim(),
        vec![subj("claim_001"), subj("policy_001"), dec(30_000)],
        &pre,
        &invariants(),
    )
    .expect("propose should not error");
    let Outcome::Rejected { reason } = outcome else {
        panic!("expected Rejected, got {outcome:?}");
    };
    assert!(
        reason.contains("at_most_one_claim_report_per_id"),
        "expected at_most_one_claim_report_per_id invariant violation, got: {reason}"
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
    let post = must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_001"), subj("settlement_001"), dec(30_000)],
        subj("alex"),
        pre,
        &invariants(),
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
    // Migrated to propose_with_trace as the canonical demonstration
    // of the PR D DX win. The old assertion was `reason.contains
    // ("require")`, which proved a require failed but not *which*
    // one. The trace assertion below proves:
    //
    //   1. All three bind_ones succeeded (claim_001's policy_id,
    //      policy_001's aggregate_limit, and policy_001's
    //      current PolicyHeadroom were all bound).
    //   2. The subsequent require - the SettlementAuthority + Le
    //      gate - is the one that rejected.
    //
    // That is the kind of precision a future test author should
    // reach for instead of `reason.contains(...)`.
    use morpholog_core::{
        BindOneOutcome, RequireOutcome, TraceEntry, TracedProposal, Transition, propose_with_trace,
    };
    let pre = {
        let s = issue(State::default(), "policy_001", 100_000);
        report(s, "claim_001", "policy_001", 20_000)
    };
    let t = insurance_claim_settlement::authorise_settlement();
    let transition = Transition {
        transformation_name: t.name.clone(),
        args: vec![subj("claim_001"), subj("settlement_001"), dec(30_000)],
        actor: subj("alex"),
    };
    let TracedProposal::Completed { outcome, trace } =
        propose_with_trace(&t, &transition, &pre, &invariants())
    else {
        panic!("expected Completed");
    };
    assert!(
        matches!(outcome, Outcome::Rejected { .. }),
        "expected Rejected, got {outcome:?}"
    );

    // Step 1: both bind_ones (ClaimReported, Policy) succeeded.
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

    // Step 2: the require that rejected has SettlementAuthority in
    // its rendered expression.
    let failing = trace.iter().find_map(|e| match e {
        TraceEntry::Require {
            expression,
            outcome: RequireOutcome::Rejected { .. },
        } => Some(expression),
        _ => None,
    });
    let expr = failing.expect("expected exactly one failing require entry");
    assert!(
        expr.contains("SettlementAuthority"),
        "failing require should be the authority gate; got: {expr}"
    );
}

#[test]
fn authorise_settlement_above_actor_limit_is_rejected_at_require() {
    let pre = happy_pre(); // alex has 50k limit
    let outcome = propose_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_001"), subj("settlement_001"), dec(60_000)],
        subj("alex"),
        &pre,
        &invariants(),
    )
    .expect("propose should not error");
    let Outcome::Rejected { reason } = outcome else {
        panic!("expected Rejected, got {outcome:?}");
    };
    assert!(reason.contains("require"), "got reason: {reason}");
}

#[test]
fn authorise_settlement_at_actor_boundary_admits() {
    let pre = happy_pre(); // alex has 50k limit
    let post = must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_001"), subj("settlement_001"), dec(50_000)],
        subj("alex"),
        pre,
        &invariants(),
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
// Cumulative aggregate-limit gate (the Expr::Add forcing function)
// ============================================================

/// Setup for cumulative-cap tests: actor authority high enough that
/// the actor gate does not interact with the aggregate gate (the cap
/// being tested). Same policy / claim shape as `happy_pre`.
fn cap_pre() -> State {
    let s = issue(State::default(), "policy_001", 100_000);
    let s = report(s, "claim_001", "policy_001", 60_000);
    grant(s, "alex", 100_000)
}

fn after_first_settlement(amount: i64) -> State {
    let pre = cap_pre();
    must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_001"), subj("settlement_001"), dec(amount)],
        subj("alex"),
        pre,
        &invariants(),
    )
}

#[test]
fn second_settlement_under_remaining_aggregate_admits() {
    let pre = after_first_settlement(40_000); // 40k of 100k consumed
    let pre = report(pre, "claim_002", "policy_001", 30_000);
    let post = must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_002"), subj("settlement_002"), dec(30_000)],
        subj("alex"),
        pre,
        &invariants(),
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
    let post = must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_002"), subj("settlement_002"), dec(40_000)],
        subj("alex"),
        pre,
        &invariants(),
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
    // 60 + 50 = 110 > 100.
    //
    // Asserts via the trace that the actor-authority require Held
    // (alex's 100k limit covers a 50k settlement) but the aggregate
    // require Rejected (cumulative paid + proposed exceeds the
    // policy's aggregate). The old `reason.contains("require")`
    // could only prove a require failed; the trace identifies the
    // specific gate.
    use morpholog_core::{
        RequireOutcome, TraceEntry, TracedProposal, Transition, propose_with_trace,
    };
    let pre = after_first_settlement(60_000);
    let pre = report(pre, "claim_002", "policy_001", 50_000);
    let t = insurance_claim_settlement::authorise_settlement();
    let transition = Transition {
        transformation_name: t.name.clone(),
        args: vec![subj("claim_002"), subj("settlement_002"), dec(50_000)],
        actor: subj("alex"),
    };
    let TracedProposal::Completed { outcome, trace } =
        propose_with_trace(&t, &transition, &pre, &invariants())
    else {
        panic!("expected Completed");
    };
    assert!(
        matches!(outcome, Outcome::Rejected { .. }),
        "expected Rejected, got {outcome:?}"
    );

    let require_outcomes: Vec<(&str, &RequireOutcome)> = trace
        .iter()
        .filter_map(|e| match e {
            TraceEntry::Require {
                expression,
                outcome,
            } => Some((expression.as_str(), outcome)),
            _ => None,
        })
        .collect();
    // The first require (actor authority + amount-vs-limit) holds;
    // the second (cumulative cap) rejects.
    let held_authority = require_outcomes
        .iter()
        .find(|(expr, out)| {
            expr.contains("SettlementAuthority") && matches!(out, RequireOutcome::Held { .. })
        })
        .is_some();
    let rejected_aggregate = require_outcomes
        .iter()
        .find(|(expr, out)| {
            expr.contains("aggregate_limit") && matches!(out, RequireOutcome::Rejected { .. })
        })
        .is_some();
    assert!(
        held_authority,
        "expected the SettlementAuthority gate to hold; trace: {trace:#?}"
    );
    assert!(
        rejected_aggregate,
        "expected the aggregate_limit gate to reject; trace: {trace:#?}"
    );
}

#[test]
fn aggregate_limit_scoped_per_policy() {
    // policy_001 fully consumed at 100k; policy_002 still empty.
    let s = issue(State::default(), "policy_001", 100_000);
    let s = issue(s, "policy_002", 100_000);
    let s = report(s, "claim_001", "policy_001", 100_000);
    let s = grant(s, "alex", 100_000);
    let s = must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_001"), subj("settlement_001"), dec(100_000)],
        subj("alex"),
        s,
        &invariants(),
    );
    // policy_002 should still accept a fresh settlement up to its own limit.
    let s = report(s, "claim_002", "policy_002", 80_000);
    let post = must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_002"), subj("settlement_002"), dec(80_000)],
        subj("alex"),
        s,
        &invariants(),
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
    // Two settlements with the same settlement_id but different
    // claim_ids would violate identity uniqueness. The invariant
    // catches this on the candidate state.
    let s = issue(State::default(), "policy_001", 100_000);
    let s = report(s, "claim_001", "policy_001", 10_000);
    let s = report(s, "claim_002", "policy_001", 10_000);
    let s = grant(s, "alex", 100_000);

    let s = must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_001"), subj("settlement_001"), dec(10_000)],
        subj("alex"),
        s,
        &invariants(),
    );
    // Second settlement reusing settlement_001 against a different claim.
    let outcome = propose_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_002"), subj("settlement_001"), dec(10_000)],
        subj("alex"),
        &s,
        &invariants(),
    )
    .expect("propose should not error");
    let Outcome::Rejected { reason } = outcome else {
        panic!("expected Rejected, got {outcome:?}");
    };
    assert!(
        reason.contains("settlement_id_uniquely_identifies_payment"),
        "expected settlement-id uniqueness invariant violation, got: {reason}"
    );
}

#[test]
fn paid_without_authorised_violates_invariant() {
    // Hand-construct a state with a payment but no matching
    // authorisation. The transformations never produce this; the
    // invariant exists so the runtime contract holds against
    // candidate states regardless of how they arrived.
    let orphan_payment = ClaimInstance {
        predicate: "SettlementPaid".to_string(),
        args: vec![
            subj("policy_001"),
            subj("claim_001"),
            subj("settlement_001"),
            dec(30_000),
        ],
    };
    let state = State::from_claims(vec![orphan_payment]);
    let inv = insurance_claim_settlement::paid_implies_authorised();
    let holds = eval_invariant(&inv, &state, None).expect("eval should not error");
    assert!(
        !holds,
        "paid_implies_authorised should not hold when an orphan payment is admitted"
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

    let s = must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_001"), subj("settlement_001"), dec(50_000)],
        subj("alex"),
        s,
        &invariants(),
    );
    let s = must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_002"), subj("settlement_002"), dec(25_000)],
        subj("alex"),
        s,
        &invariants(),
    );
    let s = must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_003"), subj("settlement_003"), dec(100_000)],
        subj("alex"),
        s,
        &invariants(),
    );

    let rows = enumerate_derived(&insurance_claim_settlement::policy_limit_usage(), &s)
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
    let rows = enumerate_derived(&insurance_claim_settlement::policy_limit_usage(), &s)
        .expect("enumerate_derived should not error");
    assert!(rows.is_empty(), "expected no rows, got {rows:?}");
}

// ============================================================
// PolicyHeadroom conservation
//
// PR #70's payoff: every payment must consume exactly its amount
// of headroom, enforced by the `headroom_consumed_by_payment`
// transition invariant. The require gate ("is there enough?") and
// the invariant ("did the payment actually consume?") answer
// different questions; both are kept and both are tested here.
// ============================================================

/// Happy path: an authorised settlement reduces PolicyHeadroom by
/// exactly the payment amount. Pre-state headroom for `policy_001`
/// is 100k (the aggregate at issuance); a 30k payment leaves 70k.
#[test]
fn authorised_settlement_decrements_policy_headroom_by_payment_amount() {
    let pre = happy_pre();
    let post = must_accept_as(
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_001"), subj("settlement_001"), dec(30_000)],
        subj("alex"),
        pre,
        &invariants(),
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

/// Load-bearing test: the transition invariant catches a payment
/// that did not properly consume headroom. Constructs a buggy
/// transformation that admits SettlementPaid without touching
/// PolicyHeadroom - the aggregate-limit require still passes
/// (there's been no spending yet) but the conservation invariant
/// fails because pre-headroom and post-headroom are identical
/// while a new SettlementPaid was admitted.
///
/// This is the kind of bug a state invariant alone could not catch.
/// Both PolicyHeadroom(p, 100_000) and SettlementPaid(p, ..., 30_000)
/// are perfectly admissible singly; only the relationship between
/// the pre-state and post-state falsifies the rule.
#[test]
fn conservation_invariant_catches_payment_that_skips_headroom_update() {
    use morpholog_core::Transformation;
    use morpholog_core::dsl;

    let pre = happy_pre();

    // A buggy authorise_settlement that does everything the real
    // one does EXCEPT retract+assert PolicyHeadroom. The require
    // gates still hold (alex has 50k authority; 30k <= 100k
    // aggregate); the conservation invariant must reject.
    let buggy = Transformation {
        name: "buggy_authorise_settlement".to_string(),
        parameters: dsl::params(&["claim_id", "settlement_id", "amount"]),
        body: vec![
            dsl::bind_one(dsl::claim(
                "ClaimReported",
                vec![dsl::var("claim_id"), dsl::var("policy_id"), dsl::wildcard()],
            )),
            dsl::bind_one(dsl::claim(
                "Policy",
                vec![dsl::var("policy_id"), dsl::var("aggregate_limit")],
            )),
            dsl::require(dsl::and(vec![
                dsl::claim(
                    "SettlementAuthority",
                    vec![dsl::actor(), dsl::var("actor_limit")],
                ),
                dsl::le(
                    dsl::term(dsl::var("amount")),
                    dsl::term(dsl::var("actor_limit")),
                ),
            ])),
            dsl::require(dsl::le(
                dsl::add(
                    dsl::sum(
                        dsl::var("paid"),
                        "paid",
                        dsl::claim(
                            "SettlementPaid",
                            vec![
                                dsl::var("policy_id"),
                                dsl::wildcard(),
                                dsl::wildcard(),
                                dsl::var("paid"),
                            ],
                        ),
                    ),
                    dsl::term(dsl::var("amount")),
                ),
                dsl::term(dsl::var("aggregate_limit")),
            )),
            // Conspicuously missing: the let/retract/assert chain
            // that maintains PolicyHeadroom.
            dsl::assert_(
                "SettlementAuthorised",
                vec![
                    dsl::var("claim_id"),
                    dsl::var("settlement_id"),
                    dsl::var("amount"),
                    dsl::actor(),
                ],
            ),
            dsl::assert_(
                "SettlementPaid",
                vec![
                    dsl::var("policy_id"),
                    dsl::var("claim_id"),
                    dsl::var("settlement_id"),
                    dsl::var("amount"),
                ],
            ),
        ],
    };

    let outcome = propose_as(
        &buggy,
        vec![subj("claim_001"), subj("settlement_001"), dec(30_000)],
        subj("alex"),
        &pre,
        &invariants(),
    )
    .expect("kernel must not error");

    match outcome {
        Outcome::Rejected { reason } => {
            assert!(
                reason.contains("headroom_consumed_by_payment"),
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
