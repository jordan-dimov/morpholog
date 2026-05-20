//! Verified revenue - the flagship example for the project's
//! commercial thesis. One business programme combining two patterns
//! that the runtime needed to defend a contested number over time:
//!
//! - **Currentness with restatement.** An independent verifier admits
//!   a revenue figure for an asset/period; later they may correct it.
//!   The original verification stays admitted as the record of what
//!   was filed; `Supersedes` records the lineage; a singleton
//!   `CurrentVerification` pointer moves to the corrected figure.
//!
//! - **Admissibility-for-purpose.** Different authorities (a bank's
//!   credit committee, an investor-relations office) grant **standing**
//!   for the verification to be relied upon for *their* decisions
//!   (debt-service-coverage, investor reporting). Standing can be
//!   revoked. Decisions admitted under valid standing survive any
//!   later revocation - the legitimacy of a *past* decision was
//!   established when it was made; revocation prevents *future*
//!   decisions, not past ones.
//!
//! Surface-syntax form: `examples/02_verified_revenue/verified_revenue.morph`.

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
// Invariants
//
// Four invariants govern the consistency of the authority and
// currentness records. No invariant ties decision claims to live
// AdmissibleFor: decisions are gated at admission time via `require`
// and once admitted are locked in - historical decisions survive
// later revocation or restatement.
// ============================================================

/// Every active `AdmissibleFor(v, p)` must be backed by some
/// `StandingGrantedBy(v, p, _, _)` claim.
pub fn admissibility_has_provenance() -> Invariant {
    Invariant {
        name: "admissibility_has_provenance".to_string(),
        version: 1,
        body: Expr::Implies {
            left: Box::new(Expr::Claim {
                predicate: "AdmissibleFor".to_string(),
                args: vec![var("v"), var("p")],
            }),
            right: Box::new(Expr::Claim {
                predicate: "StandingGrantedBy".to_string(),
                args: vec![var("v"), var("p"), Term::Wildcard, Term::Wildcard],
            }),
        },
    }
}

/// `AdmissibleFor(v, p)` cannot coexist with any
/// `StandingRevoked(v, p, _)`. Revocation is terminal in v0.
pub fn admissibility_excludes_revocation() -> Invariant {
    Invariant {
        name: "admissibility_excludes_revocation".to_string(),
        version: 1,
        body: Expr::Implies {
            left: Box::new(Expr::Claim {
                predicate: "AdmissibleFor".to_string(),
                args: vec![var("v"), var("p")],
            }),
            right: Box::new(Expr::Not(Box::new(Expr::Exists {
                binding: "r".to_string(),
                body: Box::new(Expr::Claim {
                    predicate: "StandingRevoked".to_string(),
                    args: vec![var("v"), var("p"), var("r")],
                }),
            }))),
        },
    }
}

/// At most one `CurrentVerification` per `(asset, period)`. The
/// pointer is singleton.
pub fn at_most_one_current_verification_per_asset_period() -> Invariant {
    Invariant {
        name: "at_most_one_current_verification_per_asset_period".to_string(),
        version: 1,
        body: Expr::Implies {
            left: Box::new(Expr::And(vec![
                Expr::Claim {
                    predicate: "CurrentVerification".to_string(),
                    args: vec![var("asset"), var("period"), var("a")],
                },
                Expr::Claim {
                    predicate: "CurrentVerification".to_string(),
                    args: vec![var("asset"), var("period"), var("b")],
                },
            ])),
            right: Box::new(Expr::Eq(
                Box::new(Expr::Term(var("a"))),
                Box::new(Expr::Term(var("b"))),
            )),
        },
    }
}

/// A verification can be superseded by at most one direct successor.
/// Parallel restatement chains are forbidden by construction.
pub fn at_most_one_direct_successor() -> Invariant {
    Invariant {
        name: "at_most_one_direct_successor".to_string(),
        version: 1,
        body: Expr::Implies {
            left: Box::new(Expr::And(vec![
                Expr::Claim {
                    predicate: "Supersedes".to_string(),
                    args: vec![var("new_a"), var("old")],
                },
                Expr::Claim {
                    predicate: "Supersedes".to_string(),
                    args: vec![var("new_b"), var("old")],
                },
            ])),
            right: Box::new(Expr::Eq(
                Box::new(Expr::Term(var("new_a"))),
                Box::new(Expr::Term(var("new_b"))),
            )),
        },
    }
}

