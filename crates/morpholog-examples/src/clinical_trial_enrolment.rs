//! Clinical trial enrolment IR: validity-window admission with civil
//! dates. The load-bearing transformation `randomise_participant`
//! admits only if the protocol version, consent form, investigator
//! delegation and eligibility evidence are all valid on the
//! randomisation date - encoded as inclusive `DateLe(from, date) /\
//! DateLe(date, to)` gates inside a single `require And(...)`.
//!
//! Validity windows are checked at admission, not as eternal
//! invariants on `ParticipantRandomised`: closing a window or
//! amending the protocol must not retroactively invalidate an earlier
//! randomisation. See `examples/06_clinical_trial_enrolment/README.md`
//! for the business framing.

use morpholog_core::{Invariant, Transformation};

use morpholog_core::dsl::*;

/// Subject literal used as the `role` argument of a
/// `DelegatedInvestigator` claim when the delegation grants
/// authority to propose `randomise_participant` transitions. Kept
/// as a named constant so the transformation body and tests cannot
/// drift on spelling.
pub const ROLE_RANDOMISE_PARTICIPANT: &str = "randomise_participant";

// ============================================================
// Invariants - structural only
// ============================================================

/// A given `(trial_id, protocol_version)` has at most one effective
/// window. Two `ProtocolVersion` claims sharing those keys must agree
/// on `effective_from` and `effective_to`. Without this, a
/// `randomise_participant` could admit under one window and be
/// retroactively contradicted by a second.
///
/// Mirrors the singleton-shape from
/// `verified_revenue::at_most_one_current_verification_per_asset_period`
/// and `insurance_claim_settlement::at_most_one_policy_per_id`.
pub fn at_most_one_protocol_window_per_version() -> Invariant {
    Invariant {
        name: "at_most_one_protocol_window_per_version".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim(
                    "ProtocolVersion",
                    vec![var("trial"), var("version"), var("from_a"), var("to_a")],
                ),
                claim(
                    "ProtocolVersion",
                    vec![var("trial"), var("version"), var("from_b"), var("to_b")],
                ),
            ]),
            and(vec![
                eq(term(var("from_a")), term(var("from_b"))),
                eq(term(var("to_a")), term(var("to_b"))),
            ]),
        ),
    }
}

/// A given `(trial_id, consent_form_version)` has at most one
/// effective window. Same shape as the protocol-version uniqueness
/// invariant.
pub fn at_most_one_consent_window_per_version() -> Invariant {
    Invariant {
        name: "at_most_one_consent_window_per_version".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim(
                    "ConsentFormVersion",
                    vec![var("trial"), var("version"), var("from_a"), var("to_a")],
                ),
                claim(
                    "ConsentFormVersion",
                    vec![var("trial"), var("version"), var("from_b"), var("to_b")],
                ),
            ]),
            and(vec![
                eq(term(var("from_a")), term(var("from_b"))),
                eq(term(var("to_a")), term(var("to_b"))),
            ]),
        ),
    }
}

/// A participant is randomised at most once per trial. Two
/// `ParticipantRandomised` claims sharing `(participant_id, trial_id)`
/// must agree on protocol version, date, and randomising actor.
/// Catches the "randomised twice under different protocols" footgun
/// without making validity-window violations an eternal invariant.
pub fn participant_randomised_once_per_trial() -> Invariant {
    Invariant {
        name: "participant_randomised_once_per_trial".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim(
                    "ParticipantRandomised",
                    vec![
                        var("p"),
                        var("trial"),
                        var("proto_a"),
                        var("date_a"),
                        var("actor_a"),
                    ],
                ),
                claim(
                    "ParticipantRandomised",
                    vec![
                        var("p"),
                        var("trial"),
                        var("proto_b"),
                        var("date_b"),
                        var("actor_b"),
                    ],
                ),
            ]),
            and(vec![
                eq(term(var("proto_a")), term(var("proto_b"))),
                eq(term(var("date_a")), term(var("date_b"))),
                eq(term(var("actor_a")), term(var("actor_b"))),
            ]),
        ),
    }
}

// ============================================================
// Setup transformations - the minimum to make the load-bearing
// transformation reachable. Each transformation asserts its
// admitted claims and emits a matching outbox intent; invariants
// enforce uniqueness where they would otherwise let duplicates
// through.
// ============================================================

