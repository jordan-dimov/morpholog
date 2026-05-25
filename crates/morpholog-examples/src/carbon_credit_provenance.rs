//! Carbon-credit / certificate-of-origin provenance IR.
//!
//! A carbon credit is not primarily a calculation - it is an *official
//! standing claim*: this credit exists, is backed by admissible
//! provenance, is held by one party, has not been double-counted, and
//! has not been retired twice. This example governs the admission of that
//! standing, never the measurement behind it: the MRV maths (generation x
//! emission factor -> tonnes) is the meter, it stays outside Morpholog
//! and returns as the admitted `VerifiedMeasurement` quantity.
//!
//! The provenance chain is modelled as ordinary claims about claims - no
//! kernel primitive. Issuance is gated on a verified measurement, an
//! attestation of that measurement, and a *currently* accredited verifier;
//! revoking accreditation blocks new issuance without disturbing credits
//! already issued (the verified-revenue currentness pattern). See
//! `examples/09_carbon_credit_provenance/README.md` for the business
//! framing and the deliberate "one credit per measurement" simplification.

use morpholog_core::Invariant;
use morpholog_core::Transformation;
use morpholog_core::dsl::*;

// ============================================================
// Vocabulary
// ============================================================

pub fn all_predicates() -> Vec<morpholog_core::PredicateDecl> {
    vec![
        // A verifier currently authorised to attest measurements.
        predicate("Accredited").subject("verifier").build(),
        // The MRV result, admitted as evidence: this measurement is
        // verified to `quantity` tonnes. The computation is external.
        predicate("VerifiedMeasurement")
            .subject("measurement")
            .decimal("quantity")
            .build(),
        // A verifier's attestation of the admitted measurement (including
        // its quantity - the attestation is over the measurement claim).
        predicate("Attestation")
            .subject("measurement")
            .subject("verifier")
            .build(),
        // A credit's official standing, backed by one measurement.
        predicate("Issued")
            .subject("credit")
            .subject("measurement")
            .decimal("quantity")
            .build(),
        // Current custody of a credit.
        predicate("HeldBy")
            .subject("credit")
            .subject("account")
            .build(),
        // A credit retired (cancelled) by an account. Terminal.
        predicate("Retired")
            .subject("credit")
            .subject("account")
            .build(),
        // A compliance obligation: `account` must retire `quantity`
        // tonnes of credits by `due_on`.
        predicate("Obligation")
            .subject("obligation")
            .subject("account")
            .decimal("quantity")
            .date("due_on")
            .build(),
        // The obligation has been discharged - enough retired in time.
        predicate("ObligationSatisfied")
            .subject("obligation")
            .build(),
        // The obligation was missed - past due and under target.
        predicate("ObligationBreached")
            .subject("obligation")
            .build(),
    ]
}

// ============================================================
// Invariants - the states this model makes impossible
// ============================================================

/// At most one verified quantity per measurement. Pins the structural
/// uniqueness that `issue_credit`'s `bind VerifiedMeasurement(...)`
/// depends on: two conflicting verified quantities for one measurement
/// would make the bound quantity ambiguous.
pub fn at_most_one_verified_quantity_per_measurement() -> Invariant {
    Invariant {
        name: "at_most_one_verified_quantity_per_measurement".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim("VerifiedMeasurement", vec![var("m"), var("q1")]),
                claim("VerifiedMeasurement", vec![var("m"), var("q2")]),
            ]),
            eq(term(var("q1")), term(var("q2"))),
        ),
    }
}

/// No double counting: at most one credit may be issued against any one
/// measurement. Two distinct credits backing the same measurement is the
/// canonical double-counting failure, and this makes it uncommittable.
pub fn no_double_issuance() -> Invariant {
    Invariant {
        name: "no_double_issuance".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim("Issued", vec![var("c1"), var("m"), var("q1")]),
                claim("Issued", vec![var("c2"), var("m"), var("q2")]),
            ]),
            eq(term(var("c1")), term(var("c2"))),
        ),
    }
}

/// The converse of `no_double_issuance`: a credit is backed by exactly
/// one measurement, and so carries one quantity. Together with
/// `no_double_issuance` this makes `Issued` a one-to-one correspondence
/// between credits and measurements - the precise meaning of "one credit
/// per measurement", in both directions.
pub fn credit_backed_by_one_measurement() -> Invariant {
    Invariant {
        name: "credit_backed_by_one_measurement".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim("Issued", vec![var("c"), var("m1"), var("q1")]),
                claim("Issued", vec![var("c"), var("m2"), var("q2")]),
            ]),
            and(vec![
                eq(term(var("m1")), term(var("m2"))),
                eq(term(var("q1")), term(var("q2"))),
            ]),
        ),
    }
}

