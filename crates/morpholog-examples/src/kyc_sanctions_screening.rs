//! KYC sanctions and PEP screening: customer onboarding gated by
//! current-clean screenings, with the round-trip compute pattern
//! (request -> external provider -> result) at the heart of every
//! screening interaction. See `examples/08_kyc_sanctions_screening/README.md`
//! for the business framing.

use morpholog_core::dsl::*;
use morpholog_core::{Invariant, Transformation};

// ============================================================
// Disposition + list-type subject literals
// ============================================================

pub const SANCTIONS: &str = "sanctions";
pub const PEP: &str = "pep";

pub const DISP_CLEAN: &str = "clean";
pub const DISP_MATCH: &str = "match";
pub const DISP_ADJUDICATED_CLEAR: &str = "adjudicated_clear";

// ============================================================
// Invariants
// ============================================================

/// At most one current screening per `(customer, list_type)` pair.
/// The currentness pointer is singleton: a later screening retracts
/// the prior one and admits itself in its place.
pub fn at_most_one_current_screening_per_customer_and_list_type() -> Invariant {
    Invariant {
        name: "at_most_one_current_screening_per_customer_and_list_type".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim("CurrentScreening", vec![var("c"), var("t"), var("s_a")]),
                claim("CurrentScreening", vec![var("c"), var("t"), var("s_b")]),
            ]),
            eq(term(var("s_a")), term(var("s_b"))),
        ),
    }
}

fn onboarded_requires_current_clean(name: &str, list_type: &str) -> Invariant {
    Invariant {
        name: name.to_string(),
        version: 1,
        body: implies(
            claim("OnboardedCustomer", vec![var("c"), var("on_date")]),
            and(vec![
                claim(
                    "CurrentScreening",
                    vec![var("c"), subj(list_type), var("s")],
                ),
                or(vec![
                    claim(
                        "ScreeningResult",
                        vec![var("s"), subj(DISP_CLEAN), var("completed"), var("expires")],
                    ),
                    claim(
                        "ScreeningResult",
                        vec![
                            var("s"),
                            subj(DISP_ADJUDICATED_CLEAR),
                            var("completed"),
                            var("expires"),
                        ],
                    ),
                ]),
                date_le(term(var("on_date")), term(var("expires"))),
            ]),
        ),
    }
}

/// An onboarded customer must have a current sanctions screening
/// whose disposition is clean (or adjudicated-clear) and whose
/// expiry has not been reached by the onboarding date.
pub fn onboarded_requires_current_clean_sanctions() -> Invariant {
    onboarded_requires_current_clean("onboarded_requires_current_clean_sanctions", SANCTIONS)
}

/// Mirror for PEP screening - sanctions and PEP are distinct lists
/// with distinct legal weight; both must be current.
pub fn onboarded_requires_current_clean_pep() -> Invariant {
    onboarded_requires_current_clean("onboarded_requires_current_clean_pep", PEP)
}

/// An onboarded customer cannot have any unresolved match on ANY
/// of their screenings - not only the current standing one. The
/// join is through `Screening`, not `CurrentScreening`: a match
/// found by a re-screen must block onboarding even while an older
/// clean screening still holds the currentness pointer. A hit
/// must be adjudicated, never simply superseded by a later clean
/// result.
pub fn onboarded_requires_no_unresolved_match() -> Invariant {
    Invariant {
        name: "onboarded_requires_no_unresolved_match".to_string(),
        version: 1,
        body: implies(
            claim("OnboardedCustomer", vec![var("c"), wildcard()]),
            not(and(vec![
                claim(
                    "Screening",
                    vec![var("s"), var("c"), wildcard(), wildcard()],
                ),
                claim("MatchUnderReview", vec![var("s"), wildcard()]),
            ])),
        ),
    }
}

pub fn all_invariants() -> Vec<Invariant> {
    vec![
        at_most_one_current_screening_per_customer_and_list_type(),
        onboarded_requires_current_clean_sanctions(),
        onboarded_requires_current_clean_pep(),
        onboarded_requires_no_unresolved_match(),
    ]
}

// ============================================================
// Transformations
// ============================================================

pub fn register_customer() -> Transformation {
    Transformation {
        name: "register_customer".to_string(),
        parameters: params(&["customer_id"]),
        body: vec![
            require(not(claim("Customer", vec![var("customer_id")]))),
            assert_("Customer", vec![var("customer_id")]),
        ],
    }
}

pub fn request_screening() -> Transformation {
    Transformation {
        name: "request_screening".to_string(),
        parameters: params(&["screening_id", "customer", "list_type", "requested_on"]),
        body: vec![
            require(claim("Customer", vec![var("customer")])),
            assert_(
                "Screening",
                vec![
                    var("screening_id"),
                    var("customer"),
                    var("list_type"),
                    var("requested_on"),
                ],
            ),
            emit(
                "ScreeningRequested",
                vec![var("screening_id"), var("customer"), var("list_type")],
            ),
        ],
    }
}

