//! Integration tests for the biometric identification oversight
//! example (`examples/13_biometric_identification_oversight/`) - the
//! EU AI Act Articles 12 and 14(5) demo script, each beat a refused
//! or admitted proposal.
//!
//! The composition thesis under test: authority grant/revoke
//! (example 04's shape), admission-time validity windows (example
//! 06's), standing granted by verification (example 02's), and exact
//! instants (example 12's) meet a statute - with no new kernel or
//! surface needed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{must_accept_as, propose_with_test_actor, subj, ts};
use morpholog_core::{
    Definition, EvalValue, Invariant, Outcome, Rejection, State, Subject, Transformation,
    Transition, Verdict, enumerate_derived, propose,
};
use morpholog_examples::biometric_identification_oversight as bio;

fn invariants() -> Vec<Invariant> {
    bio::all_invariants()
}

fn definitions() -> Vec<Definition> {
    bio::definitions()
}

/// Propose as a named actor and require a lawful business rejection.
fn must_reject_as(t: &Transformation, args: Vec<EvalValue>, actor: &str, pre: &State) {
    let transition = Transition {
        transformation_name: t.name.clone(),
        args,
        actor: Subject::from(actor),
    };
    let outcome = propose(t, &transition, pre, &invariants(), &definitions())
        .expect("proposal should evaluate cleanly");
    assert!(
        matches!(outcome, Outcome::Rejected { .. }),
        "expected rejection, got {outcome:?}"
    );
}

/// Fixture: a deployed system with model version v1 in service for
/// October 2026, two trained overseers (anna, ben) assigned, a use
/// started on 12 October against the missing-persons register, and
/// one candidate match recorded by the system itself - a claim with
/// no standing yet.
fn match_awaiting_verification() -> State {
    let state = must_accept_as(
        &bio::deploy_system(),
        vec![subj("cam_system"), subj("city_operator")],
        "compliance_office",
        State::default(),
        &invariants(),
        &definitions(),
    );
    let state = must_accept_as(
        &bio::place_version_in_service(),
        vec![
            subj("cam_system"),
            subj("model_v1"),
            ts("2026-10-01T00:00:00Z"),
            ts("2026-10-31T23:59:59Z"),
        ],
        "compliance_office",
        state,
        &invariants(),
        &definitions(),
    );
    let state = must_accept_as(
        &bio::assign_oversight(),
        vec![subj("anna"), subj("cam_system")],
        "compliance_office",
        state,
        &invariants(),
        &definitions(),
    );
    let state = must_accept_as(
        &bio::assign_oversight(),
        vec![subj("ben"), subj("cam_system")],
        "compliance_office",
        state,
        &invariants(),
        &definitions(),
    );
    let state = must_accept_as(
        &bio::start_use(),
        vec![
            subj("use_1"),
            subj("cam_system"),
            subj("model_v1"),
            subj("missing_persons_register"),
            ts("2026-10-12T08:00:00Z"),
        ],
        "cam_system",
        state,
        &invariants(),
        &definitions(),
    );
    // The machine actor: the embedding system proposes its own raw
    // output, through the same gates as anyone else.
    must_accept_as(
        &bio::record_match(),
        vec![
            subj("match_1"),
            subj("use_1"),
            subj("frame_4411"),
            subj("candidate_7"),
            ts("2026-10-12T09:30:00Z"),
        ],
        "cam_system",
        state,
        &invariants(),
        &definitions(),
    )
}

#[test]
fn programme_validates() {
    let p = bio::program();
    assert!(p.validate().is_ok(), "{:?}", p.validate());
}

// Beat 1: an unassessed model version cannot put anything on the
// record at all - a use cannot start outside the version's service
// window.
#[test]
fn use_cannot_start_under_a_version_not_in_service() {
    let state = must_accept_as(
        &bio::deploy_system(),
        vec![subj("cam_system"), subj("city_operator")],
        "compliance_office",
        State::default(),
        &invariants(),
        &definitions(),
    );
    let state = must_accept_as(
        &bio::place_version_in_service(),
        vec![
            subj("cam_system"),
            subj("model_v1"),
            ts("2026-10-01T00:00:00Z"),
            ts("2026-10-31T23:59:59Z"),
        ],
        "compliance_office",
        state,
        &invariants(),
        &definitions(),
    );
    // November is outside the assessed window: refused.
    must_reject_as(
        &bio::start_use(),
        vec![
            subj("use_x"),
            subj("cam_system"),
            subj("model_v1"),
            subj("missing_persons_register"),
            ts("2026-11-02T08:00:00Z"),
        ],
        "cam_system",
        &state,
    );
}

// Beat 2: the statute's two-person rule as a refusal. One
// verification is not enough to decide.
#[test]
fn decision_with_one_verification_is_refused() {
    let state = match_awaiting_verification();
    let state = must_accept_as(
        &bio::verify_match(),
        vec![subj("match_1"), ts("2026-10-12T10:00:00Z")],
        "anna",
        state,
        &invariants(),
        &definitions(),
    );
    must_reject_as(
        &bio::decide_on_identification(),
        vec![
            subj("decision_1"),
            subj("match_1"),
            subj("confirmed_identification"),
            ts("2026-10-12T11:00:00Z"),
        ],
        "anna",
        &state,
    );
}

