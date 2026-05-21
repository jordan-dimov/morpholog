//! Verified revenue: currentness-with-restatement + admissibility-for-purpose
//! in one programme. See `examples/02_verified_revenue/README.md` for the
//! business framing; this module is the IR.

use morpholog_core::{Invariant, Transformation};

use morpholog_core::dsl::*;

/// The purpose subject identifying bank debt-service-coverage usage.
pub const BANK_DEBT_SERVICE: &str = "bank_debt_service";

/// The purpose subject identifying investor reporting usage.
pub const INVESTOR_REPORTING: &str = "investor_reporting";

// ============================================================
// Invariants. No invariant ties decision claims to live
// AdmissibleFor - decisions are gated at admission time via
// `require` so historical decisions survive later revocation.
// ============================================================

/// Every active `AdmissibleFor(v, p)` must be backed by some
/// `StandingGrantedBy(v, p, _, _)` claim.
pub fn admissibility_has_provenance() -> Invariant {
    Invariant {
        name: "admissibility_has_provenance".to_string(),
        version: 1,
        body: implies(
            claim("AdmissibleFor", vec![var("v"), var("p")]),
            claim(
                "StandingGrantedBy",
                vec![var("v"), var("p"), wildcard(), wildcard()],
            ),
        ),
    }
}

/// `AdmissibleFor(v, p)` cannot coexist with any
/// `StandingRevoked(v, p, _)`. Revocation is terminal in v0.
pub fn admissibility_excludes_revocation() -> Invariant {
    Invariant {
        name: "admissibility_excludes_revocation".to_string(),
        version: 1,
        body: implies(
            claim("AdmissibleFor", vec![var("v"), var("p")]),
            not(exists(
                "r",
                claim("StandingRevoked", vec![var("v"), var("p"), var("r")]),
            )),
        ),
    }
}

/// At most one `CurrentVerification` per `(asset, period)`. The
/// pointer is singleton.
pub fn at_most_one_current_verification_per_asset_period() -> Invariant {
    Invariant {
        name: "at_most_one_current_verification_per_asset_period".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim(
                    "CurrentVerification",
                    vec![var("asset"), var("period"), var("a")],
                ),
                claim(
                    "CurrentVerification",
                    vec![var("asset"), var("period"), var("b")],
                ),
            ]),
            eq(term(var("a")), term(var("b"))),
        ),
    }
}

/// A verification can be superseded by at most one direct successor.
/// Parallel restatement chains are forbidden by construction.
pub fn at_most_one_direct_successor() -> Invariant {
    Invariant {
        name: "at_most_one_direct_successor".to_string(),
        version: 1,
        body: implies(
            and(vec![
                claim("Supersedes", vec![var("new_a"), var("old")]),
                claim("Supersedes", vec![var("new_b"), var("old")]),
            ]),
            eq(term(var("new_a")), term(var("new_b"))),
        ),
    }
}

// ============================================================
// Transformations
// ============================================================

/// First admission of an independent verification figure for an
/// `(asset, period)`. Rejected if there is already a current
/// verification for this asset/period - use
/// `correct_independent_verification` to update.
pub fn admit_independent_verification() -> Transformation {
    Transformation {
        name: "admit_independent_verification".to_string(),
        parameters: params(&["asset", "period", "amount", "verification_id"]),
        body: vec![
            require(not(claim(
                "CurrentVerification",
                vec![var("asset"), var("period"), wildcard()],
            ))),
            assert_(
                "IndependentlyVerifiedRevenue",
                vec![
                    var("asset"),
                    var("period"),
                    var("amount"),
                    var("verification_id"),
                ],
            ),
            assert_(
                "CurrentVerification",
                vec![var("asset"), var("period"), var("verification_id")],
            ),
            emit(
                "IndependentVerificationAdmitted",
                vec![var("verification_id")],
            ),
        ],
    }
}

/// Correct a previously-admitted verification figure. The original
/// stays in admitted state; the new figure is admitted; lineage is
/// recorded as `Supersedes`; the singleton pointer moves to the new
/// verification; and **any standing previously granted on the prior
/// verification is retracted** - authorities must re-issue standing
/// on the corrected figure if they still agree with it.
///
/// Existing decisions admitted under the prior standing survive in
/// admitted state. The runtime does not cascade-retract them.
pub fn correct_independent_verification() -> Transformation {
    Transformation {
        name: "correct_independent_verification".to_string(),
        parameters: params(&[
            "asset",
            "period",
            "new_amount",
            "new_verification_id",
            "prior_verification_id",
        ]),
        body: vec![
            require(claim(
                "IndependentlyVerifiedRevenue",
                vec![
                    var("asset"),
                    var("period"),
                    wildcard(),
                    var("prior_verification_id"),
                ],
            )),
            require(not(claim(
                "Supersedes",
                vec![wildcard(), var("prior_verification_id")],
            ))),
            assert_(
                "IndependentlyVerifiedRevenue",
                vec![
                    var("asset"),
                    var("period"),
                    var("new_amount"),
                    var("new_verification_id"),
                ],
            ),
            assert_(
                "Supersedes",
                vec![var("new_verification_id"), var("prior_verification_id")],
            ),
            retract(
                "CurrentVerification",
                vec![var("asset"), var("period"), var("prior_verification_id")],
            ),
            // Pattern-based retraction: every active AdmissibleFor for
            // the prior verification, for any purpose. Authorities
            // must re-issue standing if they accept the correction.
            retract(
                "AdmissibleFor",
                vec![var("prior_verification_id"), wildcard()],
            ),
            assert_(
                "CurrentVerification",
                vec![var("asset"), var("period"), var("new_verification_id")],
            ),
            emit(
                "VerificationCorrected",
                vec![var("new_verification_id"), var("prior_verification_id")],
            ),
        ],
    }
}

