//! Settlement netting example IR.
//!
//! Surface-syntax form: `examples/01_settlement_netting/netting.morph`.

use crate::{Claim, Expr, Intent, Invariant, Stmt, Term, Transformation};

fn var(name: &str) -> Term {
    Term::Var(name.to_string())
}

pub fn net_settlement_has_lines() -> Invariant {
    Invariant {
        name: "net_settlement_has_lines".to_string(),
        version: 1,
        body: Expr::Implies {
            left: Box::new(Expr::Claim {
                predicate: "NetSettlement".to_string(),
                args: vec![var("net"), Term::Wildcard, Term::Wildcard, Term::Wildcard],
            }),
            right: Box::new(Expr::Exists {
                binding: "line".to_string(),
                body: Box::new(Expr::Claim {
                    predicate: "SettlementLine".to_string(),
                    args: vec![var("line"), var("net"), Term::Wildcard],
                }),
            }),
        },
    }
}

pub fn net_amount_equals_lines() -> Invariant {
    Invariant {
        name: "net_amount_equals_lines".to_string(),
        version: 1,
        body: Expr::Implies {
            left: Box::new(Expr::Claim {
                predicate: "NetSettlement".to_string(),
                args: vec![var("net"), Term::Wildcard, Term::Wildcard, var("amount")],
            }),
            right: Box::new(Expr::Eq(
                Box::new(Expr::Term(var("amount"))),
                Box::new(Expr::Sum {
                    value: var("x"),
                    binding: "x".to_string(),
                    body: Box::new(Expr::Claim {
                        predicate: "SettlementLine".to_string(),
                        args: vec![Term::Wildcard, var("net"), var("x")],
                    }),
                }),
            )),
        },
    }
}

pub fn no_double_netting() -> Invariant {
    Invariant {
        name: "no_double_netting".to_string(),
        version: 1,
        body: Expr::Implies {
            left: Box::new(Expr::Claim {
                predicate: "SettlementLine".to_string(),
                args: vec![var("line"), var("net"), Term::Wildcard],
            }),
            right: Box::new(Expr::Not(Box::new(Expr::Exists {
                binding: "other".to_string(),
                body: Box::new(Expr::And(vec![
                    Expr::Claim {
                        predicate: "SettlementLine".to_string(),
                        args: vec![var("line"), var("other"), Term::Wildcard],
                    },
                    Expr::Neq(var("other"), var("net")),
                ])),
            }))),
        },
    }
}

pub fn create_net_settlement() -> Transformation {
    Transformation {
        name: "create_net_settlement".to_string(),
        parameters: vec![
            "party_a".to_string(),
            "party_b".to_string(),
            "lines".to_string(),
        ],
        body: vec![
            Stmt::Require(Expr::Forall {
                binding: "line".to_string(),
                source: Box::new(Expr::In(var("line"), var("lines"))),
                body: Box::new(Expr::And(vec![
                    Expr::Claim {
                        predicate: "ApprovedSettlementLine".to_string(),
                        args: vec![var("line")],
                    },
                    Expr::Claim {
                        predicate: "Between".to_string(),
                        args: vec![var("line"), var("party_a"), var("party_b")],
                    },
                    Expr::Not(Box::new(Expr::Claim {
                        predicate: "Netted".to_string(),
                        args: vec![var("line")],
                    })),
                ])),
            }),
            Stmt::LetNewSubject {
                name: "net".to_string(),
            },
            Stmt::Let {
                name: "amount".to_string(),
                value: Expr::Sum {
                    value: var("x"),
                    binding: "x".to_string(),
                    body: Box::new(Expr::And(vec![
                        Expr::In(var("line"), var("lines")),
                        Expr::Claim {
                            predicate: "LineAmount".to_string(),
                            args: vec![var("line"), var("x")],
                        },
                    ])),
                },
            },
            Stmt::Assert(Claim {
                predicate: "NetSettlement".to_string(),
                args: vec![var("net"), var("party_a"), var("party_b"), var("amount")],
            }),
            Stmt::For {
                binding: "line".to_string(),
                collection: Expr::Term(var("lines")),
                body: vec![
                    Stmt::Let {
                        name: "amt".to_string(),
                        value: Expr::ValueOf {
                            predicate: "LineAmount".to_string(),
                            args: vec![var("line"), Term::Wildcard],
                            default: None,
                        },
                    },
                    Stmt::Assert(Claim {
                        predicate: "SettlementLine".to_string(),
                        args: vec![var("line"), var("net"), var("amt")],
                    }),
                    Stmt::Assert(Claim {
                        predicate: "Netted".to_string(),
                        args: vec![var("line")],
                    }),
                ],
            },
            Stmt::Emit(Intent {
                name: "NetSettlementCreated".to_string(),
                args: vec![var("net")],
            }),
        ],
    }
}

pub fn all_invariants() -> Vec<Invariant> {
    vec![
        net_settlement_has_lines(),
        net_amount_equals_lines(),
        no_double_netting(),
    ]
}

/// The settlement-netting example as a [`crate::Program`]: one
/// transformation (`create_net_settlement`) and three invariants.
/// Stable identifier: `"settlement_netting"`.
pub fn program() -> crate::Program {
    crate::Program {
        name: "settlement_netting".to_string(),
        invariants: all_invariants(),
        transformations: vec![create_net_settlement()],
    }
}
