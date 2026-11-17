//! Integration tests for the double-entry ledger example
//! (`examples/03_double_entry_ledger/`).
//!
//! Proves: posted entries balance; a closed period refuses normal posting;
//! a closed period can be restated, keeping the original entry and recording
//! `Supersedes`; an entry cannot be restated twice; a period cannot be
//! closed twice; two lines with the same account and amounts stay two
//! lines; an entry id names one entry even for an act without the gate.

mod common;
use std::sync::OnceLock;

use common::{Example, claim_instance, dec, has_claim, refused_by, subj};
use morpholog_core::{EvalValue, State, Stmt};
use morpholog_examples::double_entry_ledger;
use morpholog_test_support::{enumerate_derived, eval_invariant};

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&double_entry_ledger::program()))
}

/// Whether `state` holds a journal line for this entry with these values,
/// whatever line id its posting minted.
fn has_line(state: &State, values: &[EvalValue]) -> bool {
    state
        .claims_for("JournalLine")
        .any(|c| c.args[..1] == values[..1] && c.args[2..] == values[1..])
}

#[test]
fn simple_entry_balances_and_commits() {
    let args = vec![
        subj("entry_001"),
        subj("d_2026_04_15"),
        subj("p_2026_04"),
        subj("account_cash"),
        subj("account_revenue"),
        dec(100),
    ];
    let state = ex().must_accept(
        &double_entry_ledger::post_simple_entry(),
        args.clone(),
        State::default(),
    );
    // Posting the same entry again is refused, so its notice goes out once.
    refused_by(
        &ex().must_reject(&double_entry_ledger::post_simple_entry(), args, &state),
        "entry_id_unused",
    );

    assert_eq!(state.len(), 3, "1 JournalEntry + 2 JournalLine");
    assert!(has_claim(
        &state,
        "JournalEntry",
        &[subj("entry_001"), subj("d_2026_04_15"), subj("p_2026_04")],
    ));
    assert!(has_line(
        &state,
        &[subj("entry_001"), subj("account_cash"), dec(100), dec(0)],
    ));
    assert!(has_line(
        &state,
        &[subj("entry_001"), subj("account_revenue"), dec(0), dec(100)],
    ));
}

#[test]
fn split_entry_balances_and_commits() {
    // Debit 100 to cash; credit 70 to revenue + 30 to deferred revenue.
    // Sums: debits = 100; credits = 70 + 30 = 100. Balance invariant
    // holds.
    let args = vec![
        subj("entry_001"),
        subj("d_2026_04_15"),
        subj("p_2026_04"),
        subj("account_cash"),
        dec(100),
        subj("account_revenue"),
        dec(70),
        subj("account_deferred_revenue"),
        dec(30),
    ];
    let state = ex().must_accept(
        &double_entry_ledger::post_split_entry(),
        args.clone(),
        State::default(),
    );
    refused_by(
        &ex().must_reject(&double_entry_ledger::post_split_entry(), args, &state),
        "entry_id_unused",
    );

    assert_eq!(state.len(), 4, "1 JournalEntry + 3 JournalLine");
    // Lines: 100 debit cash, 70 credit revenue, 30 credit deferred.
    assert!(has_line(
        &state,
        &[subj("entry_001"), subj("account_cash"), dec(100), dec(0)],
    ));
    assert!(has_line(
        &state,
        &[subj("entry_001"), subj("account_revenue"), dec(0), dec(70)],
    ));
    assert!(has_line(
        &state,
        &[
            subj("entry_001"),
            subj("account_deferred_revenue"),
            dec(0),
            dec(30),
        ],
    ));
}

#[test]
fn unbalanced_entry_rejected_by_invariant() {
    // Debit 100; credit 70 + 25 = 95. The gates pass, but the candidate state
    // violates `balanced_posted_entry`, so no claim is admitted.
    let reason = ex().must_reject(
        &double_entry_ledger::post_split_entry(),
        vec![
            subj("entry_001"),
            subj("d_2026_04_15"),
            subj("p_2026_04"),
            subj("account_cash"),
            dec(100),
            subj("account_revenue"),
            dec(70),
            subj("account_deferred_revenue"),
            dec(25),
        ],
        &State::default(),
    );
    assert!(
        reason.to_string().contains("balanced_posted_entry"),
        "got reason: {reason}"
    );
}

