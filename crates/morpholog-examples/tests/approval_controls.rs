//! Integration tests for the approval controls example
//! (`examples/04_approval_controls/`).
//!
//! Two-section coverage matching the example's two authority shapes:
//!
//! - **Unconditional authority** (`MayApprove`, `approve_document`):
//!   actor consultation via `Term::Actor`, rejection without grant,
//!   asserted `Approval` carries the proposing actor, one actor
//!   cannot impersonate another, revocation preserves history.
//!
//! - **Quantitative authority** (`ApprovalLimit`, `approve_within_limit`):
//!   the same shape with a decimal `Prop::Compare` on amount-against-limit, boundary
//!   equality, stacked grants, per-doc-type scoping, and the
//!   ill-typed-limit doctrine.
//!
//! Plus kernel-level guards that make the require-vs-invariant
//! doctrine catchable: `Term::Actor` in an invariant body raises
//! `UnboundActor`, and the same error is position-independent in
//! `find_claim_matches`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{claim_instance, dec, has_claim, must_accept, must_accept_as, propose_as, subj};
use morpholog_core::ir_builder::invariant;
use morpholog_core::{
    EvalError, EvalValue, Invariant, Outcome, Prop, State, Subject, Term, Value, eval_invariant,
};
use morpholog_examples::approval_controls;

fn empty_invariants() -> Vec<Invariant> {
    approval_controls::all_invariants()
}

fn grant_authority(state: State, actor: &str, doc_type: &str) -> State {
    must_accept(
        &approval_controls::grant_approval_authority(),
        vec![subj(actor), subj(doc_type)],
        state,
        &empty_invariants(),
    )
}

fn grant_limit(state: State, actor: &str, doc_type: &str, limit: i64) -> State {
    must_accept(
        &approval_controls::grant_approval_limit(),
        vec![subj(actor), subj(doc_type), dec(limit)],
        state,
        &empty_invariants(),
    )
}

// ============================================================
// Unconditional authority
// ============================================================

#[test]
fn approve_without_authority_is_rejected_at_require() {
    let pre = State::default();
    let outcome = propose_as(
        &approval_controls::approve_document(),
        vec![subj("doc_001"), subj("vendor_onboarding")],
        "jordan",
        &pre,
        &empty_invariants(),
    )
    .expect("propose should not error");
    let Outcome::Rejected { reason } = outcome else {
        panic!("expected Rejected, got {outcome:?}");
    };
    assert!(reason.contains("require"), "got reason: {reason}");
}

#[test]
fn approve_with_authority_carries_proposing_actor_on_asserted_claim() {
    let pre = grant_authority(State::default(), "jordan", "vendor_onboarding");
    let post = must_accept_as(
        &approval_controls::approve_document(),
        vec![subj("doc_001"), subj("vendor_onboarding")],
        "jordan",
        pre,
        &empty_invariants(),
    );
    assert!(
        has_claim(
            &post,
            "Approval",
            &[subj("doc_001"), subj("vendor_onboarding"), subj("jordan")],
        ),
        "Approval must carry the proposing actor as its third arg",
    );
}

#[test]
fn approve_uses_proposing_actor_not_a_caller_parameter() {
    // jordan has authority; alice does not. alice cannot approve on
    // jordan's behalf because $actor binds to the proposing actor,
    // not to a parameter the caller controls.
    let pre = grant_authority(State::default(), "jordan", "vendor_onboarding");
    let outcome = propose_as(
        &approval_controls::approve_document(),
        vec![subj("doc_001"), subj("vendor_onboarding")],
        "alice",
        &pre,
        &empty_invariants(),
    )
    .expect("propose should not error");
    let Outcome::Rejected { .. } = outcome else {
        panic!("alice should not be able to approve without authority");
    };
}