// ============================================================
// Transformations
// ============================================================

/// First admission of an independent verification figure for an
/// `(asset, period)`. Asserts both the underlying claim and the
/// singleton `CurrentVerification` pointer. Rejected if there is
/// already a current verification for this asset/period - use
/// `correct_independent_verification` to update.
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
            Stmt::Require(Expr::Not(Box::new(Expr::Claim {
                predicate: "CurrentVerification".to_string(),
                args: vec![var("asset"), var("period"), Term::Wildcard],
            }))),
            Stmt::Assert(Claim {
                predicate: "IndependentlyVerifiedRevenue".to_string(),
                args: vec![
                    var("asset"),
                    var("period"),
                    var("amount"),
                    var("verification_id"),
                ],
            }),
            Stmt::Assert(Claim {
                predicate: "CurrentVerification".to_string(),
                args: vec![var("asset"), var("period"), var("verification_id")],
            }),
            Stmt::Emit(Intent {
                name: "IndependentVerificationAdmitted".to_string(),
                args: vec![var("verification_id")],
            }),
        ],
    }
}

/// Correct a previously-admitted verification figure. The original
/// stays in admitted state; the new figure is admitted; lineage is
/// recorded as `Supersedes`; the singleton pointer moves to the new
/// verification; and **any standing previously granted on the prior
/// verification is retracted** - the authorities must re-issue
/// standing on the corrected figure if they still agree with it.
///
/// Existing decisions admitted under the prior standing survive in
/// admitted state. The runtime does not cascade-retract them.
pub fn correct_independent_verification() -> Transformation {
    Transformation {
        name: "correct_independent_verification".to_string(),
        parameters: vec![
            "asset".to_string(),
            "period".to_string(),
            "new_amount".to_string(),
            "new_verification_id".to_string(),
            "prior_verification_id".to_string(),
        ],
        body: vec![
            Stmt::Require(Expr::Claim {
                predicate: "IndependentlyVerifiedRevenue".to_string(),
                args: vec![
                    var("asset"),
                    var("period"),
                    Term::Wildcard,
                    var("prior_verification_id"),
                ],
            }),
            Stmt::Require(Expr::Not(Box::new(Expr::Claim {
                predicate: "Supersedes".to_string(),
                args: vec![Term::Wildcard, var("prior_verification_id")],
            }))),
            Stmt::Assert(Claim {
                predicate: "IndependentlyVerifiedRevenue".to_string(),
                args: vec![
                    var("asset"),
                    var("period"),
                    var("new_amount"),
                    var("new_verification_id"),
                ],
            }),
            Stmt::Assert(Claim {
                predicate: "Supersedes".to_string(),
                args: vec![var("new_verification_id"), var("prior_verification_id")],
            }),
            Stmt::Retract {
                predicate: "CurrentVerification".to_string(),
                args: vec![var("asset"), var("period"), var("prior_verification_id")],
            },
            // Pattern-based retraction: every active AdmissibleFor for
            // the prior verification, for any purpose. Authorities
            // must re-issue standing if they accept the correction.
            Stmt::Retract {
                predicate: "AdmissibleFor".to_string(),
                args: vec![var("prior_verification_id"), Term::Wildcard],
            },
            Stmt::Assert(Claim {
                predicate: "CurrentVerification".to_string(),
                args: vec![var("asset"), var("period"), var("new_verification_id")],
            }),
            Stmt::Emit(Intent {
                name: "VerificationCorrected".to_string(),
                args: vec![var("new_verification_id"), var("prior_verification_id")],
            }),
        ],
    }
}