#[test]
fn closed_period_rejects_normal_posting() {
    // Close the period, then try to post a normal entry to it.
    // `require not PeriodClosed(period)` catches it at admission;
    // no claim is admitted.
    let after_close = ex().must_accept(
        &double_entry_ledger::close_period(),
        vec![subj("p_2026_04")],
        State::default(),
    );

    assert!(has_claim(
        &after_close,
        "PeriodClosed",
        &[subj("p_2026_04")]
    ));

    let reason = ex().must_reject(
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj("entry_001"),
            subj("d_2026_04_15"),
            subj("p_2026_04"),
            subj("account_cash"),
            subj("account_revenue"),
            dec(100),
        ],
        &after_close,
    );
    assert!(
        reason.to_string().contains("require"),
        "got reason: {reason}"
    );
}

#[test]
fn double_close_rejected() {
    let after_close = ex().must_accept(
        &double_entry_ledger::close_period(),
        vec![subj("p_2026_04")],
        State::default(),
    );

    let reason = ex().must_reject(
        &double_entry_ledger::close_period(),
        vec![subj("p_2026_04")],
        &after_close,
    );
    assert!(
        reason.to_string().contains("require"),
        "got reason: {reason}"
    );
}

#[test]
fn restatement_into_closed_period_preserves_original() {
    // 1. Post an entry: cash debit 100, revenue credit 100.
    let s1 = ex().must_accept(
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj("entry_001"),
            subj("d_2026_04_15"),
            subj("p_2026_04"),
            subj("account_cash"),
            subj("account_revenue"),
            dec(100),
        ],
        State::default(),
    );

    // 2. Close the period.
    let s2 = ex().must_accept(
        &double_entry_ledger::close_period(),
        vec![subj("p_2026_04")],
        s1,
    );

    // 3. Restate the entry with a corrected amount (101 instead of
    //    100). The restatement transformation does *not* check
    //    PeriodClosed - restatement is the path for closed periods.
    let s3 = ex().must_accept(
        &double_entry_ledger::restate_entry(),
        vec![
            subj("entry_002"),
            subj("entry_001"),
            subj("d_2026_05_10"),
            subj("p_2026_04"),
            subj("account_cash"),
            subj("account_revenue"),
            dec(101),
        ],
        s2,
    );

    // Final state contains:
    //  - the original entry header + 2 lines (preserved)
    //  - the new entry header + 2 lines
    //  - the PeriodClosed claim
    //  - the Supersedes(entry_002, entry_001) link
    // Total: 8 claims.
    assert_eq!(s3.len(), 8);

    // Original entry preserved.
    assert!(
        has_claim(
            &s3,
            "JournalEntry",
            &[subj("entry_001"), subj("d_2026_04_15"), subj("p_2026_04")],
        ),
        "original entry must remain in admitted state"
    );
    assert!(
        has_line(
            &s3,
            &[subj("entry_001"), subj("account_cash"), dec(100), dec(0)],
        ),
        "original debit line must remain"
    );

    // New entry present at the corrected amount.
    assert!(has_claim(
        &s3,
        "JournalEntry",
        &[subj("entry_002"), subj("d_2026_05_10"), subj("p_2026_04")],
    ));
    assert!(has_line(
        &s3,
        &[subj("entry_002"), subj("account_cash"), dec(101), dec(0)],
    ));

    // Supersession lineage recorded.
    assert!(has_claim(
        &s3,
        "Supersedes",
        &[subj("entry_002"), subj("entry_001")],
    ));

    // Period still closed.
    assert!(has_claim(&s3, "PeriodClosed", &[subj("p_2026_04")]));
}

#[test]
fn lone_journal_entry_without_lines_violates_invariant() {
    // A JournalEntry with no lines balances trivially (both sums are 0), so
    // `journal_entry_has_lines` requires at least one. No transformation can
    // produce this state, so the invariant is evaluated directly.
    let state = State::from_claims(vec![claim_instance(
        "JournalEntry",
        &[subj("orphan"), subj("d_2026_04_15"), subj("p_2026_04")],
    )]);
    let inv = double_entry_ledger::journal_entry_has_lines();
    let holds = eval_invariant(&double_entry_ledger::program(), &inv, &state, None)
        .expect("evaluation should not error");
    assert!(
        !holds,
        "a JournalEntry with no matching JournalLine must violate journal_entry_has_lines"
    );
}

