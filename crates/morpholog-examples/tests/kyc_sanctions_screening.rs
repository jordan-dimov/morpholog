//! Integration tests for the KYC sanctions/PEP screening example
//! (`examples/08_kyc_sanctions_screening/`).
//!
//! These tests pin that: onboarding requires current clean screenings on
//! both lists; an unresolved match blocks onboarding; a request/result
//! round trip moves the currentness pointer; and a reviewed match must be
//! exactly one of false positive or confirmed hit (`xor`), refusing both a
//! missing disposition and a confirmed-hit back door. Intents are emitted
//! for the distinct downstream consumers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{Example, date, subj};
use morpholog_core::{EvalValue, Outcome, Program, State};
use morpholog_examples::kyc_sanctions_screening::{self, PEP, SANCTIONS};
use std::sync::OnceLock;

// ============================================================
// Helpers
// ============================================================

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&kyc_sanctions_screening::program()))
}

// `program` is only a transformation source (some tests extend it
// with extra transformations); the rules always come from `ex()`.
fn run(program: &Program, name: &str, args: Vec<EvalValue>, state: State) -> State {
    let t = program
        .transformation(name)
        .unwrap_or_else(|| panic!("transformation `{name}` not found"));
    ex().must_accept(t, args, state)
}

fn try_run(
    program: &Program,
    name: &str,
    args: Vec<EvalValue>,
    state: &State,
) -> Result<Outcome, morpholog_core::EvalError> {
    let t = program
        .transformation(name)
        .unwrap_or_else(|| panic!("transformation `{name}` not found"));
    ex().propose(t, args, state)
}

/// Register the customer and clear screenings against both sanctions
/// and PEP lists. Expiry dates are set well after `2026-02-01` so
/// onboarding on that date is within the validity window.
fn registered_and_screened(program: &Program, state: State, customer: &str) -> State {
    let state = run(program, "register_customer", vec![subj(customer)], state);

    let state = run(
        program,
        "request_screening",
        vec![
            subj(&format!("{customer}_sanctions_1")),
            subj(customer),
            subj(SANCTIONS),
            date("2026-01-01"),
        ],
        state,
    );
    let state = run(
        program,
        "record_clean_screening_result",
        vec![
            subj(&format!("{customer}_sanctions_1")),
            date("2026-01-02"),
            date("2027-01-02"),
        ],
        state,
    );

    let state = run(
        program,
        "request_screening",
        vec![
            subj(&format!("{customer}_pep_1")),
            subj(customer),
            subj(PEP),
            date("2026-01-01"),
        ],
        state,
    );
    run(
        program,
        "record_clean_screening_result",
        vec![
            subj(&format!("{customer}_pep_1")),
            date("2026-01-02"),
            date("2027-01-02"),
        ],
        state,
    )
}

// ============================================================
// IR-shape sanity
// ============================================================

#[test]
fn program_validates() {
    let program = kyc_sanctions_screening::program();
    program
        .validate()
        .expect("kyc_sanctions_screening must validate cleanly");
}

#[test]
fn program_declares_its_outbox_intents() {
    let program = kyc_sanctions_screening::program();
    let names: Vec<&str> = program.intents.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(
        names,
        vec![
            "ScreeningRequested",
            "MatchRaised",
            "MatchConfirmed",
            "CustomerOnboarded",
            "CustomerRejected",
        ],
    );
}

// ============================================================
// Round-trip pattern: request -> result, currentness pointer moves.
// ============================================================