/// A credit is held by at most one account at a time. Custody is
/// single-valued; a credit cannot be in two places at once.
pub fn single_custody() -> Invariant {
    Invariant {
        name: "single_custody".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim("HeldBy", vec![var("credit"), var("a1")]),
                claim("HeldBy", vec![var("credit"), var("a2")]),
            ]),
            eq(term(var("a1")), term(var("a2"))),
        ),
    }
}

/// Retirement is terminal: no credit is both retired and held. Retirement
/// retracts custody, so a retired credit can never be transferred or
/// retired again.
pub fn retirement_terminal() -> Invariant {
    Invariant {
        name: "retirement_terminal".to_string(),
        version: 1,
        body: not(and(vec![
            claim("Retired", vec![var("credit"), var("ra")]),
            claim("HeldBy", vec![var("credit"), var("ha")]),
        ])),
    }
}

/// At most one obligation per id. Pins the structural uniqueness that
/// `discharge_obligation` and `sweep_obligation`'s
/// `bind Obligation(obligation, ...)` depend on.
pub fn at_most_one_obligation_per_id() -> Invariant {
    Invariant {
        name: "at_most_one_obligation_per_id".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim(
                    "Obligation",
                    vec![var("o"), var("a1"), var("q1"), var("d1")],
                ),
                claim(
                    "Obligation",
                    vec![var("o"), var("a2"), var("q2"), var("d2")],
                ),
            ]),
            and(vec![
                eq(term(var("a1")), term(var("a2"))),
                eq(term(var("q1")), term(var("q2"))),
                eq(term(var("d1")), term(var("d2"))),
            ]),
        ),
    }
}

/// An obligation is never both satisfied and breached. The two outcomes
/// are mutually exclusive - discharge gates on not-breached and the sweep
/// gates on not-satisfied - and this makes the joint state uncommittable
/// regardless of how it might be reached.
pub fn obligation_not_both_satisfied_and_breached() -> Invariant {
    Invariant {
        name: "obligation_not_both_satisfied_and_breached".to_string(),
        version: 1,
        body: not(and(vec![
            claim("ObligationSatisfied", vec![var("o")]),
            claim("ObligationBreached", vec![var("o")]),
        ])),
    }
}

pub fn all_invariants() -> Vec<Invariant> {
    vec![
        at_most_one_verified_quantity_per_measurement(),
        no_double_issuance(),
        credit_backed_by_one_measurement(),
        single_custody(),
        retirement_terminal(),
        at_most_one_obligation_per_id(),
        obligation_not_both_satisfied_and_breached(),
    ]
}

// ============================================================
// Provenance: accreditation, verification, attestation
// ============================================================

pub fn grant_accreditation() -> Transformation {
    Transformation {
        name: "grant_accreditation".to_string(),
        parameters: params(&["verifier"]),
        body: vec![assert_("Accredited", vec![var("verifier")])],
    }
}

/// Revoking accreditation retracts the verifier's *current* standing.
/// New issuance through that verifier then fails, but credits already
/// issued keep their standing - currentness without rewriting history.
pub fn revoke_accreditation() -> Transformation {
    Transformation {
        name: "revoke_accreditation".to_string(),
        parameters: params(&["verifier"]),
        body: vec![
            require(claim("Accredited", vec![var("verifier")])),
            retract("Accredited", vec![var("verifier")]),
        ],
    }
}

/// Admit the MRV result as evidence. The measurement quantity is computed
/// outside Morpholog (the meter) and enters here as an admitted claim.
pub fn verify_measurement() -> Transformation {
    Transformation {
        name: "verify_measurement".to_string(),
        parameters: params(&["measurement", "quantity"]),
        body: vec![assert_(
            "VerifiedMeasurement",
            vec![var("measurement"), var("quantity")],
        )],
    }
}

/// Only a currently accredited verifier may attest a measurement.
pub fn attest_measurement() -> Transformation {
    Transformation {
        name: "attest_measurement".to_string(),
        parameters: params(&["measurement", "verifier"]),
        body: vec![
            require(claim("Accredited", vec![var("verifier")])),
            assert_("Attestation", vec![var("measurement"), var("verifier")]),
        ],
    }
}

// ============================================================
// Lifecycle: issue, transfer, retire
// ============================================================

/// Issue a credit against a measurement. The gate is the legitimacy
/// chain: the measurement must be verified (binding its quantity), and a
/// *currently* accredited verifier must have attested it. A missing link
/// is exactly what `explain` surfaces, naming the transformation that
/// would supply it.
pub fn issue_credit() -> Transformation {
    Transformation {
        name: "issue_credit".to_string(),
        parameters: params(&["credit", "measurement", "verifier", "account"]),
        body: vec![
            bind_one(claim(
                "VerifiedMeasurement",
                vec![var("measurement"), var("quantity")],
            )),
            require(and(vec![
                claim("Attestation", vec![var("measurement"), var("verifier")]),
                claim("Accredited", vec![var("verifier")]),
            ])),
            assert_(
                "Issued",
                vec![var("credit"), var("measurement"), var("quantity")],
            ),
            assert_("HeldBy", vec![var("credit"), var("account")]),
        ],
    }
}

