//! Revenue restatement example IR.
//!
//! Surface-syntax form: `examples/02_revenue_restatement/restatement.morph`.
//!
//! Proves the runtime can model **contested legitimacy**: historical
//! claims remain admitted, current-standing pointer claims move via
//! retractions, supersession lineage is recorded — all without claim
//! metadata.

use crate::{Claim, Expr, Intent, Invariant, Stmt, Term, Transformation};

fn var(name: &str) -> Term {
    Term::Var(name.to_string())
}

pub fn current_recognition_matches_current_verification() -> Invariant {
    Invariant {
        name: "current_recognition_matches_current_verification".to_string(),
        version: 1,
        body: Expr::Implies {
            left: Box::new(Expr::And(vec![
                Expr::Claim {
                    predicate: "CurrentBankRecognition".to_string(),
                    args: vec![var("asset"), var("period"), var("r")],
                },
                Expr::Claim {
                    predicate: "BankRecognisedRevenue".to_string(),
                    args: vec![var("asset"), var("period"), var("amount"), var("r")],
                },
            ])),
            right: Box::new(Expr::Exists {
                binding: "v".to_string(),
                body: Box::new(Expr::And(vec![
                    Expr::Claim {
                        predicate: "IndependentlyVerifiedRevenue".to_string(),
                        args: vec![var("asset"), var("period"), var("amount"), var("v")],
                    },
                    Expr::Not(Box::new(Expr::Exists {
                        binding: "newer".to_string(),
                        body: Box::new(Expr::Claim {
                            predicate: "Supersedes".to_string(),
                            args: vec![var("newer"), var("v")],
                        }),
                    })),
                ])),
            }),
        },
    }
}

pub fn at_most_one_current_recognition_per_asset_period() -> Invariant {
    Invariant {
        name: "at_most_one_current_recognition_per_asset_period".to_string(),
        version: 1,
        body: Expr::Implies {
            left: Box::new(Expr::And(vec![
                Expr::Claim {
                    predicate: "CurrentBankRecognition".to_string(),
                    args: vec![var("asset"), var("period"), var("a")],
                },
                Expr::Claim {
                    predicate: "CurrentBankRecognition".to_string(),
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

pub fn recognise_bank_revenue() -> Transformation {
    Transformation {
        name: "recognise_bank_revenue".to_string(),
        parameters: vec![
            "asset".to_string(),
            "period".to_string(),
            "amount".to_string(),
            "recognition_id".to_string(),
        ],
        body: vec![
            Stmt::Require(Expr::Not(Box::new(Expr::Claim {
                predicate: "CurrentBankRecognition".to_string(),
                args: vec![var("asset"), var("period"), Term::Wildcard],
            }))),
            Stmt::Assert(Claim {
                predicate: "BankRecognisedRevenue".to_string(),
                args: vec![
                    var("asset"),
                    var("period"),
                    var("amount"),
                    var("recognition_id"),
                ],
            }),
            Stmt::Assert(Claim {
                predicate: "CurrentBankRecognition".to_string(),
                args: vec![var("asset"), var("period"), var("recognition_id")],
            }),
            Stmt::Emit(Intent {
                name: "BankRevenueRecognised".to_string(),
                args: vec![var("recognition_id")],
            }),
        ],
    }
}

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
                predicate: "CurrentBankRecognition".to_string(),
                args: vec![var("asset"), var("period"), Term::Wildcard],
            },
            Stmt::Emit(Intent {
                name: "VerificationCorrected".to_string(),
                args: vec![var("new_verification_id"), var("prior_verification_id")],
            }),
        ],
    }
}

pub fn restate_bank_revenue() -> Transformation {
    Transformation {
        name: "restate_bank_revenue".to_string(),
        parameters: vec![
            "asset".to_string(),
            "period".to_string(),
            "new_amount".to_string(),
            "new_recognition_id".to_string(),
            "prior_recognition_id".to_string(),
        ],
        body: vec![
            Stmt::Require(Expr::Claim {
                predicate: "BankRecognisedRevenue".to_string(),
                args: vec![
                    var("asset"),
                    var("period"),
                    Term::Wildcard,
                    var("prior_recognition_id"),
                ],
            }),
            Stmt::Retract {
                predicate: "CurrentBankRecognition".to_string(),
                args: vec![var("asset"), var("period"), Term::Wildcard],
            },
            Stmt::Assert(Claim {
                predicate: "BankRecognisedRevenue".to_string(),
                args: vec![
                    var("asset"),
                    var("period"),
                    var("new_amount"),
                    var("new_recognition_id"),
                ],
            }),
            Stmt::Assert(Claim {
                predicate: "CurrentBankRecognition".to_string(),
                args: vec![var("asset"), var("period"), var("new_recognition_id")],
            }),
            Stmt::Assert(Claim {
                predicate: "Supersedes".to_string(),
                args: vec![var("new_recognition_id"), var("prior_recognition_id")],
            }),
            Stmt::Emit(Intent {
                name: "BankRevenueRestated".to_string(),
                args: vec![var("new_recognition_id"), var("prior_recognition_id")],
            }),
        ],
    }
}

pub fn all_invariants() -> Vec<Invariant> {
    vec![
        current_recognition_matches_current_verification(),
        at_most_one_current_recognition_per_asset_period(),
        at_most_one_direct_successor(),
    ]
}

/// The revenue-restatement example as a [`crate::Program`]: four
/// transformations and three invariants. Stable identifier:
/// `"revenue_restatement"`.
pub fn program() -> crate::Program {
    crate::Program {
        name: "revenue_restatement".to_string(),
        invariants: all_invariants(),
        transformations: vec![
            admit_independent_verification(),
            recognise_bank_revenue(),
            correct_independent_verification(),
            restate_bank_revenue(),
        ],
    }
}