#[test]
fn second_clean_result_retracts_first_and_takes_pointer() {
    let program = kyc_sanctions_screening::program();
    let alice = "alice";

    let state = State::default();
    let state = run(&program, "register_customer", vec![subj(alice)], state);
    let state = run(
        &program,
        "request_screening",
        vec![
            subj("scr_a"),
            subj(alice),
            subj(SANCTIONS),
            date("2026-01-01"),
        ],
        state,
    );
    let state = run(
        &program,
        "record_clean_screening_result",
        vec![subj("scr_a"), date("2026-01-02"), date("2027-01-02")],
        state,
    );
    let state = run(
        &program,
        "request_screening",
        vec![
            subj("scr_b"),
            subj(alice),
            subj(SANCTIONS),
            date("2026-06-01"),
        ],
        state,
    );
    let state = run(
        &program,
        "record_clean_screening_result",
        vec![subj("scr_b"), date("2026-06-02"), date("2027-06-02")],
        state,
    );

    let pointer_b = state.claims().iter().any(|c| {
        c.predicate.as_str() == "CurrentScreening"
            && c.args == vec![subj(alice), subj(SANCTIONS), subj("scr_b")]
    });
    let pointer_a = state.claims().iter().any(|c| {
        c.predicate.as_str() == "CurrentScreening"
            && c.args == vec![subj(alice), subj(SANCTIONS), subj("scr_a")]
    });
    assert!(pointer_b, "newer screening must own the pointer");
    assert!(!pointer_a, "older screening's pointer must be retracted");
}

// ============================================================
// Onboarding requires current clean screenings on BOTH lists.
// ============================================================

#[test]
fn onboarding_succeeds_when_both_screenings_are_current_and_clean() {
    let program = kyc_sanctions_screening::program();
    let alice = "alice";
    let state = registered_and_screened(&program, State::default(), alice);

    let outcome = try_run(
        &program,
        "onboard_customer",
        vec![subj(alice), date("2026-02-01")],
        &state,
    )
    .expect("propose should not error");
    assert!(
        matches!(outcome, Outcome::Accepted { .. }),
        "onboarding with both screenings current+clean should accept; got {outcome:?}"
    );
}

#[test]
fn onboarding_rejects_when_sanctions_screening_missing() {
    let program = kyc_sanctions_screening::program();
    let alice = "alice";

    let state = State::default();
    let state = run(&program, "register_customer", vec![subj(alice)], state);
    // PEP only - no sanctions screening.
    let state = run(
        &program,
        "request_screening",
        vec![subj("scr_pep"), subj(alice), subj(PEP), date("2026-01-01")],
        state,
    );
    let state = run(
        &program,
        "record_clean_screening_result",
        vec![subj("scr_pep"), date("2026-01-02"), date("2027-01-02")],
        state,
    );

    let outcome = try_run(
        &program,
        "onboard_customer",
        vec![subj(alice), date("2026-02-01")],
        &state,
    )
    .expect("propose should not error");
    assert!(
        matches!(outcome, Outcome::Rejected { .. }),
        "onboarding without sanctions screening must reject; got {outcome:?}"
    );
}

#[test]
fn onboarding_rejects_when_screening_has_expired_by_onboarding_date() {
    let program = kyc_sanctions_screening::program();
    let alice = "alice";

    let state = State::default();
    let state = run(&program, "register_customer", vec![subj(alice)], state);
    // Both screenings clean but with expiry BEFORE the onboarding date.
    let state = run(
        &program,
        "request_screening",
        vec![
            subj("scr_sanctions"),
            subj(alice),
            subj(SANCTIONS),
            date("2025-01-01"),
        ],
        state,
    );
    let state = run(
        &program,
        "record_clean_screening_result",
        vec![
            subj("scr_sanctions"),
            date("2025-01-02"),
            date("2025-12-31"),
        ],
        state,
    );
    let state = run(
        &program,
        "request_screening",
        vec![subj("scr_pep"), subj(alice), subj(PEP), date("2025-01-01")],
        state,
    );
    let state = run(
        &program,
        "record_clean_screening_result",
        vec![subj("scr_pep"), date("2025-01-02"), date("2025-12-31")],
        state,
    );

    let outcome = try_run(
        &program,
        "onboard_customer",
        vec![subj(alice), date("2026-02-01")],
        &state,
    )
    .expect("propose should not error");
    assert!(
        matches!(outcome, Outcome::Rejected { .. }),
        "onboarding with expired screenings must reject; got {outcome:?}"
    );
}