// The PR's headline, pinned: a refused decision explains itself in
// the statute's own terms. With no verification yet on record, asking
// why the decision is refused names the verification condition by its
// declared name AND - descending through that definition's body - the
// directly-missing MatchVerified claim, with `verify_match` as the
// transformation that supplies it. Both levels: the named business
// condition, and the concrete missing evidence an auditor (or an
// agent retrying) can act on.
#[test]
fn the_refused_decision_explains_itself_in_the_statutes_terms() {
    let state = match_awaiting_verification();
    // A match recorded, but not yet verified by anyone.
    let transition = Transition {
        transformation_name: bio::decide_on_identification().name.clone(),
        args: vec![
            subj("decision_1"),
            subj("match_1"),
            subj("confirmed_identification"),
            ts("2026-10-12T11:00:00Z"),
        ],
        actor: Subject::from("anna"),
    };
    let explanation = morpholog_core::explain(&bio::program(), &transition, &state);
    let Verdict::Rejected(Rejection::Gate(gate)) = &explanation.verdict else {
        panic!("expected a gate rejection, got {:?}", explanation.verdict);
    };
    assert!(
        gate.gate.contains("two_distinct_prior_verifications"),
        "the failing gate names the statute's condition by name: {}",
        gate.gate
    );
    assert!(
        gate.directly_missing_claims
            .iter()
            .any(|m| m.predicate == "MatchVerified"
                && m.candidate_supplier_transformations
                    .contains(&"verify_match".to_string())),
        "a missing MatchVerified names verify_match as its supplier: {:?}",
        gate.directly_missing_claims
    );
}

// The one-hop explanation engine's honest boundary, pinned so it
// cannot regress unnoticed: once ONE verification exists, the gate
// still fails (a second distinct verifier is needed), but the gap is
// the `v1 != v2` distinctness, not an absent claim - so the engine
// names the failing gate without a directly-missing-claim checklist.
// Distinctness-aware why-not is a deferred tier; this test documents
// that the example sits right at its edge.
#[test]
fn one_verification_names_the_gate_but_distinctness_is_not_a_missing_claim() {
    let state = match_awaiting_verification();
    let state = must_accept_as(
        &bio::verify_match(),
        vec![subj("match_1"), ts("2026-10-12T10:00:00Z")],
        "anna",
        state,
        &invariants(),
        &definitions(),
    );
    let transition = Transition {
        transformation_name: bio::decide_on_identification().name.clone(),
        args: vec![
            subj("decision_1"),
            subj("match_1"),
            subj("confirmed_identification"),
            ts("2026-10-12T11:00:00Z"),
        ],
        actor: Subject::from("anna"),
    };
    let explanation = morpholog_core::explain(&bio::program(), &transition, &state);
    let Verdict::Rejected(Rejection::Gate(gate)) = &explanation.verdict else {
        panic!("expected a gate rejection, got {:?}", explanation.verdict);
    };
    assert!(gate.gate.contains("two_distinct_prior_verifications"));
    assert!(
        gate.directly_missing_claims.is_empty(),
        "distinctness failure surfaces no missing claim in v0: {:?}",
        gate.directly_missing_claims
    );
}

// Beat 3: one person confirming twice is one voice, not two. The
// second verification by the same overseer is itself refused, so the
// two-voice requirement cannot be met single-handedly.
#[test]
fn the_same_overseer_cannot_be_both_voices() {
    let state = match_awaiting_verification();
    let state = must_accept_as(
        &bio::verify_match(),
        vec![subj("match_1"), ts("2026-10-12T10:00:00Z")],
        "anna",
        state,
        &invariants(),
        &definitions(),
    );
    must_reject_as(
        &bio::verify_match(),
        vec![subj("match_1"), ts("2026-10-12T10:05:00Z")],
        "anna",
        &state,
    );
}

// The happy path: two distinct verifiers, then a decision - and the
// derived use period reads like the statute's own log line.
#[test]
fn two_distinct_verifications_admit_the_decision() {
    let state = match_awaiting_verification();
    let state = must_accept_as(
        &bio::verify_match(),
        vec![subj("match_1"), ts("2026-10-12T10:00:00Z")],
        "anna",
        state,
        &invariants(),
        &definitions(),
    );
    let state = must_accept_as(
        &bio::verify_match(),
        vec![subj("match_1"), ts("2026-10-12T10:20:00Z")],
        "ben",
        state,
        &invariants(),
        &definitions(),
    );
    let state = must_accept_as(
        &bio::decide_on_identification(),
        vec![
            subj("decision_1"),
            subj("match_1"),
            subj("confirmed_identification"),
            ts("2026-10-12T11:00:00Z"),
        ],
        "anna",
        state,
        &invariants(),
        &definitions(),
    );
    // Article 12(3)(a): the period of each use, derived once the use
    // ends - eight and a half hours, exactly.
    let state = must_accept_as(
        &bio::end_use(),
        vec![subj("use_1"), ts("2026-10-12T16:30:00Z")],
        "cam_system",
        state,
        &invariants(),
        &definitions(),
    );
    let rows = enumerate_derived(&bio::use_period(), &state, &bio::definitions()).unwrap();
    assert_eq!(rows.len(), 1, "one completed use: {rows:?}");
    assert_eq!(rows[0].args[3], common::dur("PT8H30M"));
}

