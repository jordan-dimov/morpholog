//! Integration tests for the approval controls example
//! (`examples/04_approval_controls/`).
//!
//! A document is put on the record once, with its kind and, if it is about
//! money, its amount. An approver names only the document; the record
//! supplies the rest and decides the route.
//!
//! - **Unconditional** (`MayApprove`, `approve_document`): the actor is
//!   consulted via `Term::Actor`, no grant means rejection, the `Approval`
//!   records the proposing actor, one actor cannot act as another, and
//!   revocation keeps history.
//!
//! - **Quantitative** (`ApprovalLimit`, `approve_within_limit`): the same, plus
//!   amount-against-limit, the boundary, stacked grants, per-doc-type scope,
//!   and an ill-typed limit.
//!
//! - **The route**: a document with an amount cannot take the unconditional
//!   route, and no sequence of submissions can give a document an amount, or
//!   take one away, after it is registered.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::OnceLock;

use common::{Example, claim_instance, dec, has_claim, subj};
use morpholog_core::{EvalError, EvalValue, State, Subject};
use morpholog_examples::approval_controls;

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&approval_controls::program()))
}

fn grant_authority(state: State, actor: &str, doc_type: &str) -> State {
    ex().must_accept(
        &approval_controls::grant_approval_authority(),
        vec![subj(actor), subj(doc_type)],
        state,
    )
}

fn grant_limit(state: State, actor: &str, doc_type: &str, limit: i64) -> State {
    ex().must_accept(
        &approval_controls::grant_approval_limit(),
        vec![subj(actor), subj(doc_type), dec(limit)],
        state,
    )
}

fn submit(state: State, doc_id: &str, doc_type: &str) -> State {
    ex().must_accept(
        &approval_controls::submit_document(),
        vec![subj(doc_id), subj(doc_type)],
        state,
    )
}

fn submit_amount(state: State, doc_id: &str, doc_type: &str, amount: i64) -> State {
    ex().must_accept(
        &approval_controls::submit_monetary_document(),
        vec![subj(doc_id), subj(doc_type), dec(amount)],
        state,
    )
}

fn approve(state: State, doc_id: &str, actor: &str) -> State {
    ex().must_accept_as(
        &approval_controls::approve_document(),
        vec![subj(doc_id)],
        actor,
        state,
    )
}

fn approve_within(state: State, doc_id: &str, actor: &str) -> State {
    ex().must_accept_as(
        &approval_controls::approve_within_limit(),
        vec![subj(doc_id)],
        actor,
        state,
    )
}

fn refuse_approve(state: &State, doc_id: &str, actor: &str) {
    ex().must_reject_as(
        &approval_controls::approve_document(),
        vec![subj(doc_id)],
        actor,
        state,
    );
}

fn refuse_within(state: &State, doc_id: &str, actor: &str) {
    ex().must_reject_as(
        &approval_controls::approve_within_limit(),
        vec![subj(doc_id)],
        actor,
        state,
    );
}

/// A vendor onboarding document, which has no amount.
fn onboarding() -> State {
    submit(State::default(), "doc_001", "vendor_onboarding")
}

// ============================================================
// Unconditional authority
// ============================================================

#[test]
fn approve_without_authority_is_rejected_at_require() {
    let reason = ex().must_reject_as(
        &approval_controls::approve_document(),
        vec![subj("doc_001")],
        "jordan",
        &onboarding(),
    );
    assert!(
        reason.to_string().contains("MayApprove"),
        "got reason: {reason}"
    );
}

#[test]
fn approve_with_authority_carries_proposing_actor_on_asserted_claim() {
    let pre = grant_authority(onboarding(), "jordan", "vendor_onboarding");
    let post = approve(pre, "doc_001", "jordan");
    assert!(
        has_claim(
            &post,
            "Approval",
            &[subj("doc_001"), subj("vendor_onboarding"), subj("jordan")],
        ),
        "Approval must carry the proposing actor and the recorded kind",
    );
}

#[test]
fn approve_uses_proposing_actor_not_a_caller_parameter() {
    // jordan has authority; alice does not. alice cannot approve on
    // jordan's behalf because `actor` is the proposing actor, not a
    // parameter the caller controls.
    let pre = grant_authority(onboarding(), "jordan", "vendor_onboarding");
    refuse_approve(&pre, "doc_001", "alice");
}

#[test]
fn an_unregistered_document_cannot_be_approved() {
    let pre = grant_authority(State::default(), "jordan", "vendor_onboarding");
    refuse_approve(&pre, "doc_001", "jordan");
}

