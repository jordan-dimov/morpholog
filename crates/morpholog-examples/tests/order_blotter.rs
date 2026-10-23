//! Integration tests for the order blotter example
//! (`examples/24_order_blotter/`).
//!
//! A cancel recorded by mistake on an append-only blotter is marked in
//! error, never removed; the fill it blocked is then admitted; the mark
//! reaches only its own cancel's order, and a breach on one order blocks
//! no act on another.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::OnceLock;

use common::{Example, claim_instance, dec_str, has_claim, subj};
use morpholog_core::ir_builder::{assert_, params, transformation, var};
use morpholog_core::{Bounding, EvalError, ImpactPlan, Outcome, RejectionReason, State};
use morpholog_examples::order_blotter;

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&order_blotter::program()))
}

const DESK: &str = "desk";
const CORRECTOR: &str = "middle_office";
const REASON: &str = "cancel_sent_after_fill";

fn placed(state: State, order: &str, book: &str) -> State {
    ex().must_accept(
        &order_blotter::place(),
        vec![subj(order), subj(book), dec_str("100")],
        state,
    )
}

fn cancelled(state: State, order: &str, cancel_id: &str) -> State {
    ex().must_accept(
        &order_blotter::cancel(),
        vec![subj(order), subj(cancel_id)],
        state,
    )
}

fn fill(order: &str, fill_id: &str, qty: &str, state: &State) -> Result<Outcome, EvalError> {
    ex().propose_as(
        &order_blotter::fill(),
        vec![subj(order), subj(fill_id), dec_str(qty)],
        DESK,
        state,
    )
}

fn erratum(cancel_id: &str, actor: &str, state: &State) -> Result<Outcome, EvalError> {
    ex().propose_as(
        &order_blotter::record_erratum(),
        vec![subj(cancel_id), subj(REASON)],
        actor,
        state,
    )
}

fn admitted(outcome: Result<Outcome, EvalError>) -> State {
    match outcome {
        Ok(Outcome::Accepted {
            candidate_state, ..
        }) => candidate_state,
        other => panic!("expected admission: {other:?}"),
    }
}

fn refused(outcome: Result<Outcome, EvalError>) -> RejectionReason {
    match outcome {
        Ok(Outcome::Rejected { reason }) => reason,
        other => panic!("expected refusal: {other:?}"),
    }
}

fn invariant_named(reason: &RejectionReason, rule: &str) -> bool {
    matches!(reason, RejectionReason::Invariant { name, .. } if name.as_str() == rule)
}

/// Book A with its corrector appointed, order o1 placed and wrongly
/// cancelled as c1.
fn the_wrong_cancel() -> State {
    let state = ex().must_accept(
        &order_blotter::appoint_corrector(),
        vec![subj("book_a"), subj(CORRECTOR)],
        State::from_claims(vec![]),
    );
    let state = placed(state, "o1", "book_a");
    cancelled(state, "o1", "c1")
}

/// The incident: the fill is refused while the cancel stands, the
/// erratum is admitted only by the appointed corrector, the fill then
/// goes through, and the cancel is still on the record.
#[test]
fn a_cancel_in_error_is_marked_not_removed_and_the_fill_follows() {
    let state = the_wrong_cancel();
    let blocked = refused(fill("o1", "f1", "100", &state));
    assert!(
        invariant_named(&blocked, "a_cancel_closes_only_an_unfilled_order"),
        "{blocked:?}"
    );

    let stranger = refused(erratum("c1", "stranger", &state));
    assert!(
        matches!(stranger, RejectionReason::Require { .. }),
        "{stranger:?}"
    );

    let state = admitted(erratum("c1", CORRECTOR, &state));
    let state = admitted(fill("o1", "f1", "100", &state));
    assert!(has_claim(&state, "Cancel", &[subj("o1"), subj("c1")]));
    assert!(has_claim(
        &state,
        "RecordedInError",
        &[subj("c1"), subj(REASON)]
    ));
    assert!(has_claim(
        &state,
        "Fill",
        &[subj("o1"), subj("f1"), dec_str("100")]
    ));
}

/// One erratum per line: a second, with another reason, is refused by
/// the declared uniqueness.
#[test]
fn an_erratum_is_recorded_once() {
    let state = admitted(erratum("c1", CORRECTOR, &the_wrong_cancel()));
    let reason = ex().must_reject_as(
        &order_blotter::record_erratum(),
        vec![subj("c1"), subj("another_reason")],
        CORRECTOR,
        &state,
    );
    assert!(
        matches!(&reason, RejectionReason::Invariant { name, .. } if name.as_str().starts_with("recorded_in_error_unique_by")),
        "{reason:?}"
    );
}

/// The act reads the cancel off the record, so an unknown cancel finds
/// nothing to bind; a bare admit of the mark is refused by provenance.
#[test]
fn an_erratum_names_a_cancel_that_exists() {
    let state = the_wrong_cancel();
    let unknown = refused(erratum("c9", CORRECTOR, &state));
    assert!(
        matches!(unknown, RejectionReason::BindNone { .. }),
        "{unknown:?}"
    );

    let bare = transformation(
        "mark_only",
        params(&["cancel_id", "reason"]),
        vec![assert_(
            "RecordedInError",
            vec![var("cancel_id"), var("reason")],
        )],
    );
    let reason = ex().must_reject(&bare, vec![subj("c9"), subj(REASON)], &state);
    assert!(
        invariant_named(&reason, "erratum_names_a_cancel"),
        "{reason:?}"
    );
}