// ============================================================
// Match-handling: an unresolved match against a current screening
// blocks onboarding; adjudication as false-positive admits it.
// ============================================================

/// A customer with clean current screenings gets a newer sanctions
/// screening that returns a match. The match does not move the currentness
/// pointer, so a rule that only checked the current screening would let
/// onboarding through. The invariant joins through Screening, not
/// CurrentScreening, so the match blocks.
#[test]
fn onboarding_rejects_when_newer_screening_returns_match_even_if_old_clean_current_exists() {
    let program = kyc_sanctions_screening::program();
    let alice = "alice";

    // Clean current screenings on both lists.
    let state = registered_and_screened(&program, State::default(), alice);

    // A re-screen against sanctions returns a match.
    let state = run(
        &program,
        "request_screening",
        vec![
            subj("alice_sanctions_2"),
            subj(alice),
            subj(SANCTIONS),
            date("2026-03-01"),
        ],
        state,
    );
    let state = run(
        &program,
        "record_match_screening_result",
        vec![
            subj("alice_sanctions_2"),
            date("2026-03-02"),
            date("2027-03-02"),
            date("2026-03-02"),
        ],
        state,
    );

    // The old clean sanctions screening still holds the currentness
    // pointer, but the new unresolved match must still block.
    let outcome = try_run(
        &program,
        "onboard_customer",
        vec![subj(alice), date("2026-03-15")],
        &state,
    )
    .expect("propose should not error");
    assert!(
        matches!(outcome, Outcome::Rejected { .. }),
        "newer unresolved match must block onboarding even with an old clean current screening; got {outcome:?}"
    );
}

#[test]
fn match_adjudicated_false_positive_enables_onboarding() {
    let program = kyc_sanctions_screening::program();
    let alice = "alice";

    let state = State::default();
    let state = run(&program, "register_customer", vec![subj(alice)], state);

    // Sanctions: request, get a match, adjudicate false-positive.
    let state = run(
        &program,
        "request_screening",
        vec![
            subj("scr_sanctions_1"),
            subj(alice),
            subj(SANCTIONS),
            date("2026-01-01"),
        ],
        state,
    );
    let state = run(
        &program,
        "record_match_screening_result",
        vec![
            subj("scr_sanctions_1"),
            date("2026-01-02"),
            date("2027-01-02"),
            date("2026-01-02"),
        ],
        state,
    );
    let state = run(
        &program,
        "adjudicate_match_as_false_positive",
        vec![
            subj("scr_sanctions_1"),
            date("2026-01-03"),
            date("2027-01-03"),
        ],
        state,
    );

    // PEP: simple clean result.
    let state = run(
        &program,
        "request_screening",
        vec![
            subj("scr_pep_1"),
            subj(alice),
            subj(PEP),
            date("2026-01-01"),
        ],
        state,
    );
    let state = run(
        &program,
        "record_clean_screening_result",
        vec![subj("scr_pep_1"), date("2026-01-02"), date("2027-01-02")],
        state,
    );

    let outcome = try_run(
        &program,
        "onboard_customer",
        vec![subj(alice), date("2026-02-01")],
        &state,
    )
    .expect("propose should not error");
    assert!(
        matches!(outcome, Outcome::Accepted { .. }),
        "onboarding after match adjudicated false-positive should accept; got {outcome:?}"
    );
}