#[test]
fn revoked_authority_blocks_future_but_preserves_past() {
    let pre = grant_authority(onboarding(), "jordan", "vendor_onboarding");
    let after_approval = approve(pre, "doc_001", "jordan");
    let after_revoke = ex().must_accept(
        &approval_controls::revoke_approval_authority(),
        vec![subj("jordan"), subj("vendor_onboarding")],
        after_approval,
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
    let after_revoke = submit(after_revoke, "doc_002", "vendor_onboarding");
    refuse_approve(&after_revoke, "doc_002", "jordan");
}

// ============================================================
// Quantitative authority
// ============================================================

#[test]
fn limit_approval_without_grant_is_rejected() {
    let pre = submit_amount(State::default(), "inv_001", "invoice", 100);
    refuse_within(&pre, "inv_001", "jordan");
}

#[test]
fn limit_approval_under_limit_commits_with_actor_and_amount() {
    let pre = grant_limit(State::default(), "jordan", "invoice", 1000);
    let pre = submit_amount(pre, "inv_001", "invoice", 750);
    let post = approve_within(pre, "inv_001", "jordan");
    assert!(has_claim(
        &post,
        "LimitedApproval",
        &[subj("inv_001"), subj("invoice"), dec(750), subj("jordan")],
    ));
}

#[test]
fn limit_approval_exactly_at_limit_commits() {
    // The ceiling is inclusive.
    let pre = grant_limit(State::default(), "jordan", "invoice", 1000);
    let pre = submit_amount(pre, "inv_at_limit", "invoice", 1000);
    let post = approve_within(pre, "inv_at_limit", "jordan");
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
    let pre = submit_amount(pre, "inv_over", "invoice", 1001);
    refuse_within(&pre, "inv_over", "jordan");
}

#[test]
fn limit_grant_is_per_actor_and_per_doc_type() {
    // jordan has an invoice limit but not a contract limit.
    let pre = grant_limit(State::default(), "jordan", "invoice", 1000);
    let pre = submit_amount(pre, "inv_alice", "invoice", 10);
    let pre = submit_amount(pre, "ct_001", "contract", 10);

    // alice cannot approve under jordan's invoice limit.
    refuse_within(&pre, "inv_alice", "alice");
    // jordan cannot use her invoice limit for contracts.
    refuse_within(&pre, "ct_001", "jordan");
}

#[test]
fn multiple_grants_take_the_satisfying_one() {
    // jordan holds two layered grants: 500 and 5000. An approval at
    // 3000 satisfies the second but not the first; the And + binding
    // shape of the require finds *some* satisfying limit.
    let pre = grant_limit(State::default(), "jordan", "invoice", 500);
    let pre = grant_limit(pre, "jordan", "invoice", 5000);
    let pre = submit_amount(pre, "inv_3k", "invoice", 3000);
    let post = approve_within(pre, "inv_3k", "jordan");
    assert!(has_claim(
        &post,
        "LimitedApproval",
        &[subj("inv_3k"), subj("invoice"), dec(3000), subj("jordan")],
    ));
}

#[test]
fn revoking_a_limit_blocks_future_but_preserves_past() {
    let pre = grant_limit(State::default(), "jordan", "invoice", 1000);
    let pre = submit_amount(pre, "inv_001", "invoice", 800);
    let pre = submit_amount(pre, "inv_002", "invoice", 500);
    let after_approval = approve_within(pre, "inv_001", "jordan");
    let after_revoke = ex().must_accept(
        &approval_controls::revoke_approval_limit(),
        vec![subj("jordan"), subj("invoice"), dec(1000)],
        after_approval,
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
    refuse_within(&after_revoke, "inv_002", "jordan");
}

#[test]
fn non_decimal_limit_in_authority_claim_surfaces_as_type_mismatch() {
    // An ill-typed claim in state is corruption, not a business rejection:
    // a non-decimal limit makes `amount <= limit` raise
    // `EvalError::TypeMismatch`.
    let mut claims = submit_amount(State::default(), "inv_001", "invoice", 100)
        .claims()
        .to_vec();
    claims.push(claim_instance(
        "ApprovalLimit",
        &[
            subj("jordan"),
            subj("invoice"),
            EvalValue::Subject("not_a_decimal".into()),
        ],
    ));
    let pre = State::from_claims(claims);

    let mut transition = common::test_transition(
        &approval_controls::approve_within_limit(),
        vec![subj("inv_001")],
    );
    transition.actor = Subject::from("jordan");

    let err = morpholog_test_support::propose(
        &approval_controls::program(),
        &transition,
        &pre,
        &mut morpholog_test_support::fresh(),
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
// The route: the record decides it, once
// ============================================================

#[test]
fn a_document_with_an_amount_cannot_take_the_unconditional_route() {
    // jordan holds both kinds of authority for invoices. The £4,300
    // invoice is over the ceiling, and the sign-off route is closed to it
    // because it carries an amount.
    let pre = grant_authority(State::default(), "jordan", "invoice");
    let pre = grant_limit(pre, "jordan", "invoice", 1000);
    let pre = submit_amount(pre, "inv_4300", "invoice", 4300);
    refuse_approve(&pre, "inv_4300", "jordan");
    refuse_within(&pre, "inv_4300", "jordan");
}

#[test]
fn a_document_without_an_amount_cannot_take_the_limited_route() {
    let pre = grant_limit(onboarding(), "jordan", "vendor_onboarding", 1000);
    refuse_within(&pre, "doc_001", "jordan");
}

#[test]
fn an_amount_cannot_be_added_after_an_unconditional_approval() {
    // Registered without an amount, approved by sign-off, then submitted
    // again with the amount the invoice really carries.
    let pre = grant_authority(State::default(), "jordan", "invoice");
    let pre = submit(pre, "inv_42", "invoice");
    let approved = approve(pre, "inv_42", "jordan");
    ex().must_reject(
        &approval_controls::submit_monetary_document(),
        vec![subj("inv_42"), subj("invoice"), dec(4300)],
        &approved,
    );
    assert!(
        !approved
            .claims()
            .iter()
            .any(|c| c.predicate.as_str() == "DocumentAmount")
    );
}

#[test]
fn a_monetary_document_cannot_be_registered_again_without_its_amount() {
    let pre = submit_amount(State::default(), "inv_42", "invoice", 4300);
    for doc_type in ["invoice", "vendor_onboarding"] {
        ex().must_reject(
            &approval_controls::submit_document(),
            vec![subj("inv_42"), subj(doc_type)],
            &pre,
        );
    }
}

#[test]
fn an_amount_must_be_positive() {
    for amount in [0, -1_000_000] {
        ex().must_reject(
            &approval_controls::submit_monetary_document(),
            vec![subj("inv_neg"), subj("invoice"), dec(amount)],
            &State::default(),
        );
    }
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

    // Nothing asserts an unknown predicate, so the answer is an empty list,
    // not an error.
    assert!(transformations_asserting(&program, "NoSuchPredicate").is_empty());
}
