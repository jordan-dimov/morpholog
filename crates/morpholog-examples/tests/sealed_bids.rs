//! The sealed-bids example: a reveal is held to the commitment sealed
//! before the close, closure is a claim and not a clock reading, and
//! every claim of the protocol stays once admitted.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{dec_str, subj, ts};
use morpholog_core::{EvalValue, Outcome, RejectionReason, State, digest};
use morpholog_examples::sealed_bids;
use morpholog_test_support::{enumerate_derived, fresh, propose, refused_by, transition_at};

const BEFORE: &str = "2026-06-01T09:00:00Z";
const CLOSES: &str = "2026-06-01T12:00:00Z";
const AFTER: &str = "2026-06-01T12:00:01Z";

fn commitment(lot: &str, bidder: &str, amount: &str, salt: &str) -> EvalValue {
    subj(&digest::digest(&[
        subj("sealed_bid_v1"),
        subj(lot),
        subj(bidder),
        dec_str(amount),
        subj(salt),
    ]))
}

/// Decide `t` as `actor` at `at`.
fn decide(
    t: &str,
    args: Vec<EvalValue>,
    actor: &str,
    at: &str,
    state: &State,
) -> Result<Outcome, morpholog_core::EvalError> {
    let p = sealed_bids::program();
    let t = p.transformation(t).unwrap();
    propose(
        &p,
        &transition_at(t, args, morpholog_core::Subject::from(actor), at),
        state,
        &mut fresh(),
    )
}

fn accepted(outcome: Result<Outcome, morpholog_core::EvalError>) -> State {
    match outcome {
        Ok(Outcome::Accepted {
            candidate_state, ..
        }) => candidate_state,
        other => panic!("expected acceptance, got {other:?}"),
    }
}

fn refused(outcome: Result<Outcome, morpholog_core::EvalError>, rule: &str) {
    match outcome {
        Ok(Outcome::Rejected { reason }) => refused_by(&reason, rule),
        other => panic!("expected a refusal by {rule}, got {other:?}"),
    }
}

/// A lot offered and sealed by alice (100 under salt s1) and bob (90
/// under s2), then closed.
fn closed_lot() -> State {
    let offered = accepted(decide(
        "offer",
        vec![subj("lot"), ts(CLOSES)],
        "auctioneer",
        BEFORE,
        &State::default(),
    ));
    let alice = accepted(decide(
        "seal",
        vec![subj("lot"), commitment("lot", "alice", "100", "s1")],
        "alice",
        BEFORE,
        &offered,
    ));
    let bob = accepted(decide(
        "seal",
        vec![subj("lot"), commitment("lot", "bob", "90", "s2")],
        "bob",
        BEFORE,
        &alice,
    ));
    accepted(decide(
        "close",
        vec![subj("lot")],
        "auctioneer",
        AFTER,
        &bob,
    ))
}

#[test]
fn a_reveal_that_matches_its_envelope_is_admitted_and_leads() {
    let state = closed_lot();
    let revealed = accepted(decide(
        "reveal",
        vec![subj("lot"), dec_str("100"), subj("s1")],
        "alice",
        AFTER,
        &state,
    ));
    let revealed = accepted(decide(
        "reveal",
        vec![subj("lot"), dec_str("90"), subj("s2")],
        "bob",
        AFTER,
        &revealed,
    ));
    let p = sealed_bids::program();
    let leading = p
        .derived_claims
        .iter()
        .find(|d| d.predicate.as_str() == "LeadingBid")
        .unwrap();
    let rows = enumerate_derived(&p, leading, &revealed).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0].args,
        vec![subj("lot"), subj("alice"), dec_str("100")]
    );
}

/// The scale a bid is revealed at is not part of the bid: `100.0`
/// opens an envelope sealed over `100`.
#[test]
fn a_reveal_at_another_scale_opens_the_same_envelope() {
    let state = closed_lot();
    accepted(decide(
        "reveal",
        vec![subj("lot"), dec_str("100.0"), subj("s1")],
        "alice",
        AFTER,
        &state,
    ));
}

