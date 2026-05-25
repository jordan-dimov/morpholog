//! Clinical trial enrolment example.
//!
//! Authored as surface source at
//! `examples/06_clinical_trial_enrolment/clinical_trial_enrolment.morph`;
//! this module parses it and exposes the registered program plus the
//! by-name accessors the tests use. There is no hand-built IR.

use std::sync::LazyLock;

use morpholog_core::{Invariant, PredicateDecl, Program, Transformation};

/// The investigator role-name that confers authority to propose
/// `randomise_participant` transitions. Kept as a named constant so the
/// transformation body and tests cannot drift on spelling.
pub const ROLE_RANDOMISE_PARTICIPANT: &str = "randomise_participant";

static PROGRAM: LazyLock<Program> = LazyLock::new(|| {
    crate::parse_example(
        "clinical_trial_enrolment",
        include_str!(
            "../../../examples/06_clinical_trial_enrolment/clinical_trial_enrolment.morph"
        ),
    )
});

pub fn program() -> Program {
    PROGRAM.clone()
}

pub fn all_predicates() -> Vec<PredicateDecl> {
    PROGRAM.predicates.clone()
}

pub fn all_invariants() -> Vec<Invariant> {
    PROGRAM.invariants.clone()
}

pub fn at_most_one_protocol_window_per_version() -> Invariant {
    crate::invariant(&PROGRAM, "at_most_one_protocol_window_per_version")
}

pub fn at_most_one_consent_window_per_version() -> Invariant {
    crate::invariant(&PROGRAM, "at_most_one_consent_window_per_version")
}

pub fn participant_randomised_once_per_trial() -> Invariant {
    crate::invariant(&PROGRAM, "participant_randomised_once_per_trial")
}

pub fn consent_obtained_before_randomisation() -> Invariant {
    crate::invariant(&PROGRAM, "consent_obtained_before_randomisation")
}

pub fn open_trial() -> Transformation {
    crate::transformation(&PROGRAM, "open_trial")
}

pub fn approve_protocol_version() -> Transformation {
    crate::transformation(&PROGRAM, "approve_protocol_version")
}

pub fn approve_consent_form_version() -> Transformation {
    crate::transformation(&PROGRAM, "approve_consent_form_version")
}

pub fn delegate_investigator() -> Transformation {
    crate::transformation(&PROGRAM, "delegate_investigator")
}

pub fn screen_participant() -> Transformation {
    crate::transformation(&PROGRAM, "screen_participant")
}

pub fn record_consent() -> Transformation {
    crate::transformation(&PROGRAM, "record_consent")
}

pub fn record_eligibility_criterion() -> Transformation {
    crate::transformation(&PROGRAM, "record_eligibility_criterion")
}

pub fn record_eligibility_assessment() -> Transformation {
    crate::transformation(&PROGRAM, "record_eligibility_assessment")
}

pub fn open_important_protocol_deviation() -> Transformation {
    crate::transformation(&PROGRAM, "open_important_protocol_deviation")
}

pub fn randomise_participant() -> Transformation {
    crate::transformation(&PROGRAM, "randomise_participant")
}
