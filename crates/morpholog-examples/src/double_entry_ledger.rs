//! Double-entry ledger IR: balance invariant, period close, restatement
//! via `Supersedes`, and the `TrialBalanceRow` derived claim as the
//! programme's read-side projection. See
//! `examples/03_double_entry_ledger/README.md` for the business framing.

use morpholog_core::{Invariant, Transformation};

use morpholog_core::dsl::*;

// ============================================================
// Invariants - eternal rules over admitted state.
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
pub fn balanced_posted_entry() -> Invariant {
    Invariant {
        name: "balanced_posted_entry".to_string(),
        version: 1,
        body: implies(
            claim("JournalEntry", vec![var("entry"), wildcard(), wildcard()]),
            eq(
                sum(
                    var("d"),
                    "d",
                    claim(
                        "JournalLine",
                        vec![var("entry"), wildcard(), var("d"), wildcard()],
                    ),
                ),
                sum(
                    var("c"),
                    "c",
                    claim(
                        "JournalLine",
                        vec![var("entry"), wildcard(), wildcard(), var("c")],
                    ),
                ),
            ),
        ),
    }
}

/// Every `JournalEntry` must have at least one matching
/// `JournalLine`. Without this invariant, a `JournalEntry` with zero
/// lines would trivially satisfy `balanced_posted_entry` (both sums
/// are zero). The supplied transformations never construct that
/// state, but the runtime's contract is "candidate state is
/// admissible under invariants", so the gap is closed explicitly.
pub fn journal_entry_has_lines() -> Invariant {
    Invariant {
        name: "journal_entry_has_lines".to_string(),
        version: 1,
        body: implies(
            claim("JournalEntry", vec![var("entry"), wildcard(), wildcard()]),
            claim(
                "JournalLine",
                vec![var("entry"), wildcard(), wildcard(), wildcard()],
            ),
        ),
    }
}

/// A posted entry can be superseded by at most one direct successor.
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

/// Post a simple two-line journal entry: one debit, one credit, same
/// amount. Rejects if the target period has been closed.
pub fn post_simple_entry() -> Transformation {
    Transformation {
        name: "post_simple_entry".to_string(),
        parameters: params(&[
            "entry_id",
            "posting_date",
            "period",
            "debit_account",
            "credit_account",
            "amount",
        ]),
        body: vec![
            require(not(claim("PeriodClosed", vec![var("period")]))),
            assert_(
                "JournalEntry",
                vec![var("entry_id"), var("posting_date"), var("period")],
            ),
            assert_(
                "JournalLine",
                vec![
                    var("entry_id"),
                    var("debit_account"),
                    var("amount"),
                    dec("0"),
                ],
            ),
            assert_(
                "JournalLine",
                vec![
                    var("entry_id"),
                    var("credit_account"),
                    dec("0"),
                    var("amount"),
                ],
            ),
            emit("JournalEntryPosted", vec![var("entry_id")]),
        ],
    }
}

/// Post a three-line journal entry: one debit, two credits. Caller
/// supplies both credit amounts independently. The
/// `balanced_posted_entry` invariant catches arithmetic mismatches
/// on the candidate state.
pub fn post_split_entry() -> Transformation {
    Transformation {
        name: "post_split_entry".to_string(),
        parameters: params(&[
            "entry_id",
            "posting_date",
            "period",
            "debit_account",
            "debit_amount",
            "credit_a_account",
            "credit_a_amount",
            "credit_b_account",
            "credit_b_amount",
        ]),
        body: vec![
            require(not(claim("PeriodClosed", vec![var("period")]))),
            assert_(
                "JournalEntry",
                vec![var("entry_id"), var("posting_date"), var("period")],
            ),
            assert_(
                "JournalLine",
                vec![
                    var("entry_id"),
                    var("debit_account"),
                    var("debit_amount"),
                    dec("0"),
                ],
            ),
            assert_(
                "JournalLine",
                vec![
                    var("entry_id"),
                    var("credit_a_account"),
                    dec("0"),
                    var("credit_a_amount"),
                ],
            ),
            assert_(
                "JournalLine",
                vec![
                    var("entry_id"),
                    var("credit_b_account"),
                    dec("0"),
                    var("credit_b_amount"),
                ],
            ),
            emit("JournalEntryPosted", vec![var("entry_id")]),
        ],
    }
}