pub fn record_clean_screening_result() -> Transformation {
    Transformation {
        name: "record_clean_screening_result".to_string(),
        parameters: params(&["screening_id", "completed_on", "expires_on"]),
        body: vec![
            bind_one(claim(
                "Screening",
                vec![
                    var("screening_id"),
                    var("customer"),
                    var("list_type"),
                    wildcard(),
                ],
            )),
            assert_(
                "ScreeningResult",
                vec![
                    var("screening_id"),
                    subj(DISP_CLEAN),
                    var("completed_on"),
                    var("expires_on"),
                ],
            ),
            retract(
                "CurrentScreening",
                vec![var("customer"), var("list_type"), wildcard()],
            ),
            assert_(
                "CurrentScreening",
                vec![var("customer"), var("list_type"), var("screening_id")],
            ),
        ],
    }
}

pub fn record_match_screening_result() -> Transformation {
    Transformation {
        name: "record_match_screening_result".to_string(),
        parameters: params(&["screening_id", "completed_on", "expires_on", "raised_on"]),
        body: vec![
            bind_one(claim(
                "Screening",
                vec![
                    var("screening_id"),
                    var("customer"),
                    var("list_type"),
                    wildcard(),
                ],
            )),
            assert_(
                "ScreeningResult",
                vec![
                    var("screening_id"),
                    subj(DISP_MATCH),
                    var("completed_on"),
                    var("expires_on"),
                ],
            ),
            assert_(
                "MatchUnderReview",
                vec![var("screening_id"), var("raised_on")],
            ),
            emit(
                "MatchRaised",
                vec![var("screening_id"), var("customer"), var("list_type")],
            ),
        ],
    }
}

pub fn adjudicate_match_as_false_positive() -> Transformation {
    Transformation {
        name: "adjudicate_match_as_false_positive".to_string(),
        parameters: params(&["screening_id", "adjudicated_on", "expires_on"]),
        body: vec![
            require(claim(
                "MatchUnderReview",
                vec![var("screening_id"), wildcard()],
            )),
            bind_one(claim(
                "Screening",
                vec![
                    var("screening_id"),
                    var("customer"),
                    var("list_type"),
                    wildcard(),
                ],
            )),
            assert_(
                "ScreeningResult",
                vec![
                    var("screening_id"),
                    subj(DISP_ADJUDICATED_CLEAR),
                    var("adjudicated_on"),
                    var("expires_on"),
                ],
            ),
            retract("MatchUnderReview", vec![var("screening_id"), wildcard()]),
            retract(
                "CurrentScreening",
                vec![var("customer"), var("list_type"), wildcard()],
            ),
            assert_(
                "CurrentScreening",
                vec![var("customer"), var("list_type"), var("screening_id")],
            ),
        ],
    }
}

pub fn onboard_customer() -> Transformation {
    Transformation {
        name: "onboard_customer".to_string(),
        parameters: params(&["customer", "onboarded_on"]),
        body: vec![
            require(claim("Customer", vec![var("customer")])),
            require(not(claim(
                "OnboardedCustomer",
                vec![var("customer"), wildcard()],
            ))),
            assert_(
                "OnboardedCustomer",
                vec![var("customer"), var("onboarded_on")],
            ),
            emit(
                "CustomerOnboarded",
                vec![var("customer"), var("onboarded_on")],
            ),
        ],
    }
}

pub fn reject_customer() -> Transformation {
    Transformation {
        name: "reject_customer".to_string(),
        parameters: params(&["customer", "reason"]),
        body: vec![
            require(claim("Customer", vec![var("customer")])),
            emit("CustomerRejected", vec![var("customer"), var("reason")]),
        ],
    }
}

pub fn all_transformations() -> Vec<Transformation> {
    vec![
        register_customer(),
        request_screening(),
        record_clean_screening_result(),
        record_match_screening_result(),
        adjudicate_match_as_false_positive(),
        onboard_customer(),
        reject_customer(),
    ]
}

pub fn all_predicates() -> Vec<morpholog_core::PredicateDecl> {
    vec![
        predicate("Customer").subject("customer_id").build(),
        predicate("Screening")
            .subject("screening_id")
            .subject("customer")
            .subject("list_type")
            .date("requested_on")
            .build(),
        predicate("ScreeningResult")
            .subject("screening_id")
            .subject("disposition")
            .date("completed_on")
            .date("expires_on")
            .build(),
        predicate("CurrentScreening")
            .subject("customer")
            .subject("list_type")
            .subject("screening_id")
            .build(),
        predicate("MatchUnderReview")
            .subject("screening_id")
            .date("raised_on")
            .build(),
        predicate("OnboardedCustomer")
            .subject("customer")
            .date("onboarded_on")
            .build(),
    ]
}

pub fn all_intents() -> Vec<morpholog_core::IntentDecl> {
    vec![
        intent_decl("ScreeningRequested")
            .subject("screening_id")
            .subject("customer")
            .subject("list_type")
            .build(),
        intent_decl("MatchRaised")
            .subject("screening_id")
            .subject("customer")
            .subject("list_type")
            .build(),
        intent_decl("CustomerOnboarded")
            .subject("customer")
            .date("onboarded_on")
            .build(),
        intent_decl("CustomerRejected")
            .subject("customer")
            .subject("reason")
            .build(),
    ]
}

/// The KYC sanctions/PEP screening example as a
/// [`morpholog_core::Program`]. Stable identifier:
/// `"kyc_sanctions_screening"`.
pub fn program() -> morpholog_core::Program {
    morpholog_core::Program {
        name: "kyc_sanctions_screening".to_string(),
        predicates: all_predicates(),
        intents: all_intents(),
        invariants: all_invariants(),
        transformations: all_transformations(),
        derived_claims: vec![],
    }
}
