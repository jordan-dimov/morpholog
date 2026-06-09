//! Integration tests for the clinical-trial-enrolment example
//! (`examples/06_clinical_trial_enrolment/`).
//!
//! Coverage:
//!
//! - **Validity-window happy path.** Every gate satisfied;
//!   randomisation admits.
//!
//! - **Boundary equality.** `effective_to == randomised_on` and
//!   `expires_on == randomised_on` admit. Pins the **inclusive**
//!   `[from, to]` semantics of v0 validity windows.
//!
//! - **Each gate rejects individually.** Expired consent form,
//!   expired eligibility assessment, expired delegation, open
//!   important protocol deviation - each surfaces as a lawful
//!   `Outcome::Rejected`, not a kernel error.
//!
//! - **Protocol amendment.** Admitting `proto_v2` later does not
//!   invalidate an earlier randomisation under `proto_v1`; a new
//!   participant after `proto_v1`'s window closes must enrol
//!   under `proto_v2`. This is the standing-after-amendment
//!   doctrine made enforceable.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{claim_instance, date, has_claim, must_accept, must_accept_as, propose_as, subj};
use morpholog_core::{Definition, EvalValue, Invariant, Outcome, State, eval_invariant};
use morpholog_examples::clinical_trial_enrolment::{
    self as cte, ROLE_RANDOMISE_PARTICIPANT, all_invariants,
};

fn invariants() -> Vec<Invariant> {
    all_invariants()
}

fn definitions() -> Vec<Definition> {
    morpholog_examples::clinical_trial_enrolment::definitions()
}

/// Build a state with the full happy-path setup for randomising
/// `participant_id` on `randomised_on` under `proto_v1`, with windows
/// designed to admit by default. Each per-gate test then **overrides**
/// one specific claim to exercise that gate's rejection path; the
/// other gates stay valid so the rejection is unambiguously
/// attributable.
struct Setup {
    trial: &'static str,
    investigator: &'static str,
    participant: &'static str,
    proto_v1: &'static str,
    proto_v1_from: &'static str,
    proto_v1_to: &'static str,
    consent_form: &'static str,
    consent_from: &'static str,
    consent_to: &'static str,
    consented_on: &'static str,
    delegation_from: &'static str,
    delegation_to: &'static str,
    criterion: &'static str,
    /// The result the criterion demands for eligibility.
    criterion_required_result: &'static str,
    /// The result the assessment actually reported. Defaults to
    /// `criterion_required_result` (happy path); set to a different
    /// value to exercise the failed-assessment rejection path.
    assessment_actual_result: &'static str,
    assessed_on: &'static str,
    assessment_expires_on: &'static str,
    randomised_on: &'static str,
}

fn default_setup() -> Setup {
    Setup {
        trial: "trial_001",
        investigator: "dr_smith",
        participant: "p_001",
        proto_v1: "proto_v1",
        proto_v1_from: "2026-01-01",
        proto_v1_to: "2026-03-31",
        consent_form: "icf_v1",
        consent_from: "2026-01-01",
        consent_to: "2026-03-31",
        consented_on: "2026-03-08",
        delegation_from: "2026-01-01",
        delegation_to: "2026-12-31",
        criterion: "creatinine_panel",
        criterion_required_result: "PASS",
        assessment_actual_result: "PASS",
        assessed_on: "2026-03-09",
        assessment_expires_on: "2026-03-23",
        randomised_on: "2026-03-12",
    }
}

