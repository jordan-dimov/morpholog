//! Integration tests for the double-entry ledger example
//! (`examples/03_double_entry_ledger/`).
//!
//! Proves: posted entries must balance (sum of debits = sum of
//! credits); period close gates further normal posting via
//! `require`; closed periods can be restated through a separate
//! transformation that preserves the original entry and records
//! `Supersedes` lineage; re-restatement is forbidden by the
//! at-most-one-direct-successor invariant; double-closing the same
//! period is rejected.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{claim_instance, dec, has_claim, must_accept, subj};
use morpholog_core::{Outcome, State, eval_invariant};
use morpholog_examples::double_entry_ledger;

#[test]
fn simple_entry_balances_and_commits() {
    let state = must_accept(
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
        &double_entry_ledger::all_invariants(),
    );

    assert_eq!(state.len(), 3, "1 JournalEntry + 2 JournalLine");
    assert!(has_claim(
        &state,
        "JournalEntry",
        &[subj("entry_001"), subj("d_2026_04_15"), subj("p_2026_04")],
    ));
    assert!(has_claim(
        &state,
        "JournalLine",
        &[subj("entry_001"), subj("account_cash"), dec(100), dec(0)],
    ));
    assert!(has_claim(
        &state,
        "JournalLine",
        &[subj("entry_001"), subj("account_revenue"), dec(0), dec(100)],
    ));
}

#[test]
fn split_entry_balances_and_commits() {
    // Debit 100 to cash; credit 70 to revenue + 30 to deferred revenue.
    // Sums: debits = 100; credits = 70 + 30 = 100. Balance invariant
    // holds.
    let state = must_accept(
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
            dec(30),
        ],
        State::default(),
        &double_entry_ledger::all_invariants(),
    );

    assert_eq!(state.len(), 4, "1 JournalEntry + 3 JournalLine");
    // Three lines: 100 debit cash, 70 credit revenue, 30 credit deferred.
    assert!(has_claim(
        &state,
        "JournalLine",
        &[subj("entry_001"), subj("account_cash"), dec(100), dec(0)],
    ));
    assert!(has_claim(
        &state,
        "JournalLine",
        &[subj("entry_001"), subj("account_revenue"), dec(0), dec(70)],
    ));
    assert!(has_claim(
        &state,
        "JournalLine",
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
    // Debit 100; credit 70 + 25 = 95. Mismatch of 5. The require
    // checks pass (period is open, etc.), the transformation stages
    // the journal entry + three lines, and the candidate state
    // violates `balanced_posted_entry`. Atomic rollback: no claim
    // is admitted.
    let outcome = common::propose_with_test_actor(
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
        &double_entry_ledger::all_invariants(),
    )
    .expect("propose should not error");

    let Outcome::Rejected { reason } = outcome else {
        panic!("expected Rejected, got {outcome:?}");
    };
    assert!(
        reason.contains("balanced_posted_entry"),
        "got reason: {reason}"
    );
}

#[test]
fn closed_period_rejects_normal_posting() {
    // Close the period, then try to post a normal entry to it.
    // `require not PeriodClosed(period)` catches it at admission;
    // no claim is admitted.
    let after_close = must_accept(
        &double_entry_ledger::close_period(),
        vec![subj("p_2026_04")],
        State::default(),
        &double_entry_ledger::all_invariants(),
    );

    assert!(has_claim(
        &after_close,
        "PeriodClosed",
        &[subj("p_2026_04")]
    ));

    let outcome = common::propose_with_test_actor(
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
        &double_entry_ledger::all_invariants(),
    )
    .expect("propose should not error");

    let Outcome::Rejected { reason } = outcome else {
        panic!("expected Rejected, got {outcome:?}");
    };
    assert!(reason.contains("require"), "got reason: {reason}");
}

#[test]
fn double_close_rejected() {
    let after_close = must_accept(
        &double_entry_ledger::close_period(),
        vec![subj("p_2026_04")],
        State::default(),
        &double_entry_ledger::all_invariants(),
    );

    let outcome = common::propose_with_test_actor(
        &double_entry_ledger::close_period(),
        vec![subj("p_2026_04")],
        &after_close,
        &double_entry_ledger::all_invariants(),
    )
    .expect("propose should not error");

    let Outcome::Rejected { reason } = outcome else {
        panic!("expected Rejected, got {outcome:?}");
    };
    assert!(reason.contains("require"), "got reason: {reason}");
}

#[test]
fn restatement_into_closed_period_preserves_original() {
    // 1. Post an entry: cash debit 100, revenue credit 100.
    let s1 = must_accept(
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
        &double_entry_ledger::all_invariants(),
    );

    // 2. Close the period.
    let s2 = must_accept(
        &double_entry_ledger::close_period(),
        vec![subj("p_2026_04")],
        s1,
        &double_entry_ledger::all_invariants(),
    );

    // 3. Restate the entry with a corrected amount (101 instead of
    //    100). The restatement transformation does *not* check
    //    PeriodClosed - restatement is the path for closed periods.
    let s3 = must_accept(
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
        &double_entry_ledger::all_invariants(),
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
        has_claim(
            &s3,
            "JournalLine",
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
    assert!(has_claim(
        &s3,
        "JournalLine",
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
    // The `balanced_posted_entry` invariant trivially admits a
    // JournalEntry with zero lines (both sums are 0). The
    // `journal_entry_has_lines` invariant closes that gap: a
    // JournalEntry must have at least one matching JournalLine.
    //
    // None of the supplied transformations can produce this state
    // (post_simple_entry, post_split_entry, and restate_entry all
    // assert at least two lines), so this test evaluates the
    // invariant directly against a hand-crafted state that no
    // legitimate path could reach.
    let state = State::from_claims(vec![claim_instance(
        "JournalEntry",
        &[subj("orphan"), subj("d_2026_04_15"), subj("p_2026_04")],
    )]);
    let inv = double_entry_ledger::journal_entry_has_lines();
    let holds = eval_invariant(&inv, &state, None).expect("evaluation should not error");
    assert!(
        !holds,
        "a JournalEntry with no matching JournalLine must violate journal_entry_has_lines"
    );
}

#[test]
fn cannot_restate_already_restated_entry() {
    // Post -> restate once -> attempt to restate the prior entry
    // again. The second restatement must be rejected: either by
    // the `require not exists newer: Supersedes(newer, prior)`
    // check at admission, or (if that were bypassed somehow) by
    // the at_most_one_direct_successor invariant on candidate
    // state.
    let s1 = must_accept(
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
        &double_entry_ledger::all_invariants(),
    );

    let s2 = must_accept(
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
        &double_entry_ledger::all_invariants(),
    );

    let outcome = common::propose_with_test_actor(
        &double_entry_ledger::restate_entry(),
        vec![
            subj("entry_003"),
            subj("entry_001"),
            subj("d_2026_05_20"),
            subj("p_2026_04"),
            subj("account_cash"),
            subj("account_revenue"),
            dec(102),
        ],
        &s2,
        &double_entry_ledger::all_invariants(),
    )
    .expect("propose should not error");

    let Outcome::Rejected { reason } = outcome else {
        panic!("expected Rejected, got {outcome:?}");
    };
    assert!(reason.contains("require"), "got reason: {reason}");
}
