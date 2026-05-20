//! Double-entry ledger IR.
//!
//! Surface-syntax form: `examples/03_double_entry_ledger/ledger.morph`.
//!
//! Demonstrates the runtime against the canonical accounting domain:
//! posted journal entries must balance (sum of debits = sum of credits),
//! periods can be closed (after which new postings are rejected), and
//! closed periods can be restated through a separate transformation
//! that records `Supersedes` lineage without mutating the original
//! posting. Hosts the `TrialBalanceRow` derived claim as the
//! programme's read-side projection.
//!
//! Reuses the [`super::verified_revenue`] supersession pattern (the
//! `Supersedes` predicate) without sharing constructor code; each
//! example module re-declares the IR it needs.
//!
//! Exercises the require-vs-invariant distinction (period close is
//! admission gating; balance is an eternal invariant) and the
//! history-as-append-only discipline (JournalEntry and JournalLine
//! are content; PeriodClosed is append-only state; Supersedes is
//! append-only lineage). The existing `Expr::Sum` / `Expr::Eq` pair
//! handles the balance check via `Eq(Sum, Sum)`; the trial balance is
//! a `DerivedClaim` using `Expr::Sub`.

use crate::{Claim, Expr, Intent, Invariant, Stmt, Term, Transformation};

fn var(name: &str) -> Term {
    Term::Var(name.to_string())
}

// ============================================================
// Invariants — eternal rules over admitted state.
//
// Period-close gating lives in `require` on the posting
// transformations, not in an invariant. This is the same
// require-vs-invariant lesson the verified-revenue example
// crystallized: a constraint that only applies *at admission time*
// must not be an eternal invariant, or closing a period would
// either fail (because historical postings would then violate it)
// or cascade-retract them.
// ============================================================

/// Every `JournalEntry` must satisfy the fundamental accounting
/// equation: the sum of its line debits equals the sum of its line
/// credits.
///
/// Evaluates one `Sum` over debits and one over credits across the
/// `JournalLine` claims for this entry, then compares the two
/// decimals via `Eq`.
pub fn balanced_posted_entry() -> Invariant {
    Invariant {
        name: "balanced_posted_entry".to_string(),
        version: 1,
        body: Expr::Implies {
            left: Box::new(Expr::Claim {
                predicate: "JournalEntry".to_string(),
                args: vec![var("entry"), Term::Wildcard, Term::Wildcard],
            }),
            right: Box::new(Expr::Eq(
                Box::new(Expr::Sum {
                    value: var("d"),
                    binding: "d".to_string(),
                    body: Box::new(Expr::Claim {
                        predicate: "JournalLine".to_string(),
                        args: vec![var("entry"), Term::Wildcard, var("d"), Term::Wildcard],
                    }),
                }),
                Box::new(Expr::Sum {
                    value: var("c"),
                    binding: "c".to_string(),
                    body: Box::new(Expr::Claim {
                        predicate: "JournalLine".to_string(),
                        args: vec![var("entry"), Term::Wildcard, Term::Wildcard, var("c")],
                    }),
                }),
            )),
        },
    }
}

/// Every `JournalEntry` must have at least one matching
/// `JournalLine`. Without this invariant, a `JournalEntry` with
/// zero lines would trivially satisfy `balanced_posted_entry`
/// (both debit and credit sums are zero). The supplied
/// transformations never construct that state, but the runtime's
/// contract is "candidate state is admissible under invariants",
/// not "our transformations happen to be well behaved" — so the
/// invariant rules out the gap explicitly.
pub fn journal_entry_has_lines() -> Invariant {
    Invariant {
        name: "journal_entry_has_lines".to_string(),
        version: 1,
        body: Expr::Implies {
            left: Box::new(Expr::Claim {
                predicate: "JournalEntry".to_string(),
                args: vec![var("entry"), Term::Wildcard, Term::Wildcard],
            }),
            right: Box::new(Expr::Claim {
                predicate: "JournalLine".to_string(),
                args: vec![var("entry"), Term::Wildcard, Term::Wildcard, Term::Wildcard],
            }),
        },
    }
}

/// A posted entry can be superseded by at most one direct successor.
/// Reuses the same shape as the verified-revenue programme's
/// invariant of the same name.
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