/// A confirmed match is a lasting bar. Bob's current screenings are
/// clean, but a newer re-screen was confirmed a hit. Confirming clears the
/// review flag, so "no unresolved match" no longer applies;
/// `onboarded_requires_no_confirmed_match` keeps onboarding refused.
#[test]
fn confirmed_match_blocks_onboarding_even_behind_a_clean_current_screening() {
    let program = kyc_sanctions_screening::program();
    let bob = "bob";

    // Clean sanctions + PEP, both current - bob would be onboardable.
    let state = registered_and_screened(&program, State::default(), bob);

    // A newer sanctions re-screen returns a match, adjudicated confirmed.
    // It never becomes current, so the older clean screening still stands.
    let state = run(
        &program,
        "request_screening",
        vec![
            subj("bob_sanctions_2"),
            subj(bob),
            subj(SANCTIONS),
            date("2026-01-10"),
        ],
        state,
    );
    let state = run(
        &program,
        "record_match_screening_result",
        vec![
            subj("bob_sanctions_2"),
            date("2026-01-11"),
            date("2027-01-11"),
            date("2026-01-11"),
        ],
        state,
    );
    let state = run(
        &program,
        "adjudicate_match_as_confirmed",
        vec![
            subj("bob_sanctions_2"),
            date("2026-01-12"),
            date("2027-01-12"),
        ],
        state,
    );

    let outcome = try_run(
        &program,
        "onboard_customer",
        vec![subj(bob), date("2026-02-01")],
        &state,
    )
    .expect("propose should not error");
    match outcome {
        Outcome::Rejected { reason } => assert!(
            reason
                .to_string()
                .contains("onboarded_requires_no_confirmed_match"),
            "expected the confirmed-match bar to reject onboarding, got: {reason}"
        ),
        Outcome::Accepted { .. } => {
            panic!("a confirmed match must bar onboarding even behind a clean current screening")
        }
    }
}

/// `xor` forbids *neither* disposition as well as *both*. Marking a match
/// adjudicated with no disposition is rejected by
/// `adjudicated_match_resolves_exactly_one_way`; a plain
/// `not (clear and confirmed)` would miss it.
#[test]
fn adjudicated_marker_without_a_disposition_is_rejected() {
    use morpholog_core::ir_builder;

    let mut program = kyc_sanctions_screening::program();
    let carol = "carol";

    // A screening that came back as a match, awaiting review.
    let state = State::default();
    let state = run(&program, "register_customer", vec![subj(carol)], state);
    let state = run(
        &program,
        "request_screening",
        vec![
            subj("scr_sanctions_1"),
            subj(carol),
            subj(SANCTIONS),
            date("2026-01-01"),
        ],
        state,
    );
    let state = run(
        &program,
        "record_match_screening_result",
        vec![
            subj("scr_sanctions_1"),
            date("2026-01-02"),
            date("2027-01-02"),
            date("2026-01-02"),
        ],
        state,
    );

    // A hand-built transformation that marks the screening adjudicated
    // with no disposition: the "neither" case.
    let bad = ir_builder::transformation(
        "adjudicate_without_disposition",
        ir_builder::params(&["screening_id"]),
        vec![ir_builder::assert_(
            "MatchAdjudicated",
            vec![ir_builder::var("screening_id")],
        )],
    );
    program.transformations.push(bad);
    let bad = program
        .transformation("adjudicate_without_disposition")
        .expect("just pushed");

    let reason = ex().must_reject(bad, vec![subj("scr_sanctions_1")], &state);
    assert!(
        reason
            .to_string()
            .contains("adjudicated_match_resolves_exactly_one_way"),
        "expected the xor invariant to reject a marker with no disposition, got: {reason}"
    );
}

// ============================================================
// Intent emission
// ============================================================

#[test]
fn request_screening_emits_screening_requested_intent() {
    let program = kyc_sanctions_screening::program();
    let alice = "alice";

    let state = run(
        &program,
        "register_customer",
        vec![subj(alice)],
        State::default(),
    );
    let outcome = try_run(
        &program,
        "request_screening",
        vec![
            subj("scr_a"),
            subj(alice),
            subj(SANCTIONS),
            date("2026-01-01"),
        ],
        &state,
    )
    .expect("propose should not error");
    let Outcome::Accepted {
        emitted_intents, ..
    } = outcome
    else {
        panic!("expected accept; got {outcome:?}");
    };
    assert!(
        emitted_intents
            .iter()
            .any(|i| i.name == "ScreeningRequested"),
        "expected ScreeningRequested intent; got {emitted_intents:?}"
    );
}