/// Transfer custody. A retired credit cannot be transferred (the
/// `not Retired` gate is the present-blocker), and the sender must
/// currently hold it.
pub fn transfer_credit() -> Transformation {
    Transformation {
        name: "transfer_credit".to_string(),
        parameters: params(&["credit", "from_account", "to_account"]),
        body: vec![
            require(not(claim("Retired", vec![var("credit"), wildcard()]))),
            require(claim("HeldBy", vec![var("credit"), var("from_account")])),
            retract("HeldBy", vec![var("credit"), var("from_account")]),
            assert_("HeldBy", vec![var("credit"), var("to_account")]),
        ],
    }
}

/// Retire (cancel) a credit. Terminal: the `not Retired` gate blocks a
/// second retirement, and retracting custody makes the
/// `retirement_terminal` invariant hold.
pub fn retire_credit() -> Transformation {
    Transformation {
        name: "retire_credit".to_string(),
        parameters: params(&["credit", "account"]),
        body: vec![
            require(not(claim("Retired", vec![var("credit"), wildcard()]))),
            require(claim("HeldBy", vec![var("credit"), var("account")])),
            retract("HeldBy", vec![var("credit"), var("account")]),
            assert_("Retired", vec![var("credit"), var("account")]),
        ],
    }
}

// ============================================================
// Obligations over time: raise, discharge, and the outside-coordinator
// breach sweep. "Now" enters as an argument - the kernel has no clock.
// ============================================================

/// Raise a compliance obligation: `account` must retire `quantity` tonnes
/// of credits by `due_on`.
pub fn raise_obligation() -> Transformation {
    Transformation {
        name: "raise_obligation".to_string(),
        parameters: params(&["obligation", "account", "quantity", "due_on"]),
        body: vec![assert_(
            "Obligation",
            vec![
                var("obligation"),
                var("account"),
                var("quantity"),
                var("due_on"),
            ],
        )],
    }
}

/// Discharge an obligation once the account has retired enough. The
/// retired total sums the issued quantity of every credit the account has
/// retired. A breached obligation cannot be discharged - the gate keeps
/// the two outcomes mutually exclusive.
pub fn discharge_obligation() -> Transformation {
    Transformation {
        name: "discharge_obligation".to_string(),
        parameters: params(&["obligation"]),
        body: vec![
            bind_one(claim(
                "Obligation",
                vec![
                    var("obligation"),
                    var("account"),
                    var("quantity"),
                    var("due_on"),
                ],
            )),
            require(not(claim("ObligationBreached", vec![var("obligation")]))),
            require(ge(
                sum(
                    var("q"),
                    and(vec![
                        claim("Retired", vec![var("c"), var("account")]),
                        claim("Issued", vec![var("c"), var("m"), var("q")]),
                    ]),
                ),
                term(var("quantity")),
            )),
            assert_("ObligationSatisfied", vec![var("obligation")]),
        ],
    }
}

/// The outside-coordinator breach sweep. An external scheduler invokes
/// this with the current date - the kernel keeps no clock of its own. An
/// obligation past its due date, not already satisfied, whose account has
/// not retired enough, is recorded as breached.
pub fn sweep_obligation() -> Transformation {
    Transformation {
        name: "sweep_obligation".to_string(),
        parameters: params(&["obligation", "current_date"]),
        body: vec![
            bind_one(claim(
                "Obligation",
                vec![
                    var("obligation"),
                    var("account"),
                    var("quantity"),
                    var("due_on"),
                ],
            )),
            require(not(claim("ObligationSatisfied", vec![var("obligation")]))),
            require(date_gt(term(var("current_date")), term(var("due_on")))),
            require(lt(
                sum(
                    var("q"),
                    and(vec![
                        claim("Retired", vec![var("c"), var("account")]),
                        claim("Issued", vec![var("c"), var("m"), var("q")]),
                    ]),
                ),
                term(var("quantity")),
            )),
            assert_("ObligationBreached", vec![var("obligation")]),
        ],
    }
}

/// The carbon-credit provenance example as a [`morpholog_core::Program`].
/// Stable identifier: `"carbon_credit_provenance"`.
pub fn program() -> morpholog_core::Program {
    morpholog_core::Program {
        name: "carbon_credit_provenance".to_string(),
        predicates: all_predicates(),
        intents: vec![],
        invariants: all_invariants(),
        transformations: vec![
            grant_accreditation(),
            revoke_accreditation(),
            verify_measurement(),
            attest_measurement(),
            issue_credit(),
            transfer_credit(),
            retire_credit(),
            raise_obligation(),
            discharge_obligation(),
            sweep_obligation(),
        ],
        derived_claims: vec![],
    }
}