#[test]
fn a_reveal_with_the_wrong_amount_or_salt_is_refused_by_the_rule() {
    let state = closed_lot();
    for (amount, salt) in [("101", "s1"), ("100", "s9")] {
        let outcome = decide(
            "reveal",
            vec![subj("lot"), dec_str(amount), subj(salt)],
            "alice",
            AFTER,
            &state,
        );
        match &outcome {
            Ok(Outcome::Rejected {
                reason: RejectionReason::Invariant { name, witness, .. },
            }) => {
                assert_eq!(name.as_str(), "a_revealed_bid_hashes_to_its_commitment");
                assert!(
                    witness
                        .iter()
                        .any(|w| w.var.as_str() == "bidder" && w.value == subj("alice")),
                    "{witness:?}"
                );
            }
            other => panic!("{other:?}"),
        }
    }
}

/// A clock is not a seal: a seal decided before the deadline is refused
/// once the lot is closed, and a seal after the deadline is refused by
/// the time gate while the lot is still open.
#[test]
fn sealing_is_gated_by_the_deadline_and_by_closure() {
    let offered = accepted(decide(
        "offer",
        vec![subj("lot"), ts(CLOSES)],
        "auctioneer",
        BEFORE,
        &State::default(),
    ));
    refused(
        decide(
            "seal",
            vec![subj("lot"), commitment("lot", "carol", "80", "s3")],
            "carol",
            AFTER,
            &offered,
        ),
        "before_the_deadline",
    );
    refused(
        decide("close", vec![subj("lot")], "auctioneer", BEFORE, &offered),
        "deadline_passed",
    );
    let closed = closed_lot();
    refused(
        decide(
            "seal",
            vec![subj("lot"), commitment("lot", "carol", "80", "s3")],
            "carol",
            BEFORE,
            &closed,
        ),
        "still_open",
    );
    refused(
        decide(
            "reveal",
            vec![subj("lot"), dec_str("100"), subj("s1")],
            "alice",
            AFTER,
            &offered,
        ),
        "closed",
    );
    let not_the_auctioneer = decide("close", vec![subj("lot")], "bob", AFTER, &offered);
    assert!(
        matches!(&not_the_auctioneer, Ok(Outcome::Rejected { reason }) if reason.to_string().contains("offered")),
        "{not_the_auctioneer:?}"
    );
}

/// An envelope is one per bidder per lot and, once admitted, stays:
/// every claim of the protocol is append only, so no act of the
/// programme can withdraw one. A reveal without an envelope is refused
/// by the rule, whoever asks.
#[test]
fn an_envelope_is_one_per_bidder_and_never_withdrawn() {
    let offered = accepted(decide(
        "offer",
        vec![subj("lot"), ts(CLOSES)],
        "auctioneer",
        BEFORE,
        &State::default(),
    ));
    let sealed = accepted(decide(
        "seal",
        vec![subj("lot"), commitment("lot", "alice", "100", "s1")],
        "alice",
        BEFORE,
        &offered,
    ));
    let again = decide(
        "seal",
        vec![subj("lot"), commitment("lot", "alice", "120", "s1")],
        "alice",
        BEFORE,
        &sealed,
    );
    refused(again, "sealed_unique_by_lot_bidder");
    let p = sealed_bids::program();
    for name in ["Lot", "Sealed", "Closed", "Bid"] {
        let decl = p
            .predicates
            .iter()
            .find(|d| d.name.as_str() == name)
            .unwrap();
        assert!(
            decl.disciplines
                .iter()
                .any(|d| matches!(d, morpholog_core::Discipline::AppendOnly)),
            "{name} is append only"
        );
    }
    refused(
        decide(
            "reveal",
            vec![subj("lot"), dec_str("70"), subj("s4")],
            "dave",
            AFTER,
            &closed_lot(),
        ),
        "a_bid_is_of_a_sealed_envelope",
    );
}