/// Open a trial. Unconditional in v0; downstream setup
/// transformations require the trial to exist.
pub fn open_trial() -> Transformation {
    Transformation {
        name: "open_trial".to_string(),
        parameters: params(&["trial_id"]),
        body: vec![
            assert_("Trial", vec![var("trial_id")]),
            emit("TrialOpened", vec![var("trial_id")]),
        ],
    }
}

/// Record an approved protocol version with its effective window.
/// Requires the trial to exist; the
/// `at_most_one_protocol_window_per_version` invariant rejects
/// conflicting windows on the candidate state.
pub fn approve_protocol_version() -> Transformation {
    Transformation {
        name: "approve_protocol_version".to_string(),
        parameters: params(&[
            "trial_id",
            "protocol_version",
            "effective_from",
            "effective_to",
            "ethics_committee",
            "approval_id",
        ]),
        body: vec![
            require(claim("Trial", vec![var("trial_id")])),
            assert_(
                "ProtocolVersion",
                vec![
                    var("trial_id"),
                    var("protocol_version"),
                    var("effective_from"),
                    var("effective_to"),
                ],
            ),
            assert_(
                "ProtocolApprovedBy",
                vec![
                    var("protocol_version"),
                    var("ethics_committee"),
                    var("approval_id"),
                ],
            ),
            emit(
                "ProtocolVersionApproved",
                vec![var("trial_id"), var("protocol_version")],
            ),
        ],
    }
}

/// Record an approved consent form version with its effective
/// window. Same shape as `approve_protocol_version`; the
/// `at_most_one_consent_window_per_version` invariant rejects
/// conflicting windows.
pub fn approve_consent_form_version() -> Transformation {
    Transformation {
        name: "approve_consent_form_version".to_string(),
        parameters: params(&[
            "trial_id",
            "consent_form_version",
            "effective_from",
            "effective_to",
            "ethics_committee",
            "approval_id",
        ]),
        body: vec![
            require(claim("Trial", vec![var("trial_id")])),
            assert_(
                "ConsentFormVersion",
                vec![
                    var("trial_id"),
                    var("consent_form_version"),
                    var("effective_from"),
                    var("effective_to"),
                ],
            ),
            assert_(
                "ConsentFormApprovedBy",
                vec![
                    var("consent_form_version"),
                    var("ethics_committee"),
                    var("approval_id"),
                ],
            ),
            emit(
                "ConsentFormVersionApproved",
                vec![var("trial_id"), var("consent_form_version")],
            ),
        ],
    }
}

/// Delegate a named role on a trial to an actor over an effective
/// window. The `role` argument is a subject literal naming the kind
/// of action this delegation grants - the load-bearing
/// `randomise_participant` consults a delegation whose role equals
/// the named `ROLE_RANDOMISE_PARTICIPANT` constant.
pub fn delegate_investigator() -> Transformation {
    Transformation {
        name: "delegate_investigator".to_string(),
        parameters: params(&[
            "investigator",
            "trial_id",
            "role",
            "effective_from",
            "effective_to",
        ]),
        body: vec![
            require(claim("Trial", vec![var("trial_id")])),
            assert_(
                "DelegatedInvestigator",
                vec![
                    var("investigator"),
                    var("trial_id"),
                    var("role"),
                    var("effective_from"),
                    var("effective_to"),
                ],
            ),
            emit(
                "InvestigatorDelegated",
                vec![var("investigator"), var("trial_id"), var("role")],
            ),
        ],
    }
}

/// Record a participant as screened against a trial on a given
/// date. No window: screening is a point-in-time event whose
/// downstream relevance is governed by what assessments and
/// consents follow.
pub fn screen_participant() -> Transformation {
    Transformation {
        name: "screen_participant".to_string(),
        parameters: params(&["participant_id", "trial_id", "screened_on"]),
        body: vec![
            require(claim("Trial", vec![var("trial_id")])),
            assert_(
                "ParticipantScreened",
                vec![var("participant_id"), var("trial_id"), var("screened_on")],
            ),
            emit(
                "ParticipantScreened",
                vec![var("participant_id"), var("trial_id")],
            ),
        ],
    }
}

/// Record informed consent obtained from a participant for a
/// specific consent form version on a specific date, by a specific
/// actor.
pub fn record_consent() -> Transformation {
    Transformation {
        name: "record_consent".to_string(),
        parameters: params(&[
            "participant_id",
            "trial_id",
            "consent_form_version",
            "consented_on",
            "obtained_by",
        ]),
        body: vec![
            require(claim("Trial", vec![var("trial_id")])),
            assert_(
                "InformedConsentObtained",
                vec![
                    var("participant_id"),
                    var("trial_id"),
                    var("consent_form_version"),
                    var("consented_on"),
                    var("obtained_by"),
                ],
            ),
            emit(
                "InformedConsentObtained",
                vec![var("participant_id"), var("consent_form_version")],
            ),
        ],
    }
}