// Beat 4: revocation governs the future only. The revoked overseer
// can no longer verify; the decision they helped verify stands.
#[test]
fn revocation_stops_future_verifications_and_leaves_past_decisions_standing() {
    let state = match_awaiting_verification();
    let state = must_accept_as(
        &bio::verify_match(),
        vec![subj("match_1"), ts("2026-10-12T10:00:00Z")],
        "anna",
        state,
        &invariants(),
        &definitions(),
    );
    let state = must_accept_as(
        &bio::verify_match(),
        vec![subj("match_1"), ts("2026-10-12T10:20:00Z")],
        "ben",
        state,
        &invariants(),
        &definitions(),
    );
    let state = must_accept_as(
        &bio::decide_on_identification(),
        vec![
            subj("decision_1"),
            subj("match_1"),
            subj("confirmed_identification"),
            ts("2026-10-12T11:00:00Z"),
        ],
        "ben",
        state,
        &invariants(),
        &definitions(),
    );
    // Anna's authority is revoked - her training lapsed, say.
    let state = must_accept_as(
        &bio::revoke_oversight(),
        vec![subj("anna"), subj("cam_system")],
        "compliance_office",
        state,
        &invariants(),
        &definitions(),
    );
    // A second match arrives; anna can no longer verify it.
    let state = must_accept_as(
        &bio::record_match(),
        vec![
            subj("match_2"),
            subj("use_1"),
            subj("frame_5012"),
            subj("candidate_9"),
            ts("2026-10-12T12:00:00Z"),
        ],
        "cam_system",
        state,
        &invariants(),
        &definitions(),
    );
    must_reject_as(
        &bio::verify_match(),
        vec![subj("match_2"), ts("2026-10-12T12:30:00Z")],
        "anna",
        &state,
    );
    // The decision admitted under her live authority is untouched:
    // the revocation transformation committed against a state that
    // still satisfies every invariant, decision included.
    assert!(
        state
            .claims()
            .iter()
            .any(|c| c.predicate.as_str() == "IdentificationDecision"),
        "the past decision remains a valid record"
    );
}

// Article 14(5)'s "before action", made literal: a decision cannot
// be stamped earlier than the second verification it rests on.
#[test]
fn decision_dated_before_the_second_verification_is_refused() {
    let state = match_awaiting_verification();
    let state = must_accept_as(
        &bio::verify_match(),
        vec![subj("match_1"), ts("2026-10-12T10:00:00Z")],
        "anna",
        state,
        &invariants(),
        &definitions(),
    );
    let state = must_accept_as(
        &bio::verify_match(),
        vec![subj("match_1"), ts("2026-10-12T10:20:00Z")],
        "ben",
        state,
        &invariants(),
        &definitions(),
    );
    // Both verifications exist, but the decision is back-dated to
    // 10:10 - after anna, before ben. Two records is not enough; both
    // must precede the decision.
    must_reject_as(
        &bio::decide_on_identification(),
        vec![
            subj("decision_1"),
            subj("match_1"),
            subj("confirmed_identification"),
            ts("2026-10-12T10:10:00Z"),
        ],
        "anna",
        &state,
    );
}

// The machine actor is load-bearing: only the deployed system's own
// identity may put its output on the record.
#[test]
fn a_match_cannot_be_recorded_under_the_wrong_actor() {
    let state = match_awaiting_verification();
    // An analyst - not the camera system - tries to add a match to
    // the same use. Refused: the actor is not the system.
    must_reject_as(
        &bio::record_match(),
        vec![
            subj("match_forged"),
            subj("use_1"),
            subj("frame_9999"),
            subj("candidate_x"),
            ts("2026-10-12T09:45:00Z"),
        ],
        "rogue_analyst",
        &state,
    );
}

// Beat 5: the record cannot be shortened to exclude its own matches.
#[test]
fn use_cannot_be_closed_before_a_match_it_already_produced() {
    let state = match_awaiting_verification();
    // The match is at 09:30; closing the use at 09:00 is refused by
    // the invariant, whatever transformation attempts it.
    must_reject_as(
        &bio::end_use(),
        vec![subj("use_1"), ts("2026-10-12T09:00:00Z")],
        "cam_system",
        &state,
    );
}

// A kernel-error-free sanity sweep: the decision gate's witness
// search is a lawful rejection path, not an evaluation error, even
// with zero verifications on record.
#[test]
fn decision_with_no_verifications_is_a_lawful_rejection() {
    let state = match_awaiting_verification();
    let outcome = propose_with_test_actor(
        &bio::decide_on_identification(),
        vec![
            subj("decision_1"),
            subj("match_1"),
            subj("confirmed_identification"),
            ts("2026-10-12T11:00:00Z"),
        ],
        &state,
        &invariants(),
        &definitions(),
    )
    .expect("witness search over empty extension must not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}