#[test]
fn a_cancel_on_a_filled_order_is_refused() {
    let state = placed(the_wrong_cancel(), "o2", "book_a");
    let state = admitted(fill("o2", "f2", "100", &state));
    let reason = ex().must_reject(
        &order_blotter::cancel(),
        vec![subj("o2"), subj("c2")],
        &state,
    );
    assert!(
        invariant_named(&reason, "a_cancel_closes_only_an_unfilled_order"),
        "{reason:?}"
    );
}

/// The corrector of book A cannot mark a cancel on book B's order: the
/// book is read off the record, not named by the caller.
#[test]
fn authority_follows_the_cancels_own_book() {
    let state = placed(the_wrong_cancel(), "o_b", "book_b");
    let state = cancelled(state, "o_b", "c_b");
    let reason = refused(erratum("c_b", CORRECTOR, &state));
    assert!(
        matches!(reason, RejectionReason::Require { .. }),
        "{reason:?}"
    );
}

#[test]
fn a_books_corrector_is_appointed_once() {
    let reason = ex().must_reject_as(
        &order_blotter::appoint_corrector(),
        vec![subj("book_a"), subj("stranger")],
        "stranger",
        &the_wrong_cancel(),
    );
    assert!(
        matches!(reason, RejectionReason::Require { .. }),
        "{reason:?}"
    );
}

/// The mark reaches one cancel. Another order's cancel still stands
/// after it, and a genuine cancel on an unfilled order is admitted.
#[test]
fn an_erratum_changes_one_cancels_standing_only() {
    let state = placed(the_wrong_cancel(), "o2", "book_a");
    let state = cancelled(state, "o2", "c2");
    let state = admitted(erratum("c1", CORRECTOR, &state));
    let still_cancelled = refused(fill("o2", "f2", "100", &state));
    assert!(
        invariant_named(&still_cancelled, "a_cancel_closes_only_an_unfilled_order"),
        "{still_cancelled:?}"
    );
    let state = placed(state, "o3", "book_a");
    cancelled(state, "o3", "c3");
}

/// Uniqueness over standing cancels, not recorded ones: a second cancel
/// is refused while the first stands, and admitted once the first is
/// marked, with both still on the record.
#[test]
fn marking_a_cancel_frees_the_order_for_a_genuine_one() {
    let state = the_wrong_cancel();
    let reason = ex().must_reject(
        &order_blotter::cancel(),
        vec![subj("o1"), subj("c2")],
        &state,
    );
    assert!(
        invariant_named(&reason, "one_standing_cancel_per_order"),
        "{reason:?}"
    );
    let state = admitted(erratum("c1", CORRECTOR, &state));
    let state = cancelled(state, "o1", "c2");
    assert!(has_claim(&state, "Cancel", &[subj("o1"), subj("c1")]));
    assert!(has_claim(&state, "Cancel", &[subj("o1"), subj("c2")]));
    let still_closed = refused(fill("o1", "f1", "100", &state));
    assert!(
        invariant_named(&still_closed, "a_cancel_closes_only_an_unfilled_order"),
        "c2 stands: {still_closed:?}"
    );
}

/// A quantity that is not positive is refused at the gate: a negative
/// fill could make room past the order for a later one.
#[test]
fn quantities_are_positive() {
    let reason = ex().must_reject(
        &order_blotter::place(),
        vec![subj("o9"), subj("book_a"), dec_str("0")],
        &State::from_claims(vec![]),
    );
    assert!(
        matches!(reason, RejectionReason::Require { .. }),
        "{reason:?}"
    );
    let reason = refused(fill("o1", "f1", "-100", &the_wrong_cancel()));
    assert!(
        matches!(reason, RejectionReason::Require { .. }),
        "{reason:?}"
    );
}

/// A breach history left behind, a fill beside a standing cancel on one
/// order, blocks nothing on another order: every rule is checked case
/// by case, and the breached order itself stays closed to change until
/// its cancel is marked.
#[test]
fn a_breach_on_one_order_blocks_no_act_on_another() {
    let mut claims = the_wrong_cancel().claims().to_vec();
    claims.push(claim_instance(
        "Order",
        &[subj("z"), subj("book_a"), dec_str("100")],
    ));
    claims.push(claim_instance("Cancel", &[subj("z"), subj("cz")]));
    claims.push(claim_instance(
        "Fill",
        &[subj("z"), subj("fz"), dec_str("50")],
    ));
    let dirty = State::from_claims(claims);

    let state = admitted(erratum("c1", CORRECTOR, &dirty));
    let state = admitted(fill("o1", "f1", "100", &state));
    let state = placed(state, "o2", "book_a");
    cancelled(state, "o2", "c2");

    let touched = refused(fill("z", "fz2", "50", &dirty));
    assert!(
        invariant_named(&touched, "a_cancel_closes_only_an_unfilled_order"),
        "{touched:?}"
    );
    let repaired = admitted(erratum("cz", CORRECTOR, &dirty));
    admitted(fill("z", "fz2", "50", &repaired));
}

/// What the isolation above rests on: every rule's plan is case-bound.
#[test]
fn every_rule_is_checked_case_by_case() {
    let program = order_blotter::program();
    for inv in &program.invariants {
        let plan = ImpactPlan::with_definitions(inv, &program.definitions);
        assert!(
            matches!(plan.bounding(), Bounding::Bound),
            "{}: {:?}",
            inv.name,
            plan.bounding()
        );
    }
}