/// Record an eligibility criterion required by a protocol version.
/// A criterion names a `criterion_id` and the `required_result` an
/// assessment must report for the participant to be eligible. The
/// load-bearing transformation unifies the criterion's
/// `required_result` against the assessment's result, so the
/// shared value position is the equality check.
pub fn record_eligibility_criterion() -> Transformation {
    Transformation {
        name: "record_eligibility_criterion".to_string(),
        parameters: params(&["protocol_version", "criterion_id", "required_result"]),
        body: vec![
            assert_(
                "EligibilityCriterion",
                vec![
                    var("protocol_version"),
                    var("criterion_id"),
                    var("required_result"),
                ],
            ),
            emit(
                "EligibilityCriterionRecorded",
                vec![var("protocol_version"), var("criterion_id")],
            ),
        ],
    }
}

/// Record an eligibility assessment for a participant against a
/// criterion. The assessment has a result, an assessment date, and
/// an expiry date past which the result no longer counts. The
/// validity window is `[assessed_on, expires_on]` inclusive.
pub fn record_eligibility_assessment() -> Transformation {
    Transformation {
        name: "record_eligibility_assessment".to_string(),
        parameters: params(&[
            "participant_id",
            "criterion_id",
            "result",
            "assessed_on",
            "expires_on",
        ]),
        body: vec![
            assert_(
                "EligibilityAssessment",
                vec![
                    var("participant_id"),
                    var("criterion_id"),
                    var("result"),
                    var("assessed_on"),
                    var("expires_on"),
                ],
            ),
            emit(
                "EligibilityAssessmentRecorded",
                vec![var("participant_id"), var("criterion_id")],
            ),
        ],
    }
}

/// Open an important protocol deviation against a participant.
/// While such a claim is admitted, the participant cannot be
/// randomised - the load-bearing transformation gates on
/// `Not(ImportantProtocolDeviationOpen(...))`. v0 has no closure
/// transition; once raised the deviation stays open in this
/// example.
pub fn open_important_protocol_deviation() -> Transformation {
    Transformation {
        name: "open_important_protocol_deviation".to_string(),
        parameters: params(&["participant_id", "trial_id", "deviation_id"]),
        body: vec![
            require(claim("Trial", vec![var("trial_id")])),
            assert_(
                "ImportantProtocolDeviationOpen",
                vec![var("participant_id"), var("trial_id"), var("deviation_id")],
            ),
            emit(
                "ImportantProtocolDeviationOpened",
                vec![var("participant_id"), var("trial_id"), var("deviation_id")],
            ),
        ],
    }
}