/// Close `period`. Once closed, normal posting transformations
/// against that period are rejected by their own `require`. Closing
/// is terminal in v0: there is no `reopen_period`. Restatement (via
/// [`restate_entry`]) is the only path that admits new state into a
/// closed period. Double-closing the same period is rejected.
pub fn close_period() -> Transformation {
    Transformation {
        name: "close_period".to_string(),
        parameters: params(&["period"]),
        body: vec![
            require(not(claim("PeriodClosed", vec![var("period")]))),
            assert_("PeriodClosed", vec![var("period")]),
            emit("PeriodClosed", vec![var("period")]),
        ],
    }
}

/// Restate a prior journal entry. The prior entry must exist in the
/// same period and must not already have been superseded. The
/// original `JournalEntry` and its `JournalLine` claims are *not*
/// touched - they remain in admitted state as the record of what
/// was filed. A new `JournalEntry` is asserted with new lines, plus
/// a `Supersedes(new_entry_id, prior_entry_id)` claim to record the
/// lineage.
///
/// Deliberately does *not* require the period to be closed -
/// restatement of an open-period entry is just as valid a use case.
/// The `at_most_one_direct_successor` invariant ensures only one
/// restatement chain per original entry.
pub fn restate_entry() -> Transformation {
    Transformation {
        name: "restate_entry".to_string(),
        parameters: params(&[
            "new_entry_id",
            "prior_entry_id",
            "posting_date",
            "period",
            "debit_account",
            "credit_account",
            "amount",
        ]),
        body: vec![
            require(claim(
                "JournalEntry",
                vec![var("prior_entry_id"), wildcard(), var("period")],
            )),
            require(not(exists(
                "newer",
                claim("Supersedes", vec![var("newer"), var("prior_entry_id")]),
            ))),
            assert_(
                "JournalEntry",
                vec![var("new_entry_id"), var("posting_date"), var("period")],
            ),
            assert_(
                "JournalLine",
                vec![
                    var("new_entry_id"),
                    var("debit_account"),
                    var("amount"),
                    dec("0"),
                ],
            ),
            assert_(
                "JournalLine",
                vec![
                    var("new_entry_id"),
                    var("credit_account"),
                    dec("0"),
                    var("amount"),
                ],
            ),
            assert_(
                "Supersedes",
                vec![var("new_entry_id"), var("prior_entry_id")],
            ),
            emit(
                "JournalEntryRestated",
                vec![var("new_entry_id"), var("prior_entry_id")],
            ),
        ],
    }
}

pub fn all_predicates() -> Vec<morpholog_core::PredicateDecl> {
    vec![
        predicate("JournalEntry")
            .subject("entry_id")
            .subject("posting_date")
            .subject("period")
            .build(),
        predicate("JournalLine")
            .subject("entry_id")
            .subject("account")
            .decimal("debit_amount")
            .decimal("credit_amount")
            .build(),
        predicate("PeriodClosed").subject("period").build(),
        predicate("Supersedes")
            .subject("new_entry_id")
            .subject("prior_entry_id")
            .build(),
        predicate("TrialBalanceRow")
            .subject("account")
            .decimal("balance")
            .build(),
    ]
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
/// Shape:
///
/// ```text
/// keys:    [account]
/// values:  [balance = sum(debits for account) - sum(credits for account)]
/// domain:  JournalLine(_, account, _, _)
/// ```
pub fn trial_balance_row() -> morpholog_core::DerivedClaim {
    morpholog_core::DerivedClaim {
        predicate: "TrialBalanceRow".to_string(),
        keys: vec!["account".to_string()],
        values: vec![morpholog_core::DerivedValue {
            name: "balance".to_string(),
            expr: sub(
                sum(
                    var("d"),
                    "d",
                    claim(
                        "JournalLine",
                        vec![wildcard(), var("account"), var("d"), wildcard()],
                    ),
                ),
                sum(
                    var("c"),
                    "c",
                    claim(
                        "JournalLine",
                        vec![wildcard(), var("account"), wildcard(), var("c")],
                    ),
                ),
            ),
        }],
        domain: claim(
            "JournalLine",
            vec![wildcard(), var("account"), wildcard(), wildcard()],
        ),
    }
}

/// The double-entry-ledger example as a [`morpholog_core::Program`]:
/// posting and restatement transformations, balance and lineage
/// invariants, and the trial-balance derived claim. Stable
/// identifier: `"double_entry_ledger"`.
pub fn program() -> morpholog_core::Program {
    morpholog_core::Program {
        name: "double_entry_ledger".to_string(),
        predicates: all_predicates(),
        intents: vec![
            intent_decl("JournalEntryPosted")
                .subject("entry_id")
                .build(),
            intent_decl("PeriodClosed").subject("period").build(),
            intent_decl("JournalEntryRestated")
                .subject("new_entry_id")
                .subject("prior_entry_id")
                .build(),
        ],
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