/// Grant `purpose` standing on `verification_id`, recording
/// `authority` as the granting party and `grant_id` as the provenance
/// handle.
///
/// Admission requires four things in order:
///
/// 1. **The verification exists.** Some `IndependentlyVerifiedRevenue`
///    claim references the supplied `verification_id`. Standing
///    cannot be granted on a phantom id.
/// 2. **The verification is current.** It is the active
///    `CurrentVerification` for its `(asset, period)`. Standing on a
///    superseded verification is forbidden - future reliance must
///    attach to the current figure. (Historical decisions admitted
///    when the verification *was* current survive separately; they
///    are decisions on the record, not future standing.)
/// 3. **No revocation.** The `(verification, purpose)` pair has not
///    been revoked - revocation is terminal in v0.
/// 4. **No double-grant.** The pair does not already have active
///    admissibility.
pub fn grant_standing() -> Transformation {
    Transformation {
        name: "grant_standing".to_string(),
        parameters: vec![
            "verification_id".to_string(),
            "purpose".to_string(),
            "authority".to_string(),
            "grant_id".to_string(),
        ],
        body: vec![
            Stmt::Require(Expr::Claim {
                predicate: "IndependentlyVerifiedRevenue".to_string(),
                args: vec![
                    Term::Wildcard,
                    Term::Wildcard,
                    Term::Wildcard,
                    var("verification_id"),
                ],
            }),
            Stmt::Require(Expr::Claim {
                predicate: "CurrentVerification".to_string(),
                args: vec![Term::Wildcard, Term::Wildcard, var("verification_id")],
            }),
            Stmt::Require(Expr::Not(Box::new(Expr::Exists {
                binding: "r".to_string(),
                body: Box::new(Expr::Claim {
                    predicate: "StandingRevoked".to_string(),
                    args: vec![var("verification_id"), var("purpose"), var("r")],
                }),
            }))),
            Stmt::Require(Expr::Not(Box::new(Expr::Claim {
                predicate: "AdmissibleFor".to_string(),
                args: vec![var("verification_id"), var("purpose")],
            }))),
            Stmt::Assert(Claim {
                predicate: "StandingGrantedBy".to_string(),
                args: vec![
                    var("verification_id"),
                    var("purpose"),
                    var("authority"),
                    var("grant_id"),
                ],
            }),
            Stmt::Assert(Claim {
                predicate: "AdmissibleFor".to_string(),
                args: vec![var("verification_id"), var("purpose")],
            }),
            Stmt::Emit(Intent {
                name: "StandingGranted".to_string(),
                args: vec![var("grant_id")],
            }),
        ],
    }
}

/// Revoke `purpose` standing for `verification_id`. Requires current
/// admissibility to revoke. The historical `StandingGrantedBy` claim
/// survives; the active `AdmissibleFor` is retracted; a new
/// `StandingRevoked` is admitted (terminal in v0).
pub fn revoke_standing() -> Transformation {
    Transformation {
        name: "revoke_standing".to_string(),
        parameters: vec![
            "verification_id".to_string(),
            "purpose".to_string(),
            "revocation_id".to_string(),
        ],
        body: vec![
            Stmt::Require(Expr::Claim {
                predicate: "AdmissibleFor".to_string(),
                args: vec![var("verification_id"), var("purpose")],
            }),
            Stmt::Retract {
                predicate: "AdmissibleFor".to_string(),
                args: vec![var("verification_id"), var("purpose")],
            },
            Stmt::Assert(Claim {
                predicate: "StandingRevoked".to_string(),
                args: vec![var("verification_id"), var("purpose"), var("revocation_id")],
            }),
            Stmt::Emit(Intent {
                name: "StandingRevocationAdmitted".to_string(),
                args: vec![var("revocation_id")],
            }),
        ],
    }
}

/// Admit a `DebtServiceRevenue` decision that relies on a specific
/// `IndependentlyVerifiedRevenue` claim with active standing for the
/// `bank_debt_service` purpose. The purpose is embedded as a literal
/// in the require, so the transformation is intrinsically tied to
/// its purpose.
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

/// Admit an `InvestorReportedRevenue` decision that relies on a
/// specific `IndependentlyVerifiedRevenue` claim with active standing
/// for the `investor_reporting` purpose. Same shape as
/// [`admit_debt_service_revenue`].
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
        at_most_one_current_verification_per_asset_period(),
        at_most_one_direct_successor(),
    ]
}

/// The verified-revenue example as a [`crate::Program`]: six
/// transformations and four invariants. Stable identifier:
/// `"verified_revenue"`.
pub fn program() -> crate::Program {
    crate::Program {
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