/// Grant `purpose` standing on `verification_id`, recording
/// `authority` as the granting party and `grant_id` as the provenance
/// handle. Admission requires the verification to exist, to be
/// current, to have no prior revocation for this purpose, and to have
/// no active admissibility yet.
pub fn grant_standing() -> Transformation {
    Transformation {
        name: "grant_standing".to_string(),
        parameters: params(&["verification_id", "purpose", "authority", "grant_id"]),
        body: vec![
            require(claim(
                "IndependentlyVerifiedRevenue",
                vec![wildcard(), wildcard(), wildcard(), var("verification_id")],
            )),
            require(claim(
                "CurrentVerification",
                vec![wildcard(), wildcard(), var("verification_id")],
            )),
            require(not(exists(
                "r",
                claim(
                    "StandingRevoked",
                    vec![var("verification_id"), var("purpose"), var("r")],
                ),
            ))),
            require(not(claim(
                "AdmissibleFor",
                vec![var("verification_id"), var("purpose")],
            ))),
            assert_(
                "StandingGrantedBy",
                vec![
                    var("verification_id"),
                    var("purpose"),
                    var("authority"),
                    var("grant_id"),
                ],
            ),
            assert_(
                "AdmissibleFor",
                vec![var("verification_id"), var("purpose")],
            ),
            emit("StandingGranted", vec![var("grant_id")]),
        ],
    }
}

/// Revoke `purpose` standing for `verification_id`. The historical
/// `StandingGrantedBy` claim survives; the active `AdmissibleFor` is
/// retracted; a new `StandingRevoked` is admitted (terminal in v0).
pub fn revoke_standing() -> Transformation {
    Transformation {
        name: "revoke_standing".to_string(),
        parameters: params(&["verification_id", "purpose", "revocation_id"]),
        body: vec![
            require(claim(
                "AdmissibleFor",
                vec![var("verification_id"), var("purpose")],
            )),
            retract(
                "AdmissibleFor",
                vec![var("verification_id"), var("purpose")],
            ),
            assert_(
                "StandingRevoked",
                vec![var("verification_id"), var("purpose"), var("revocation_id")],
            ),
            emit("StandingRevocationAdmitted", vec![var("revocation_id")]),
        ],
    }
}

/// Admit a `DebtServiceRevenue` decision that relies on a specific
/// `IndependentlyVerifiedRevenue` claim with active standing for the
/// `bank_debt_service` purpose.
pub fn admit_debt_service_revenue() -> Transformation {
    Transformation {
        name: "admit_debt_service_revenue".to_string(),
        parameters: params(&[
            "asset",
            "period",
            "amount",
            "decision_id",
            "verification_id",
        ]),
        body: vec![
            require(claim(
                "IndependentlyVerifiedRevenue",
                vec![
                    var("asset"),
                    var("period"),
                    var("amount"),
                    var("verification_id"),
                ],
            )),
            require(claim(
                "AdmissibleFor",
                vec![var("verification_id"), subj(BANK_DEBT_SERVICE)],
            )),
            assert_(
                "DebtServiceRevenue",
                vec![
                    var("asset"),
                    var("period"),
                    var("amount"),
                    var("decision_id"),
                    var("verification_id"),
                ],
            ),
            emit("DebtServiceRevenueAdmitted", vec![var("decision_id")]),
        ],
    }
}

/// Admit an `InvestorReportedRevenue` decision that relies on a
/// specific `IndependentlyVerifiedRevenue` claim with active standing
/// for the `investor_reporting` purpose.
pub fn admit_investor_reported_revenue() -> Transformation {
    Transformation {
        name: "admit_investor_reported_revenue".to_string(),
        parameters: params(&["asset", "period", "amount", "report_id", "verification_id"]),
        body: vec![
            require(claim(
                "IndependentlyVerifiedRevenue",
                vec![
                    var("asset"),
                    var("period"),
                    var("amount"),
                    var("verification_id"),
                ],
            )),
            require(claim(
                "AdmissibleFor",
                vec![var("verification_id"), subj(INVESTOR_REPORTING)],
            )),
            assert_(
                "InvestorReportedRevenue",
                vec![
                    var("asset"),
                    var("period"),
                    var("amount"),
                    var("report_id"),
                    var("verification_id"),
                ],
            ),
            emit("InvestorReportedRevenueAdmitted", vec![var("report_id")]),
        ],
    }
}

pub fn all_invariants() -> Vec<Invariant> {
    vec![
        admissibility_has_provenance(),
        admissibility_excludes_revocation(),
        at_most_one_current_verification_per_asset_period(),
        at_most_one_direct_successor(),
    ]
}

/// The verified-revenue example as a [`morpholog_core::Program`].
/// Stable identifier: `"verified_revenue"`.
pub fn program() -> morpholog_core::Program {
    morpholog_core::Program {
        name: "verified_revenue".to_string(),
        invariants: all_invariants(),
        transformations: vec![
            admit_independent_verification(),
            correct_independent_verification(),
            grant_standing(),
            revoke_standing(),
            admit_debt_service_revenue(),
            admit_investor_reported_revenue(),
        ],
        derived_claims: vec![],
    }
}