/// Run the full happy-path setup chain against `State::default()`.
/// Each step calls a setup transformation; the returned state has
/// every preceding admission applied. Returning the post-state lets
/// per-test overrides re-build state with one specific claim swapped.
fn happy_path_state(s: &Setup) -> State {
    let mut state = State::default();
    state = must_accept(
        &cte::open_trial(),
        vec![subj(s.trial)],
        state,
        &invariants(),
        &definitions(),
    );
    state = must_accept(
        &cte::approve_protocol_version(),
        vec![
            subj(s.trial),
            subj(s.proto_v1),
            date(s.proto_v1_from),
            date(s.proto_v1_to),
            subj("ethics_committee_uk"),
            subj("approval_001"),
        ],
        state,
        &invariants(),
        &definitions(),
    );
    state = must_accept(
        &cte::approve_consent_form_version(),
        vec![
            subj(s.trial),
            subj(s.consent_form),
            date(s.consent_from),
            date(s.consent_to),
            subj("ethics_committee_uk"),
            subj("approval_002"),
        ],
        state,
        &invariants(),
        &definitions(),
    );
    state = must_accept(
        &cte::delegate_investigator(),
        vec![
            subj(s.investigator),
            subj(s.trial),
            subj(ROLE_RANDOMISE_PARTICIPANT),
            date(s.delegation_from),
            date(s.delegation_to),
        ],
        state,
        &invariants(),
        &definitions(),
    );
    state = must_accept(
        &cte::screen_participant(),
        vec![subj(s.participant), subj(s.trial), date("2026-03-07")],
        state,
        &invariants(),
        &definitions(),
    );
    state = must_accept(
        &cte::record_consent(),
        vec![
            subj(s.participant),
            subj(s.trial),
            subj(s.consent_form),
            date(s.consented_on),
            subj(s.investigator),
        ],
        state,
        &invariants(),
        &definitions(),
    );
    state = must_accept(
        &cte::record_eligibility_criterion(),
        vec![
            subj(s.proto_v1),
            subj(s.criterion),
            subj(s.criterion_required_result),
        ],
        state,
        &invariants(),
        &definitions(),
    );
    state = must_accept(
        &cte::record_eligibility_assessment(),
        vec![
            subj(s.participant),
            subj(s.criterion),
            subj(s.assessment_actual_result),
            date(s.assessed_on),
            date(s.assessment_expires_on),
        ],
        state,
        &invariants(),
        &definitions(),
    );
    state
}

fn randomise_args(s: &Setup) -> Vec<EvalValue> {
    vec![
        subj(s.participant),
        subj(s.trial),
        subj(s.proto_v1),
        date(s.randomised_on),
    ]
}

// ============================================================
// Happy path
// ============================================================

#[test]
fn happy_path_admits_randomisation() {
    let s = default_setup();
    let pre = happy_path_state(&s);
    let post = must_accept_as(
        &cte::randomise_participant(),
        randomise_args(&s),
        s.investigator,
        pre,
        &invariants(),
        &definitions(),
    );
    assert!(
        has_claim(
            &post,
            "ParticipantRandomised",
            &[
                subj(s.participant),
                subj(s.trial),
                subj(s.proto_v1),
                date(s.randomised_on),
                subj(s.investigator),
            ],
        ),
        "happy path must record the ParticipantRandomised claim with the actor"
    );
}

// ============================================================
// Boundary equality - inclusive [from, to]
// ============================================================

#[test]
fn boundary_equality_admits_at_protocol_end() {
    // Randomise on the exact last day of the protocol window. With
    // inclusive semantics, this admits.
    let mut s = default_setup();
    s.randomised_on = s.proto_v1_to; // "2026-03-31"
    s.assessment_expires_on = s.proto_v1_to; // must still cover the date
    s.consent_to = s.proto_v1_to;
    let pre = happy_path_state(&s);
    let post = must_accept_as(
        &cte::randomise_participant(),
        randomise_args(&s),
        s.investigator,
        pre,
        &invariants(),
        &definitions(),
    );
    assert!(has_claim(
        &post,
        "ParticipantRandomised",
        &[
            subj(s.participant),
            subj(s.trial),
            subj(s.proto_v1),
            date(s.randomised_on),
            subj(s.investigator),
        ],
    ));
}

#[test]
fn boundary_equality_admits_at_assessment_expiry() {
    // Randomise on the exact last day the eligibility assessment is
    // valid. Inclusive `[assessed_on, expires_on]` admits.
    let mut s = default_setup();
    s.randomised_on = s.assessment_expires_on; // "2026-03-23"
    let pre = happy_path_state(&s);
    let post = must_accept_as(
        &cte::randomise_participant(),
        randomise_args(&s),
        s.investigator,
        pre,
        &invariants(),
        &definitions(),
    );
    assert!(has_claim(
        &post,
        "ParticipantRandomised",
        &[
            subj(s.participant),
            subj(s.trial),
            subj(s.proto_v1),
            date(s.randomised_on),
            subj(s.investigator),
        ],
    ));
}

// ============================================================
// Per-gate rejections
// ============================================================