/// Load-bearing transformation. Admits a randomisation only if all
/// of the following are valid on the `randomised_on` civil date:
///
/// - The trial is open.
/// - The named `protocol_version` belongs to this trial, has a
///   `[from, to]` window that includes the date, and has been
///   ethics-approved.
/// - A consent form version belonging to this trial has a `[from,
///   to]` window that includes the date, has been ethics-approved,
///   and the participant signed it on or before the date.
/// - A delegation for `ROLE_RANDOMISE_PARTICIPANT` from the
///   proposing actor over the date exists.
/// - The protocol has at least one eligibility criterion whose
///   matching participant assessment has a `[assessed_on,
///   expires_on]` window including the date and a `result` equal
///   to the criterion's `required_result`.
/// - No important protocol deviation is currently open against
///   this participant on this trial.
///
/// All gates live inside one `require And(...)`. `Stmt::Require` is
/// a yes/no gate that does not propagate bindings to later
/// statements; conjoining the lookups inside a single `And` is
/// what threads `protocol_from`, `protocol_to`, `consent_form_version`,
/// `consent_from`, `consent_to`, `consented_on`, `del_from`,
/// `del_to`, `criterion_id`, `required_result`, `assessed_on`,
/// `expires_on` through the `DateLe` checks. See
/// `docs/runtime-semantics.md` "Statements: gating vs binding."
///
/// `protocol_version` is an explicit transformation parameter -
/// the caller (an investigator preparing a randomisation) knows
/// which protocol version they are enrolling under. Making it a
/// parameter lets the binding flow into the `assert` cleanly
/// (`require` would otherwise discard the binding bound inside the
/// And) and matches the clinical workflow: the source document for
/// the randomisation names the protocol version it was performed
/// under.
///
/// On admission, asserts `ParticipantRandomised(participant_id,
/// trial_id, protocol_version, randomised_on, $actor)` and emits a
/// `ParticipantRandomised` outbox intent. Future amendments to the
/// protocol do not invalidate this assertion: the
/// `participant_randomised_once_per_trial` invariant pins identity
/// uniqueness, and validity is an admission-time gate, not an
/// eternal invariant.
pub fn randomise_participant() -> Transformation {
    Transformation {
        name: "randomise_participant".to_string(),
        parameters: params(&[
            "participant_id",
            "trial_id",
            "protocol_version",
            "randomised_on",
        ]),
        body: vec![
            require(and(vec![
                claim("Trial", vec![var("trial_id")]),
                claim(
                    "ProtocolVersion",
                    vec![
                        var("trial_id"),
                        var("protocol_version"),
                        var("protocol_from"),
                        var("protocol_to"),
                    ],
                ),
                date_le(term(var("protocol_from")), term(var("randomised_on"))),
                date_le(term(var("randomised_on")), term(var("protocol_to"))),
                claim(
                    "ProtocolApprovedBy",
                    vec![var("protocol_version"), wildcard(), wildcard()],
                ),
                claim(
                    "ConsentFormVersion",
                    vec![
                        var("trial_id"),
                        var("consent_form_version"),
                        var("consent_from"),
                        var("consent_to"),
                    ],
                ),
                date_le(term(var("consent_from")), term(var("randomised_on"))),
                date_le(term(var("randomised_on")), term(var("consent_to"))),
                claim(
                    "ConsentFormApprovedBy",
                    vec![var("consent_form_version"), wildcard(), wildcard()],
                ),
                claim(
                    "InformedConsentObtained",
                    vec![
                        var("participant_id"),
                        var("trial_id"),
                        var("consent_form_version"),
                        var("consented_on"),
                        wildcard(),
                    ],
                ),
                date_le(term(var("consented_on")), term(var("randomised_on"))),
                claim(
                    "DelegatedInvestigator",
                    vec![
                        actor(),
                        var("trial_id"),
                        role(ROLE_RANDOMISE_PARTICIPANT),
                        var("del_from"),
                        var("del_to"),
                    ],
                ),
                date_le(term(var("del_from")), term(var("randomised_on"))),
                date_le(term(var("randomised_on")), term(var("del_to"))),
                claim(
                    "EligibilityCriterion",
                    vec![
                        var("protocol_version"),
                        var("criterion_id"),
                        var("required_result"),
                    ],
                ),
                claim(
                    "EligibilityAssessment",
                    vec![
                        var("participant_id"),
                        var("criterion_id"),
                        var("required_result"),
                        var("assessed_on"),
                        var("expires_on"),
                    ],
                ),
                date_le(term(var("assessed_on")), term(var("randomised_on"))),
                date_le(term(var("randomised_on")), term(var("expires_on"))),
                not(claim(
                    "ImportantProtocolDeviationOpen",
                    vec![var("participant_id"), var("trial_id"), wildcard()],
                )),
            ])),
            assert_(
                "ParticipantRandomised",
                vec![
                    var("participant_id"),
                    var("trial_id"),
                    var("protocol_version"),
                    var("randomised_on"),
                    actor(),
                ],
            ),
            emit("ParticipantRandomised", vec![var("participant_id")]),
        ],
    }
}

pub fn all_invariants() -> Vec<Invariant> {
    vec![
        at_most_one_protocol_window_per_version(),
        at_most_one_consent_window_per_version(),
        participant_randomised_once_per_trial(),
    ]
}

/// The clinical-trial-enrolment example as a
/// [`morpholog_core::Program`]. Stable identifier:
/// `"clinical_trial_enrolment"`.
pub fn program() -> morpholog_core::Program {
    morpholog_core::Program {
        name: "clinical_trial_enrolment".to_string(),
        invariants: all_invariants(),
        transformations: vec![
            open_trial(),
            approve_protocol_version(),
            approve_consent_form_version(),
            delegate_investigator(),
            screen_participant(),
            record_consent(),
            record_eligibility_criterion(),
            record_eligibility_assessment(),
            open_important_protocol_deviation(),
            randomise_participant(),
        ],
        derived_claims: vec![],
    }
}
