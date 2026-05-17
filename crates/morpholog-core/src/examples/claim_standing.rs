//! Claim standing — admissibility-for-purpose example IR.
//!
//! Surface-syntax form: `examples/03_claim_standing/standing.morph`.
//!
//! Proves that the runtime can model **purpose-specific standing**: the
//! same underlying claim can carry different admissibility for different
//! decisions, granted by different authorities, lost without mutating
//! the claim itself.
//!
//! Continues the battery-revenue world of
//! [`super::revenue_restatement`]: `IndependentlyVerifiedRevenue`
//! claims are reused with the same predicate name and the same arity.
//! The IR fixtures are not shared between modules; each example
//! re-declares the IR it needs.
//!
//! Purposes (`bank_debt_service`, `investor_reporting`) are embedded
//! as literal subjects via `Term::Literal(Value::Subject(_))`, which
//! is what motivated adding `Value::Subject` to the IR.

use crate::{Claim, Expr, Intent, Invariant, Stmt, Term, Transformation, Value};

fn var(name: &str) -> Term {
    Term::Var(name.to_string())
}

fn purpose(name: &str) -> Term {
    Term::Literal(Value::Subject(name.to_string()))
}

/// The purpose subject identifying bank debt-service-coverage usage.
pub const BANK_DEBT_SERVICE: &str = "bank_debt_service";

/// The purpose subject identifying investor reporting usage.
pub const INVESTOR_REPORTING: &str = "investor_reporting";

// ============================================================
// Invariants — govern the standing claims themselves.
//
// Deliberately no invariant ties decision claims (DebtServiceRevenue,
// InvestorReportedRevenue) to AdmissibleFor. That gating happens at
// admission time via `require` in the decision transformations.
// Once admitted, a decision is locked in — historical decisions
// survive standing revocation because no invariant invalidates them.
// This matches real-world semantics: a calculation made under valid
// standing at time T stays valid even after standing is revoked at
// T + 1.
// ============================================================

/// Every active `AdmissibleFor(c, p)` must be backed by some
/// `StandingGrantedBy(c, p, _, _)` claim. Admissibility cannot exist
/// without provenance.
pub fn admissibility_has_provenance() -> Invariant {
    Invariant {
        name: "admissibility_has_provenance".to_string(),
        version: 1,
        body: Expr::Implies {
            left: Box::new(Expr::Claim {
                predicate: "AdmissibleFor".to_string(),
                args: vec![var("c"), var("p")],
            }),
            right: Box::new(Expr::Claim {
                predicate: "StandingGrantedBy".to_string(),
                args: vec![var("c"), var("p"), Term::Wildcard, Term::Wildcard],
            }),
        },
    }
}

/// `AdmissibleFor(c, p)` cannot coexist with any
/// `StandingRevoked(c, p, _)`. Once revoked, the active admissibility
/// must have been retracted (the `revoke_standing` transformation
/// does this; this invariant catches accidental inconsistency).
pub fn admissibility_excludes_revocation() -> Invariant {
    Invariant {
        name: "admissibility_excludes_revocation".to_string(),
        version: 1,
        body: Expr::Implies {
            left: Box::new(Expr::Claim {
                predicate: "AdmissibleFor".to_string(),
                args: vec![var("c"), var("p")],
            }),
            right: Box::new(Expr::Not(Box::new(Expr::Exists {
                binding: "r".to_string(),
                body: Box::new(Expr::Claim {
                    predicate: "StandingRevoked".to_string(),
                    args: vec![var("c"), var("p"), var("r")],
                }),
            }))),
        },
    }
}

// ============================================================
// Transformations
// ============================================================

/// First admission of an independent verification.
///
/// Same shape as the corresponding transformation in
/// `revenue_restatement`; re-declared here to keep the example
/// fixtures self-contained.
pub fn admit_independent_verification() -> Transformation {
    Transformation {
        name: "admit_independent_verification".to_string(),
        parameters: vec![
            "asset".to_string(),
            "period".to_string(),
            "amount".to_string(),
            "verification_id".to_string(),
        ],
        body: vec![
            Stmt::Assert(Claim {
                predicate: "IndependentlyVerifiedRevenue".to_string(),
                args: vec![
                    var("asset"),
                    var("period"),
                    var("amount"),
                    var("verification_id"),
                ],
            }),
            Stmt::Emit(Intent {
                name: "IndependentVerificationAdmitted".to_string(),
                args: vec![var("verification_id")],
            }),
        ],
    }
}

/// Grant `purpose` standing to the claim identified by `claim_id`,
/// recording `authority` as the granting party and `grant_id` as
/// the provenance handle.
///
/// Rejected if the (claim, purpose) pair has ever been revoked
/// (revocation is terminal in v0) or already has active
/// admissibility (no double-grant).
///
/// Purpose is a transformation parameter because `grant_standing`
/// is generic over purposes; the decision transformations below
/// embed their specific purposes as literals.
pub fn grant_standing() -> Transformation {
    Transformation {
        name: "grant_standing".to_string(),
        parameters: vec![
            "claim_id".to_string(),
            "purpose".to_string(),
            "authority".to_string(),
            "grant_id".to_string(),
        ],
        body: vec![
            Stmt::Require(Expr::Not(Box::new(Expr::Exists {
                binding: "r".to_string(),
                body: Box::new(Expr::Claim {
                    predicate: "StandingRevoked".to_string(),
                    args: vec![var("claim_id"), var("purpose"), var("r")],
                }),
            }))),
            Stmt::Require(Expr::Not(Box::new(Expr::Claim {
                predicate: "AdmissibleFor".to_string(),
                args: vec![var("claim_id"), var("purpose")],
            }))),
            Stmt::Assert(Claim {
                predicate: "StandingGrantedBy".to_string(),
                args: vec![
                    var("claim_id"),
                    var("purpose"),
                    var("authority"),
                    var("grant_id"),
                ],
            }),
            Stmt::Assert(Claim {
                predicate: "AdmissibleFor".to_string(),
                args: vec![var("claim_id"), var("purpose")],
            }),
            Stmt::Emit(Intent {
                name: "StandingGranted".to_string(),
                args: vec![var("grant_id")],
            }),
        ],
    }
}