#[test]
fn revoked_authority_blocks_future_but_preserves_past() {
    let pre = grant_authority(State::default(), "jordan", "vendor_onboarding");
    let after_approval = must_accept_as(
        &approval_controls::approve_document(),
        vec![subj("doc_001"), subj("vendor_onboarding")],
        "jordan",
        pre,
        &empty_invariants(),
    );
    let after_revoke = must_accept(
        &approval_controls::revoke_approval_authority(),
        vec![subj("jordan"), subj("vendor_onboarding")],
        after_approval,
        &empty_invariants(),
    );

    // History survives, future is blocked.
    assert!(has_claim(
        &after_revoke,
        "Approval",
        &[subj("doc_001"), subj("vendor_onboarding"), subj("jordan")],
    ));
    assert!(!has_claim(
        &after_revoke,
        "MayApprove",
        &[subj("jordan"), subj("vendor_onboarding")],
    ));
    let outcome = propose_as(
        &approval_controls::approve_document(),
        vec![subj("doc_002"), subj("vendor_onboarding")],
        "jordan",
        &after_revoke,
        &empty_invariants(),
    )
    .expect("propose should not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

// ============================================================
// Quantitative authority
// ============================================================

#[test]
fn limit_approval_without_grant_is_rejected() {
    let outcome = propose_as(
        &approval_controls::approve_within_limit(),
        vec![subj("inv_001"), subj("invoice"), dec(100)],
        "jordan",
        &State::default(),
        &empty_invariants(),
    )
    .expect("propose should not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn limit_approval_under_limit_commits_with_actor_and_amount() {
    let pre = grant_limit(State::default(), "jordan", "invoice", 1000);
    let post = must_accept_as(
        &approval_controls::approve_within_limit(),
        vec![subj("inv_001"), subj("invoice"), dec(750)],
        "jordan",
        pre,
        &empty_invariants(),
    );
    assert!(has_claim(
        &post,
        "LimitedApproval",
        &[subj("inv_001"), subj("invoice"), dec(750), subj("jordan")],
    ));
}

#[test]
fn limit_approval_exactly_at_limit_commits() {
    // Le is inclusive at the boundary.
    let pre = grant_limit(State::default(), "jordan", "invoice", 1000);
    let post = must_accept_as(
        &approval_controls::approve_within_limit(),
        vec![subj("inv_at_limit"), subj("invoice"), dec(1000)],
        "jordan",
        pre,
        &empty_invariants(),
    );
    assert!(has_claim(
        &post,
        "LimitedApproval",
        &[
            subj("inv_at_limit"),
            subj("invoice"),
            dec(1000),
            subj("jordan")
        ],
    ));
}

#[test]
fn limit_approval_above_limit_is_rejected() {
    let pre = grant_limit(State::default(), "jordan", "invoice", 1000);
    let outcome = propose_as(
        &approval_controls::approve_within_limit(),
        vec![subj("inv_over"), subj("invoice"), dec(1001)],
        "jordan",
        &pre,
        &empty_invariants(),
    )
    .expect("propose should not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn limit_grant_is_per_actor_and_per_doc_type() {
    // jordan has an invoice limit but not a contract limit.
    let pre = grant_limit(State::default(), "jordan", "invoice", 1000);

    // alice cannot approve under jordan's invoice limit.
    let outcome = propose_as(
        &approval_controls::approve_within_limit(),
        vec![subj("inv_alice"), subj("invoice"), dec(10)],
        "alice",
        &pre,
        &empty_invariants(),
    )
    .expect("propose should not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));

    // jordan cannot use her invoice limit for contracts.
    let outcome = propose_as(
        &approval_controls::approve_within_limit(),
        vec![subj("ct_001"), subj("contract"), dec(10)],
        "jordan",
        &pre,
        &empty_invariants(),
    )
    .expect("propose should not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn multiple_grants_take_the_satisfying_one() {
    // jordan holds two layered grants: 500 and 5000. An approval at
    // 3000 satisfies the second but not the first; the And + binding
    // shape of the require finds *some* satisfying limit.
    let pre = grant_limit(State::default(), "jordan", "invoice", 500);
    let pre = grant_limit(pre, "jordan", "invoice", 5000);
    let post = must_accept_as(
        &approval_controls::approve_within_limit(),
        vec![subj("inv_3k"), subj("invoice"), dec(3000)],
        "jordan",
        pre,
        &empty_invariants(),
    );
    assert!(has_claim(
        &post,
        "LimitedApproval",
        &[subj("inv_3k"), subj("invoice"), dec(3000), subj("jordan")],
    ));
}

#[test]
fn revoking_a_limit_blocks_future_but_preserves_past() {
    let pre = grant_limit(State::default(), "jordan", "invoice", 1000);
    let after_approval = must_accept_as(
        &approval_controls::approve_within_limit(),
        vec![subj("inv_001"), subj("invoice"), dec(800)],
        "jordan",
        pre,
        &empty_invariants(),
    );
    let after_revoke = must_accept(
        &approval_controls::revoke_approval_limit(),
        vec![subj("jordan"), subj("invoice"), dec(1000)],
        after_approval,
        &empty_invariants(),
    );

    // History survives, future blocked.
    assert!(has_claim(
        &after_revoke,
        "LimitedApproval",
        &[subj("inv_001"), subj("invoice"), dec(800), subj("jordan")],
    ));
    assert!(!has_claim(
        &after_revoke,
        "ApprovalLimit",
        &[subj("jordan"), subj("invoice"), dec(1000)],
    ));
    let outcome = propose_as(
        &approval_controls::approve_within_limit(),
        vec![subj("inv_002"), subj("invoice"), dec(500)],
        "jordan",
        &after_revoke,
        &empty_invariants(),
    )
    .expect("propose should not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn non_decimal_limit_in_authority_claim_surfaces_as_type_mismatch() {
    // Doctrine: ill-typed admitted claims are structural corruption,
    // not business rejection. An `ApprovalLimit($actor, doc_type, X)`
    // where `X` is not a decimal causes the decimal `Prop::Compare`
    // (amount <= limit) to raise `EvalError::TypeMismatch`. Until typed
    // predicates land,
    // this example's callers are trusted to admit decimal limits.
    let pre = State::from_claims(vec![claim_instance(
        "ApprovalLimit",
        &[
            subj("jordan"),
            subj("invoice"),
            EvalValue::Subject("not_a_decimal".into()),
        ],
    )]);

    let mut transition = common::test_transition(
        &approval_controls::approve_within_limit(),
        vec![subj("inv_001"), subj("invoice"), dec(100)],
    );
    transition.actor = Subject::from("jordan");

    let err = morpholog_core::propose(
        &approval_controls::approve_within_limit(),
        &transition,
        &pre,
        &empty_invariants(),
    )
    .expect_err("non-decimal limit must surface as EvalError, not Rejected");
    match err {
        EvalError::TypeMismatch(msg) => {
            assert!(
                msg.contains("decimal-domain operands") && msg.contains("subject"),
                "TypeMismatch names the domain requirement and the offending kind; got `{msg}`",
            );
        }
        other => panic!("expected TypeMismatch, got {other:?}"),
    }
}

// ============================================================
// Kernel-level pins for Term::Actor
// ============================================================

#[test]
fn term_actor_in_invariant_body_surfaces_as_unbound_actor() {
    let inv = invariant(
        "improperly_uses_actor",
        Prop::Claim {
            predicate: "AnyPredicate".into(),
            args: vec![Term::Actor],
        },
    );
    let err = eval_invariant(&inv, &State::default(), None).expect_err("must error");
    assert!(matches!(err, EvalError::UnboundActor));
}

#[test]
fn term_actor_unbound_error_is_position_independent() {
    // Regression: an earlier ground arg with a missing bucket must
    // NOT short-circuit before Term::Actor is checked. The
    // pre-pass in find_claim_matches makes the doctrine
    // position-independent.
    let inv = invariant(
        "actor_masked_by_earlier_missing_literal",
        Prop::Claim {
            predicate: "AnyPredicate".into(),
            args: vec![Term::Literal(Value::Subject("missing".into())), Term::Actor],
        },
    );
    let err = eval_invariant(&inv, &State::default(), None)
        .expect_err("Term::Actor outside transition scope must error regardless of arg order");
    assert!(matches!(err, EvalError::UnboundActor));
}

// ============================================================
// Candidate-supplier lookup - the `explain` engine's one-hop
// "what transformation could supply this missing claim?" analysis.
// ============================================================

#[test]
fn transformations_asserting_names_the_sole_supplier_of_an_authority_claim() {
    use morpholog_core::transformations_asserting;
    let program = approval_controls::program();

    // `approve_document` rejects when `MayApprove(actor, doc_type)` is
    // absent; the candidate supplier of that claim is the grant.
    assert_eq!(
        transformations_asserting(&program, "MayApprove"),
        vec!["grant_approval_authority"],
    );
    // The quantitative half mirrors it.
    assert_eq!(
        transformations_asserting(&program, "ApprovalLimit"),
        vec!["grant_approval_limit"],
    );
}

#[test]
fn transformations_asserting_is_empty_for_an_unasserted_predicate() {
    use morpholog_core::transformations_asserting;
    let program = approval_controls::program();

    // No transformation asserts a predicate the vocabulary never admits,
    // so there is no candidate supplier to name. An empty list is the
    // honest answer, not an error.
    assert!(transformations_asserting(&program, "NoSuchPredicate").is_empty());
}