#[test]
fn onboarding_emits_customer_onboarded_intent() {
    let program = kyc_sanctions_screening::program();
    let alice = "alice";
    let state = registered_and_screened(&program, State::default(), alice);

    let outcome = try_run(
        &program,
        "onboard_customer",
        vec![subj(alice), date("2026-02-01")],
        &state,
    )
    .expect("propose should not error");
    let Outcome::Accepted {
        emitted_intents, ..
    } = outcome
    else {
        panic!("expected accept; got {outcome:?}");
    };
    assert!(
        emitted_intents
            .iter()
            .any(|i| i.name == "CustomerOnboarded"),
        "expected CustomerOnboarded intent; got {emitted_intents:?}"
    );
}

#[test]
fn reject_customer_emits_customer_rejected_intent() {
    let program = kyc_sanctions_screening::program();
    let alice = "alice";

    let state = run(
        &program,
        "register_customer",
        vec![subj(alice)],
        State::default(),
    );
    let outcome = try_run(
        &program,
        "reject_customer",
        vec![subj(alice), subj("confirmed_sanctions_hit")],
        &state,
    )
    .expect("propose should not error");
    let Outcome::Accepted {
        emitted_intents, ..
    } = outcome
    else {
        panic!("expected accept; got {outcome:?}");
    };
    assert!(
        emitted_intents.iter().any(|i| i.name == "CustomerRejected"),
        "expected CustomerRejected intent; got {emitted_intents:?}"
    );
}

#[test]
fn record_match_emits_match_raised_intent() {
    let program = kyc_sanctions_screening::program();
    let alice = "alice";

    let state = run(
        &program,
        "register_customer",
        vec![subj(alice)],
        State::default(),
    );
    let state = run(
        &program,
        "request_screening",
        vec![
            subj("scr_match"),
            subj(alice),
            subj(SANCTIONS),
            date("2026-01-01"),
        ],
        state,
    );
    let outcome = try_run(
        &program,
        "record_match_screening_result",
        vec![
            subj("scr_match"),
            date("2026-01-02"),
            date("2027-01-02"),
            date("2026-01-02"),
        ],
        &state,
    )
    .expect("propose should not error");
    let Outcome::Accepted {
        emitted_intents, ..
    } = outcome
    else {
        panic!("expected accept; got {outcome:?}");
    };
    assert!(
        emitted_intents.iter().any(|i| i.name == "MatchRaised"),
        "expected MatchRaised intent; got {emitted_intents:?}"
    );
}

#[test]
fn adjudicate_as_confirmed_emits_match_confirmed_intent() {
    let program = kyc_sanctions_screening::program();
    let alice = "alice";

    let state = run(
        &program,
        "register_customer",
        vec![subj(alice)],
        State::default(),
    );
    let state = run(
        &program,
        "request_screening",
        vec![
            subj("scr_match"),
            subj(alice),
            subj(SANCTIONS),
            date("2026-01-01"),
        ],
        state,
    );
    let state = run(
        &program,
        "record_match_screening_result",
        vec![
            subj("scr_match"),
            date("2026-01-02"),
            date("2027-01-02"),
            date("2026-01-02"),
        ],
        state,
    );
    let outcome = try_run(
        &program,
        "adjudicate_match_as_confirmed",
        vec![subj("scr_match"), date("2026-01-03"), date("2027-01-03")],
        &state,
    )
    .expect("propose should not error");
    let Outcome::Accepted {
        emitted_intents, ..
    } = outcome
    else {
        panic!("expected accept; got {outcome:?}");
    };
    assert!(
        emitted_intents.iter().any(|i| i.name == "MatchConfirmed"),
        "expected MatchConfirmed intent; got {emitted_intents:?}"
    );
}