/// Revoke `purpose` standing for the claim identified by `claim_id`,
/// recording `revocation_id` as the provenance handle.
///
/// Rejected if there is no current admissibility to revoke. The
/// underlying claim is not touched. The historical
/// `StandingGrantedBy` claim survives; the active `AdmissibleFor`
/// is retracted; a new `StandingRevoked` claim is admitted.
pub fn revoke_standing() -> Transformation {
    Transformation {
        name: "revoke_standing".to_string(),
        parameters: vec![
            "claim_id".to_string(),
            "purpose".to_string(),
            "revocation_id".to_string(),
        ],
        body: vec![
            Stmt::Require(Expr::Claim {
                predicate: "AdmissibleFor".to_string(),
                args: vec![var("claim_id"), var("purpose")],
            }),
            Stmt::Retract {
                predicate: "AdmissibleFor".to_string(),
                args: vec![var("claim_id"), var("purpose")],
            },
            Stmt::Assert(Claim {
                predicate: "StandingRevoked".to_string(),
                args: vec![var("claim_id"), var("purpose"), var("revocation_id")],
            }),
            Stmt::Emit(Intent {
                name: "StandingRevocationAdmitted".to_string(),
                args: vec![var("revocation_id")],
            }),
        ],
    }
}

/// Admit a `DebtServiceRevenue` decision claim that relies on a
/// specific `IndependentlyVerifiedRevenue` claim.
///
/// Rejected if (a) there is no matching `IndependentlyVerifiedRevenue`
/// at the same (asset, period, amount, verification_id), or (b) the
/// referenced verification does not currently have `AdmissibleFor`
/// standing for the `bank_debt_service` purpose.
///
/// The purpose subject is embedded as a literal in the require
/// check, so the transformation is intrinsically tied to its
/// purpose — no caller-supplied purpose parameter, and no way
/// to pass the wrong one.
pub fn admit_debt_service_revenue() -> Transformation {
    Transformation {
        name: "admit_debt_service_revenue".to_string(),
        parameters: vec![
            "asset".to_string(),
            "period".to_string(),
            "amount".to_string(),
            "decision_id".to_string(),
            "verification_id".to_string(),
        ],
        body: vec![
            Stmt::Require(Expr::Claim {
                predicate: "IndependentlyVerifiedRevenue".to_string(),
                args: vec![
                    var("asset"),
                    var("period"),
                    var("amount"),
                    var("verification_id"),
                ],
            }),
            Stmt::Require(Expr::Claim {
                predicate: "AdmissibleFor".to_string(),
                args: vec![var("verification_id"), purpose(BANK_DEBT_SERVICE)],
            }),
            Stmt::Assert(Claim {
                predicate: "DebtServiceRevenue".to_string(),
                args: vec![
                    var("asset"),
                    var("period"),
                    var("amount"),
                    var("decision_id"),
                    var("verification_id"),
                ],
            }),
            Stmt::Emit(Intent {
                name: "DebtServiceRevenueAdmitted".to_string(),
                args: vec![var("decision_id")],
            }),
        ],
    }
}

/// Admit an `InvestorReportedRevenue` decision claim that relies on a
/// specific `IndependentlyVerifiedRevenue` claim.
///
/// Same admission gates as [`admit_debt_service_revenue`], but the
/// embedded purpose literal is `investor_reporting` and the asserted
/// predicate is `InvestorReportedRevenue`.
pub fn admit_investor_reported_revenue() -> Transformation {
    Transformation {
        name: "admit_investor_reported_revenue".to_string(),
        parameters: vec![
            "asset".to_string(),
            "period".to_string(),
            "amount".to_string(),
            "report_id".to_string(),
            "verification_id".to_string(),
        ],
        body: vec![
            Stmt::Require(Expr::Claim {
                predicate: "IndependentlyVerifiedRevenue".to_string(),
                args: vec![
                    var("asset"),
                    var("period"),
                    var("amount"),
                    var("verification_id"),
                ],
            }),
            Stmt::Require(Expr::Claim {
                predicate: "AdmissibleFor".to_string(),
                args: vec![var("verification_id"), purpose(INVESTOR_REPORTING)],
            }),
            Stmt::Assert(Claim {
                predicate: "InvestorReportedRevenue".to_string(),
                args: vec![
                    var("asset"),
                    var("period"),
                    var("amount"),
                    var("report_id"),
                    var("verification_id"),
                ],
            }),
            Stmt::Emit(Intent {
                name: "InvestorReportedRevenueAdmitted".to_string(),
                args: vec![var("report_id")],
            }),
        ],
    }
}

pub fn all_invariants() -> Vec<Invariant> {
    vec![
        admissibility_has_provenance(),
        admissibility_excludes_revocation(),
    ]
}

/// The claim-standing example as a [`crate::Program`]: five
/// transformations and two invariants. Stable identifier:
/// `"claim_standing"`.
pub fn program() -> crate::Program {
    crate::Program {
        name: "claim_standing".to_string(),
        invariants: all_invariants(),
        transformations: vec![
            admit_independent_verification(),
            grant_standing(),
            revoke_standing(),
            admit_debt_service_revenue(),
            admit_investor_reported_revenue(),
        ],
    }
}