#[test]
fn cannot_restate_already_restated_entry() {
    // Post, restate, then restate the original again. The second restatement
    // is refused by the `require not exists newer: Supersedes(newer, prior)`
    // gate, with the `Supersedes` uniqueness (`supersedes_unique_by_prior_entry_id`) as backstop.
    let s1 = ex().must_accept(
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj("entry_001"),
            subj("d_2026_04_15"),
            subj("p_2026_04"),
            subj("account_cash"),
            subj("account_revenue"),
            dec(100),
        ],
        State::default(),
    );

    let s2 = ex().must_accept(
        &double_entry_ledger::restate_entry(),
        vec![
            subj("entry_002"),
            subj("entry_001"),
            subj("d_2026_05_10"),
            subj("p_2026_04"),
            subj("account_cash"),
            subj("account_revenue"),
            dec(101),
        ],
        s1,
    );

    let restate = |new: &str, prior: &str| {
        vec![
            subj(new),
            subj(prior),
            subj("d_2026_05_20"),
            subj("p_2026_04"),
            subj("account_cash"),
            subj("account_revenue"),
            dec(102),
        ]
    };
    refused_by(
        &ex().must_reject(
            &double_entry_ledger::restate_entry(),
            restate("entry_003", "entry_001"),
            &s2,
        ),
        "prior_entry_not_yet_superseded",
    );
    // A restatement takes an entry id of its own: restating entry_002
    // under entry_001's id would merge two entries' lines under one id.
    refused_by(
        &ex().must_reject(
            &double_entry_ledger::restate_entry(),
            restate("entry_001", "entry_002"),
            &s2,
        ),
        "new_entry_id_unused",
    );
}

#[test]
fn identical_credit_lines_stay_two_lines() {
    // Debit 100 to cash; credit 50 to revenue twice. Without a line id of
    // their own the two credit lines would be one claim, the credits would
    // sum to 50, and this balanced entry would be refused.
    let args = vec![
        subj("entry_001"),
        subj("d_2026_04_15"),
        subj("p_2026_04"),
        subj("account_cash"),
        dec(100),
        subj("account_revenue"),
        dec(50),
        subj("account_revenue"),
        dec(50),
    ];
    let state = ex().must_accept(
        &double_entry_ledger::post_split_entry(),
        args,
        State::default(),
    );

    let line_ids: std::collections::HashSet<_> = state
        .claims_for("JournalLine")
        .map(|c| c.args[1].clone())
        .collect();
    assert_eq!(line_ids.len(), 3, "three lines, three line ids");

    let rows = enumerate_derived(
        &double_entry_ledger::program(),
        &double_entry_ledger::trial_balance_row(),
        &state,
    )
    .expect("the trial balance evaluates");
    for (account, balance) in [("account_cash", 100), ("account_revenue", -100)] {
        let row = claim_instance("TrialBalanceRow", &[subj(account), dec(balance)]);
        assert!(rows.contains(&row), "expected {row:?}; got {rows:#?}");
    }
}

#[test]
fn an_entry_id_names_one_entry_even_without_the_gate() {
    // The posting act with its entry-id gate removed: an act someone could
    // write next. The declared key, not the gate, refuses a second entry
    // under the same id in another period.
    let mut program = double_entry_ledger::program();
    for t in &mut program.transformations {
        t.body.retain(|s| {
            !matches!(s, Stmt::Require { name: Some(n), .. } if n.as_str() == "entry_id_unused")
        });
    }
    let ungated = Example::new(&program);
    let post = program.transformation("post_simple_entry").unwrap().clone();
    let posting = |date: &str, period: &str| {
        vec![
            subj("entry_001"),
            subj(date),
            subj(period),
            subj("account_cash"),
            subj("account_revenue"),
            dec(10),
        ]
    };
    let state = ungated.must_accept(
        &post,
        posting("d_2026_11_01", "p_2026_11"),
        State::default(),
    );
    refused_by(
        &ungated.must_reject(&post, posting("d_2026_10_31", "p_2026_10"), &state),
        "journal_entry_unique_by_entry_id",
    );
}