/// Post a simple two-line journal entry: one debit, one credit, same
/// amount. Rejects if the target period has been closed.
///
/// The entry is structurally guaranteed to balance (debit = credit =
/// amount), so the `balanced_posted_entry` invariant is satisfied
/// trivially. The transformation exists to demonstrate the period-
/// close admission gate and the happy posting path.
pub fn post_simple_entry() -> Transformation {
    Transformation {
        name: "post_simple_entry".to_string(),
        parameters: vec![
            "entry_id".to_string(),
            "posting_date".to_string(),
            "period".to_string(),
            "debit_account".to_string(),
            "credit_account".to_string(),
            "amount".to_string(),
        ],
        body: vec![
            Stmt::Require(Expr::Not(Box::new(Expr::Claim {
                predicate: "PeriodClosed".to_string(),
                args: vec![var("period")],
            }))),
            Stmt::Assert(Claim {
                predicate: "JournalEntry".to_string(),
                args: vec![var("entry_id"), var("posting_date"), var("period")],
            }),
            Stmt::Assert(Claim {
                predicate: "JournalLine".to_string(),
                args: vec![
                    var("entry_id"),
                    var("debit_account"),
                    var("amount"),
                    Term::Literal(crate::Value::Decimal("0".to_string())),
                ],
            }),
            Stmt::Assert(Claim {
                predicate: "JournalLine".to_string(),
                args: vec![
                    var("entry_id"),
                    var("credit_account"),
                    Term::Literal(crate::Value::Decimal("0".to_string())),
                    var("amount"),
                ],
            }),
            Stmt::Emit(Intent {
                name: "JournalEntryPosted".to_string(),
                args: vec![var("entry_id")],
            }),
        ],
    }
}

/// Post a three-line journal entry: one debit, two credits. Caller
/// supplies both credit amounts independently. The
/// `balanced_posted_entry` invariant catches arithmetic mismatches
/// (credit_a_amount + credit_b_amount != debit_amount) on the
/// candidate state.
///
/// This is the transformation that exercises the balance invariant
/// in earnest; `post_simple_entry` cannot violate it.
pub fn post_split_entry() -> Transformation {
    Transformation {
        name: "post_split_entry".to_string(),
        parameters: vec![
            "entry_id".to_string(),
            "posting_date".to_string(),
            "period".to_string(),
            "debit_account".to_string(),
            "debit_amount".to_string(),
            "credit_a_account".to_string(),
            "credit_a_amount".to_string(),
            "credit_b_account".to_string(),
            "credit_b_amount".to_string(),
        ],
        body: vec![
            Stmt::Require(Expr::Not(Box::new(Expr::Claim {
                predicate: "PeriodClosed".to_string(),
                args: vec![var("period")],
            }))),
            Stmt::Assert(Claim {
                predicate: "JournalEntry".to_string(),
                args: vec![var("entry_id"), var("posting_date"), var("period")],
            }),
            Stmt::Assert(Claim {
                predicate: "JournalLine".to_string(),
                args: vec![
                    var("entry_id"),
                    var("debit_account"),
                    var("debit_amount"),
                    Term::Literal(crate::Value::Decimal("0".to_string())),
                ],
            }),
            Stmt::Assert(Claim {
                predicate: "JournalLine".to_string(),
                args: vec![
                    var("entry_id"),
                    var("credit_a_account"),
                    Term::Literal(crate::Value::Decimal("0".to_string())),
                    var("credit_a_amount"),
                ],
            }),
            Stmt::Assert(Claim {
                predicate: "JournalLine".to_string(),
                args: vec![
                    var("entry_id"),
                    var("credit_b_account"),
                    Term::Literal(crate::Value::Decimal("0".to_string())),
                    var("credit_b_amount"),
                ],
            }),
            Stmt::Emit(Intent {
                name: "JournalEntryPosted".to_string(),
                args: vec![var("entry_id")],
            }),
        ],
    }
}

/// Close `period`. Once closed, normal posting transformations
/// against that period are rejected by their own `require`. Closing
/// is terminal in v0: there is no `reopen_period`. Restatement
/// (via [`restate_entry`]) is the only path that admits new state
/// into a closed period.
///
/// Double-closing the same period is rejected.
pub fn close_period() -> Transformation {
    Transformation {
        name: "close_period".to_string(),
        parameters: vec!["period".to_string()],
        body: vec![
            Stmt::Require(Expr::Not(Box::new(Expr::Claim {
                predicate: "PeriodClosed".to_string(),
                args: vec![var("period")],
            }))),
            Stmt::Assert(Claim {
                predicate: "PeriodClosed".to_string(),
                args: vec![var("period")],
            }),
            Stmt::Emit(Intent {
                name: "PeriodClosed".to_string(),
                args: vec![var("period")],
            }),
        ],
    }
}

