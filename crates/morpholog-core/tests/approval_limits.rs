//! Integration tests for the approval limits example
//! (`examples/07_approval_limits/`).
//!
//! Proves: a transformation can require *quantitative* authority -
//! the proposing actor must hold an `ApprovalLimit` claim whose
//! limit is greater than or equal to the proposed amount; the asserted
//! `LimitedApproval` carries the proposing actor and the exact amount
//! that was authorised; revoking a specific limit prevents future
//! approvals against it but preserves historical ones; `Expr::Le`'s
//! at-the-boundary equality is inclusive.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{dec, has_claim, must_accept, must_accept_as, propose_as, subj};
use morpholog_core::examples::approval_limits;
use morpholog_core::{Invariant, Outcome, State};

fn empty_invariants() -> Vec<Invariant> {
    approval_limits::all_invariants()
}

fn grant(state: State, actor: &str, doc_type: &str, limit: i64) -> State {
    must_accept(
        &approval_limits::grant_approval_limit(),
        vec![subj(actor), subj(doc_type), dec(limit)],
        state,
        &empty_invariants(),
    )
}

// ============================================================
// Tests
// ============================================================

#[test]
fn approval_without_limit_grant_is_rejected() {
    let pre = State::default();
    let outcome = propose_as(
        &approval_limits::approve_within_limit(),
        vec![subj("inv_001"), subj("invoice"), dec(100)],
        subj("jordan"),
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
fn approval_under_limit_commits_with_actor_and_amount() {
    let pre = grant(State::default(), "jordan", "invoice", 1000);
    let post = must_accept_as(
        &approval_limits::approve_within_limit(),
        vec![subj("inv_001"), subj("invoice"), dec(750)],
        subj("jordan"),
        pre,
        &empty_invariants(),
    );
    assert!(
        has_claim(
            &post,
            "LimitedApproval",
            &[subj("inv_001"), subj("invoice"), dec(750), subj("jordan")],
        ),
        "LimitedApproval must carry doc_id, doc_type, amount, and proposing actor",
    );
    // Authority claim is preserved (approve does not retract it).
    assert!(has_claim(
        &post,
        "ApprovalLimit",
        &[subj("jordan"), subj("invoice"), dec(1000)],
    ));
}

#[test]
fn approval_exactly_at_limit_commits() {
    // Le is inclusive at the boundary.
    let pre = grant(State::default(), "jordan", "invoice", 1000);
    let post = must_accept_as(
        &approval_limits::approve_within_limit(),
        vec![subj("inv_at_limit"), subj("invoice"), dec(1000)],
        subj("jordan"),
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
fn approval_above_limit_is_rejected() {
    let pre = grant(State::default(), "jordan", "invoice", 1000);
    let outcome = propose_as(
        &approval_limits::approve_within_limit(),
        vec![subj("inv_over"), subj("invoice"), dec(1001)],
        subj("jordan"),
        &pre,
        &empty_invariants(),
    )
    .expect("propose should not error");
    let Outcome::Rejected { .. } = outcome else {
        panic!("amount above limit must be rejected; got {outcome:?}");
    };
}

#[test]
fn limit_grant_is_per_actor() {
    // jordan has a limit; alice does not. alice cannot approve even
    // a small amount.
    let pre = grant(State::default(), "jordan", "invoice", 1000);
    let outcome = propose_as(
        &approval_limits::approve_within_limit(),
        vec![subj("inv_alice"), subj("invoice"), dec(10)],
        subj("alice"),
        &pre,
        &empty_invariants(),
    )
    .expect("propose should not error");
    let Outcome::Rejected { .. } = outcome else {
        panic!("alice has no limit; rejection expected; got {outcome:?}");
    };
}

#[test]
fn limit_grant_is_per_doc_type() {
    // jordan has an invoice limit but not a contract limit.
    let pre = grant(State::default(), "jordan", "invoice", 1000);
    let outcome = propose_as(
        &approval_limits::approve_within_limit(),
        vec![subj("ct_001"), subj("contract"), dec(10)],
        subj("jordan"),
        &pre,
        &empty_invariants(),
    )
    .expect("propose should not error");
    let Outcome::Rejected { .. } = outcome else {
        panic!("no contract limit; rejection expected; got {outcome:?}");
    };
}

#[test]
fn multiple_grants_take_the_satisfying_one() {
    // jordan holds two layered grants: 500 and 5000. An approval at
    // 3000 satisfies the second but not the first; the And + Exists
    // shape of the require finds the satisfying combination.
    let pre = grant(State::default(), "jordan", "invoice", 500);
    let pre = grant(pre, "jordan", "invoice", 5000);
    let post = must_accept_as(
        &approval_limits::approve_within_limit(),
        vec![subj("inv_3k"), subj("invoice"), dec(3000)],
        subj("jordan"),
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
    let pre = grant(State::default(), "jordan", "invoice", 1000);
    let after_approval = must_accept_as(
        &approval_limits::approve_within_limit(),
        vec![subj("inv_001"), subj("invoice"), dec(800)],
        subj("jordan"),
        pre,
        &empty_invariants(),
    );

    // Revoke the specific (jordan, invoice, 1000) grant.
    let after_revoke = must_accept(
        &approval_limits::revoke_approval_limit(),
        vec![subj("jordan"), subj("invoice"), dec(1000)],
        after_approval,
        &empty_invariants(),
    );

    // Historical LimitedApproval survives.
    assert!(has_claim(
        &after_revoke,
        "LimitedApproval",
        &[subj("inv_001"), subj("invoice"), dec(800), subj("jordan")],
    ));
    // ApprovalLimit is gone.
    assert!(!has_claim(
        &after_revoke,
        "ApprovalLimit",
        &[subj("jordan"), subj("invoice"), dec(1000)],
    ));
    // Future approval against the same shape is rejected.
    let outcome = propose_as(
        &approval_limits::approve_within_limit(),
        vec![subj("inv_002"), subj("invoice"), dec(500)],
        subj("jordan"),
        &after_revoke,
        &empty_invariants(),
    )
    .expect("propose should not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}