#[test]
fn expired_consent_form_rejects() {
    let mut s = default_setup();
    // Close the consent form window the day before randomisation.
    s.consent_to = "2026-03-11";
    let pre = happy_path_state(&s);
    let outcome = propose_as(
        &cte::randomise_participant(),
        randomise_args(&s),
        s.investigator,
        &pre,
        &invariants(),
        &definitions(),
    )
    .expect("propose must not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn consent_after_randomisation_violates_the_invariant() {
    // `randomise_participant`'s gate already refuses consent-after-
    // randomisation for the normal path; the invariant makes it a standing
    // guarantee over *all* admitted state, however it is reached. Here we
    // check the invariant directly against a candidate state.
    let inv = cte::consent_obtained_before_randomisation();

    // Randomised on the 12th, but consent only obtained on the 15th.
    let randomised_before_consent = State::from_claims(vec![
        claim_instance(
            "ParticipantRandomised",
            &[
                subj("p"),
                subj("t"),
                subj("proto"),
                date("2026-03-12"),
                subj("dr_smith"),
            ],
        ),
        claim_instance(
            "InformedConsentObtained",
            &[
                subj("p"),
                subj("t"),
                subj("icf"),
                date("2026-03-15"),
                subj("dr_smith"),
            ],
        ),
    ]);
    assert!(
        !eval_invariant(&inv, &randomised_before_consent, None, &definitions()).unwrap(),
        "randomisation before consent must violate the invariant",
    );

    // Consent on the 5th, randomised on the 12th: the invariant holds.
    let consent_first = State::from_claims(vec![
        claim_instance(
            "ParticipantRandomised",
            &[
                subj("p"),
                subj("t"),
                subj("proto"),
                date("2026-03-12"),
                subj("dr_smith"),
            ],
        ),
        claim_instance(
            "InformedConsentObtained",
            &[
                subj("p"),
                subj("t"),
                subj("icf"),
                date("2026-03-05"),
                subj("dr_smith"),
            ],
        ),
    ]);
    assert!(eval_invariant(&inv, &consent_first, None, &definitions()).unwrap());
}

#[test]
fn expired_eligibility_assessment_rejects() {
    let mut s = default_setup();
    // Assessment expired the day before randomisation.
    s.assessment_expires_on = "2026-03-11";
    let pre = happy_path_state(&s);
    let outcome = propose_as(
        &cte::randomise_participant(),
        randomise_args(&s),
        s.investigator,
        &pre,
        &invariants(),
        &definitions(),
    )
    .expect("propose must not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn expired_delegation_rejects() {
    let mut s = default_setup();
    // Delegation ended before randomisation.
    s.delegation_to = "2026-03-10";
    let pre = happy_path_state(&s);
    let outcome = propose_as(
        &cte::randomise_participant(),
        randomise_args(&s),
        s.investigator,
        &pre,
        &invariants(),
        &definitions(),
    )
    .expect("propose must not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn expired_protocol_window_rejects() {
    let mut s = default_setup();
    // Protocol window closed before randomisation.
    s.proto_v1_to = "2026-03-11";
    let pre = happy_path_state(&s);
    let outcome = propose_as(
        &cte::randomise_participant(),
        randomise_args(&s),
        s.investigator,
        &pre,
        &invariants(),
        &definitions(),
    )
    .expect("propose must not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

#[test]
fn open_important_protocol_deviation_rejects() {
    let s = default_setup();
    let mut pre = happy_path_state(&s);
    // Open a deviation, then attempt randomisation. The Not(...)
    // gate rejects.
    pre = must_accept_as(
        &cte::open_important_protocol_deviation(),
        vec![subj(s.participant), subj(s.trial), subj("dev_001")],
        s.investigator,
        pre,
        &invariants(),
        &definitions(),
    );
    let outcome = propose_as(
        &cte::randomise_participant(),
        randomise_args(&s),
        s.investigator,
        &pre,
        &invariants(),
        &definitions(),
    )
    .expect("propose must not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

// ============================================================
// Eligibility result mismatch
// ============================================================

#[test]
fn failed_eligibility_assessment_rejects() {
    // The criterion requires `PASS`; the assessment recorded `FAIL`.
    // Inside the load-bearing `require And(...)`, the assessment
    // claim's `result` position is unified against the criterion's
    // `required_result` binding - non-matching values fail the And.
    let mut s = default_setup();
    s.assessment_actual_result = "FAIL";
    let pre = happy_path_state(&s);
    let outcome = propose_as(
        &cte::randomise_participant(),
        randomise_args(&s),
        s.investigator,
        &pre,
        &invariants(),
        &definitions(),
    )
    .expect("propose must not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

// ============================================================
// Protocol amendment - the load-bearing doctrine
// ============================================================

#[test]
fn protocol_amendment_preserves_earlier_randomisation_under_proto_v1() {
    // Randomise p_001 on 2026-03-12 under proto_v1. Then admit a
    // later proto_v2 effective from 2026-04-01. The earlier
    // ParticipantRandomised(... proto_v1 ...) claim must remain
    // admitted: validity is checked at admission, not as an eternal
    // invariant.
    let s = default_setup();
    let pre = happy_path_state(&s);
    let post_randomise = must_accept_as(
        &cte::randomise_participant(),
        randomise_args(&s),
        s.investigator,
        pre,
        &invariants(),
        &definitions(),
    );
    let post_amend = must_accept(
        &cte::approve_protocol_version(),
        vec![
            subj(s.trial),
            subj("proto_v2"),
            date("2026-04-01"),
            date("2026-12-31"),
            subj("ethics_committee_uk"),
            subj("approval_003"),
        ],
        post_randomise,
        &invariants(),
        &definitions(),
    );
    // The earlier admission survives the amendment.
    assert!(has_claim(
        &post_amend,
        "ParticipantRandomised",
        &[
            subj(s.participant),
            subj(s.trial),
            subj(s.proto_v1),
            date(s.randomised_on),
            subj(s.investigator),
        ],
    ));
    // And the new protocol version is admitted alongside.
    assert!(has_claim(
        &post_amend,
        "ProtocolVersion",
        &[
            subj(s.trial),
            subj("proto_v2"),
            date("2026-04-01"),
            date("2026-12-31"),
        ],
    ));
}

#[test]
fn later_randomisation_must_use_active_protocol_version() {
    // proto_v1 ends 2026-03-31; proto_v2 starts 2026-04-01. A new
    // participant randomised on 2026-04-15 must enrol under
    // proto_v2 - attempting to enrol them under proto_v1 must
    // reject (proto_v1 window has closed), and enrolling them under
    // proto_v2 with criterion + assessment under proto_v2 must
    // admit. The consent form window is widened so the consent
    // gate is satisfied for the later participant.
    let mut s = default_setup();
    s.consent_to = "2026-12-31";
    let mut state = happy_path_state(&s);
    state = must_accept(
        &cte::approve_protocol_version(),
        vec![
            subj(s.trial),
            subj("proto_v2"),
            date("2026-04-01"),
            date("2026-12-31"),
            subj("ethics_committee_uk"),
            subj("approval_003"),
        ],
        state,
        &invariants(),
        &definitions(),
    );
    state = must_accept(
        &cte::record_eligibility_criterion(),
        vec![
            subj("proto_v2"),
            subj(s.criterion),
            subj(s.criterion_required_result),
        ],
        state,
        &invariants(),
        &definitions(),
    );
    state = must_accept(
        &cte::record_consent(),
        vec![
            subj("p_002"),
            subj(s.trial),
            subj(s.consent_form),
            date("2026-04-05"),
            subj(s.investigator),
        ],
        state,
        &invariants(),
        &definitions(),
    );
    state = must_accept(
        &cte::record_eligibility_assessment(),
        vec![
            subj("p_002"),
            subj(s.criterion),
            subj(s.criterion_required_result),
            date("2026-04-10"),
            date("2026-04-30"),
        ],
        state,
        &invariants(),
        &definitions(),
    );
    // Attempt under proto_v1 on 2026-04-15: protocol window has
    // closed, must reject.
    let rejected = propose_as(
        &cte::randomise_participant(),
        vec![
            subj("p_002"),
            subj(s.trial),
            subj(s.proto_v1),
            date("2026-04-15"),
        ],
        s.investigator,
        &state,
        &invariants(),
        &definitions(),
    )
    .expect("propose must not error");
    assert!(
        matches!(rejected, Outcome::Rejected { .. }),
        "proto_v1 window has closed before 2026-04-15; must reject"
    );
    // Attempt under proto_v2 on 2026-04-15: admits.
    let post = must_accept_as(
        &cte::randomise_participant(),
        vec![
            subj("p_002"),
            subj(s.trial),
            subj("proto_v2"),
            date("2026-04-15"),
        ],
        s.investigator,
        state,
        &invariants(),
        &definitions(),
    );
    assert!(has_claim(
        &post,
        "ParticipantRandomised",
        &[
            subj("p_002"),
            subj(s.trial),
            subj("proto_v2"),
            date("2026-04-15"),
            subj(s.investigator),
        ],
    ));
}