/// Restate a prior journal entry from a closed period. The prior
/// entry must exist, must be in the same period being restated, and
/// must not already have been superseded. The original
/// `JournalEntry` and its `JournalLine` claims are *not* touched —
/// they remain in admitted state as the record of what was filed.
/// A new `JournalEntry` is asserted with new lines, plus a
/// `Supersedes(new_entry_id, prior_entry_id)` claim to record the
/// lineage.
///
/// Deliberately does *not* require the period to be closed —
/// restatement of an open-period entry is just as valid a use case
/// (e.g. correcting a same-period error before close). The
/// `at_most_one_direct_successor` invariant ensures only one
/// restatement chain per original entry.
///
/// This restatement transformation handles a simple two-line
/// replacement (one debit, one credit, same amount). A multi-line
/// restate variant is straightforward future extension if needed.
pub fn restate_entry() -> Transformation {
    Transformation {
        name: "restate_entry".to_string(),
        parameters: vec![
            "new_entry_id".to_string(),
            "prior_entry_id".to_string(),
            "posting_date".to_string(),
            "period".to_string(),
            "debit_account".to_string(),
            "credit_account".to_string(),
            "amount".to_string(),
        ],
        body: vec![
            Stmt::Require(Expr::Claim {
                predicate: "JournalEntry".to_string(),
                args: vec![var("prior_entry_id"), Term::Wildcard, var("period")],
            }),
            Stmt::Require(Expr::Not(Box::new(Expr::Exists {
                binding: "newer".to_string(),
                body: Box::new(Expr::Claim {
                    predicate: "Supersedes".to_string(),
                    args: vec![var("newer"), var("prior_entry_id")],
                }),
            }))),
            Stmt::Assert(Claim {
                predicate: "JournalEntry".to_string(),
                args: vec![var("new_entry_id"), var("posting_date"), var("period")],
            }),
            Stmt::Assert(Claim {
                predicate: "JournalLine".to_string(),
                args: vec![
                    var("new_entry_id"),
                    var("debit_account"),
                    var("amount"),
                    Term::Literal(crate::Value::Decimal("0".to_string())),
                ],
            }),
            Stmt::Assert(Claim {
                predicate: "JournalLine".to_string(),
                args: vec![
                    var("new_entry_id"),
                    var("credit_account"),
                    Term::Literal(crate::Value::Decimal("0".to_string())),
                    var("amount"),
                ],
            }),
            Stmt::Assert(Claim {
                predicate: "Supersedes".to_string(),
                args: vec![var("new_entry_id"), var("prior_entry_id")],
            }),
            Stmt::Emit(Intent {
                name: "JournalEntryRestated".to_string(),
                args: vec![var("new_entry_id"), var("prior_entry_id")],
            }),
        ],
    }
}

pub fn all_invariants() -> Vec<Invariant> {
    vec![
        balanced_posted_entry(),
        journal_entry_has_lines(),
        at_most_one_direct_successor(),
    ]
}

/// Trial balance derived from the posted `JournalLine` claims. One
/// row per distinct account; the balance is debits minus credits.
///
/// The read-side projection that completes this programme. The
/// derived claim is enumerable via [`crate::enumerate_derived`] (or
/// `morpholog inspect derived double_entry_ledger TrialBalanceRow`
/// from the CLI). In v0 it is not added to admitted state, not
/// visible to invariants or transformations, not persisted, and not
/// recursively referenceable from another derived claim's body. See
/// `docs/design-history.md` for the derived-claims retrospective.
///
/// Shape:
///
///     keys:    [account]
///     values:  [balance = sum(debits for account) - sum(credits for account)]
///     domain:  JournalLine(_, account, _, _)
pub fn trial_balance_row() -> crate::DerivedClaim {
    crate::DerivedClaim {
        predicate: "TrialBalanceRow".to_string(),
        keys: vec!["account".to_string()],
        values: vec![crate::DerivedValue {
            name: "balance".to_string(),
            expr: Expr::Sub(
                // sum { d | JournalLine(_, account, d, _) }
                Box::new(Expr::Sum {
                    value: var("d"),
                    binding: "d".to_string(),
                    body: Box::new(Expr::Claim {
                        predicate: "JournalLine".to_string(),
                        args: vec![Term::Wildcard, var("account"), var("d"), Term::Wildcard],
                    }),
                }),
                // sum { c | JournalLine(_, account, _, c) }
                Box::new(Expr::Sum {
                    value: var("c"),
                    binding: "c".to_string(),
                    body: Box::new(Expr::Claim {
                        predicate: "JournalLine".to_string(),
                        args: vec![Term::Wildcard, var("account"), Term::Wildcard, var("c")],
                    }),
                }),
            ),
        }],
        domain: Expr::Claim {
            predicate: "JournalLine".to_string(),
            args: vec![
                Term::Wildcard,
                var("account"),
                Term::Wildcard,
                Term::Wildcard,
            ],
        },
    }
}

/// The double-entry-ledger example as a [`crate::Program`]: posting
/// and restatement transformations, balance and lineage invariants,
/// and the trial-balance derived claim. Stable identifier:
/// `"double_entry_ledger"`.
pub fn program() -> crate::Program {
    crate::Program {
        name: "double_entry_ledger".to_string(),
        invariants: all_invariants(),
        transformations: vec![
            post_simple_entry(),
            post_split_entry(),
            close_period(),
            restate_entry(),
        ],
        derived_claims: vec![trial_balance_row()],
    }
}
