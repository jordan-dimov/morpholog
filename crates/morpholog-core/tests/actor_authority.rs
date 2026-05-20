//! Integration tests for the actor authority example
//! (`examples/06_actor_authority/`).
//!
//! Proves: a transformation can gate admission on the proposing
//! actor's authority via `require` + `Term::Actor`; the asserted
//! claim carries the proposing actor as its third argument; authority
//! revocation prevents future approvals while preserving historical
//! ones (require-vs-invariant); `Term::Actor` referenced inside an
//! invariant body surfaces as `EvalError::UnboundActor`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{has_claim, must_accept, must_accept_as, propose_as, subj};
use morpholog_core::examples::actor_authority;
use morpholog_core::{EvalError, Expr, Invariant, Outcome, State, Term, eval_invariant};

fn empty_invariants() -> Vec<Invariant> {
    actor_authority::all_invariants()
}

/// Grant `actor` authority to approve docs of `doc_type`, returning
/// the resulting state.
fn grant(state: State, actor: &str, doc_type: &str) -> State {
    must_accept(
        &actor_authority::grant_approval_authority(),
        vec![subj(actor), subj(doc_type)],
        state,
        &empty_invariants(),
    )
}

// ============================================================
// Tests
// ============================================================

#[test]
fn approve_without_authority_is_rejected_at_require() {
    let pre = State::default();
    let outcome = propose_as(
        &actor_authority::approve_document(),
        vec![subj("doc_001"), subj("invoice")],
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
fn approve_with_authority_admits_approval_carrying_proposing_actor() {
    let pre = grant(State::default(), "jordan", "invoice");

    let post = must_accept_as(
        &actor_authority::approve_document(),
        vec![subj("doc_001"), subj("invoice")],
        subj("jordan"),
        pre,
        &empty_invariants(),
    );

    assert!(
        has_claim(
            &post,
            "Approval",
            &[subj("doc_001"), subj("invoice"), subj("jordan")],
        ),
        "Approval claim should carry the proposing actor as its third arg",
    );
    // MayApprove is preserved (approve does not retract authority).
    assert!(has_claim(
        &post,
        "MayApprove",
        &[subj("jordan"), subj("invoice")],
    ));
}

#[test]
fn approve_uses_proposing_actor_not_a_caller_parameter() {
    // jordan has authority for invoices; alice does not.
    let pre = grant(State::default(), "jordan", "invoice");

    // alice proposes the approval (same args as the test above).
    // require MayApprove($actor, doc_type) is evaluated against
    // $actor = alice, not jordan, even though jordan is present in
    // the pre-state. The require fails.
    let outcome = propose_as(
        &actor_authority::approve_document(),
        vec![subj("doc_001"), subj("invoice")],
        subj("alice"),
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
    // jordan approves under authority.
    let pre = grant(State::default(), "jordan", "invoice");
    let after_approval = must_accept_as(
        &actor_authority::approve_document(),
        vec![subj("doc_001"), subj("invoice")],
        subj("jordan"),
        pre,
        &empty_invariants(),
    );
    assert!(has_claim(
        &after_approval,
        "Approval",
        &[subj("doc_001"), subj("invoice"), subj("jordan")],
    ));

    // Authority revoked.
    let after_revoke = must_accept(
        &actor_authority::revoke_approval_authority(),
        vec![subj("jordan"), subj("invoice")],
        after_approval,
        &empty_invariants(),
    );

    // The prior approval survives. This is the require-vs-invariant
    // payoff: no invariant ties Approval to live MayApprove, so
    // revocation does not cascade-retract historical decisions.
    assert!(
        has_claim(
            &after_revoke,
            "Approval",
            &[subj("doc_001"), subj("invoice"), subj("jordan")],
        ),
        "historical Approval must survive authority revocation",
    );
    // MayApprove is gone.
    assert!(!has_claim(
        &after_revoke,
        "MayApprove",
        &[subj("jordan"), subj("invoice")],
    ));

    // A new approval attempt by jordan is now rejected.
    let outcome = propose_as(
        &actor_authority::approve_document(),
        vec![subj("doc_002"), subj("invoice")],
        subj("jordan"),
        &after_revoke,
        &empty_invariants(),
    )
    .expect("propose should not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

// ============================================================
// Kernel-level pin: Term::Actor outside a transition raises
// EvalError::UnboundActor. This is what makes the require-vs-
// invariant doctrine enforceable instead of conventional - an
// invariant author who reaches for `Term::Actor` gets a clear
// error instead of silently undefined behaviour.
// ============================================================

#[test]
fn term_actor_in_invariant_body_surfaces_as_unbound_actor() {
    // A nonsense invariant that consults Term::Actor. It should not
    // be possible to evaluate this against any state, because
    // invariants have no transition in scope.
    let inv = Invariant {
        name: "improperly_uses_actor".to_string(),
        version: 1,
        body: Expr::Claim {
            predicate: "AnyPredicate".to_string(),
            args: vec![Term::Actor],
        },
    };
    let err = eval_invariant(&inv, &State::default()).expect_err("must error");
    assert!(
        matches!(err, EvalError::UnboundActor),
        "expected UnboundActor, got {err:?}",
    );
}
