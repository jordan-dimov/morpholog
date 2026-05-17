//! Spike test for Example 5 (derived claims).
//!
//! This file is a *spike*: it documents what derived-claim support
//! should let a caller express, before any kernel work to support it
//! has landed. See `docs/derived-claims-sketch.md` for the design
//! intent and open questions.
//!
//! Two tests:
//!
//! 1. `trial_balance_today_requires_manual_evaluator_glue` runs and
//!    passes. It computes a trial balance over Example 4's double-
//!    entry ledger by iterating over `state.claims` in plain Rust,
//!    duplicating logic that should live in the kernel. The point is
//!    to show how unsatisfying the current state is: every caller
//!    that wants a trial balance writes this same loop, outside the
//!    governed model.
//!
//! 2. `trial_balance_as_derived_claim` is `#[ignore]`'d and panics.
//!    It documents the target API for derived claims even though no
//!    such API exists yet. CI stays green; the test runs explicitly
//!    via `cargo test -- --ignored` once the implementation PR lands.
//!    Its `#[ignore]` attribute should be removed in that PR.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{dec, must_accept, subj};
use morpholog_core::examples::double_entry_ledger;
use morpholog_core::{EvalValue, State};
use rust_decimal::Decimal;
use std::collections::BTreeMap;

/// Helper: post a journal entry with the given parameters.
fn post(
    state: State,
    entry_id: &str,
    date: &str,
    period: &str,
    debit_account: &str,
    credit_account: &str,
    amount: i64,
) -> State {
    must_accept(
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj(entry_id),
            subj(date),
            subj(period),
            subj(debit_account),
            subj(credit_account),
            dec(amount),
        ],
        state,
        &double_entry_ledger::all_invariants(),
    )
}

/// Build a small ledger with three postings against the same period:
///
///   1. cash debit 100 / revenue credit 100
///   2. cash debit  50 / revenue credit  50
///   3. expenses debit 30 / cash credit 30
///
/// Expected trial balance:
///   - account_cash:     +100 +50 - 30 = +120 (net debit)
///   - account_revenue:  -100 -50      = -150 (net credit, signed)
///   - account_expenses: +30           =  +30 (net debit)
fn small_ledger_state() -> State {
    let s = post(
        State::default(),
        "e1",
        "d1",
        "p1",
        "account_cash",
        "account_revenue",
        100,
    );
    let s = post(s, "e2", "d2", "p1", "account_cash", "account_revenue", 50);
    post(s, "e3", "d3", "p1", "account_expenses", "account_cash", 30)
}

#[test]
fn trial_balance_today_requires_manual_evaluator_glue() {
    // Computes the trial balance over the small ledger above WITHOUT
    // any kernel support for derived claims. The caller has to:
    //
    // 1. Iterate over every claim in state.
    // 2. Filter to `JournalLine` claims.
    // 3. Pull out the per-line account, debit, credit by hand.
    // 4. Maintain a per-account running total.
    // 5. Subtract credit from debit per account.
    //
    // None of this lives in the model. None of it is governed. Every
    // caller that wants a trial balance writes this loop, in whatever
    // host language they happen to use. The kernel's `eval_value` is
    // private and would not help even if it were public, because
    // there is no way to declare "trial balance" alongside the
    // invariants and transformations of the ledger program.
    //
    // The shape this test takes IS THE PROBLEM Example 5 should
    // solve. The next test in this file shows the target shape.

    let state = small_ledger_state();
    let mut balances: BTreeMap<String, Decimal> = BTreeMap::new();

    for claim in &state.claims {
        if claim.predicate != "JournalLine" {
            continue;
        }
        // JournalLine(entry_id, account, debit, credit)
        let EvalValue::Subject(account) = &claim.args[1] else {
            panic!("JournalLine arg 1 must be a Subject");
        };
        let EvalValue::Decimal(debit) = &claim.args[2] else {
            panic!("JournalLine arg 2 must be a Decimal");
        };
        let EvalValue::Decimal(credit) = &claim.args[3] else {
            panic!("JournalLine arg 3 must be a Decimal");
        };
        let entry = balances.entry(account.clone()).or_default();
        *entry += debit;
        *entry -= credit;
    }

    // Verified expected balances per the docstring on small_ledger_state.
    assert_eq!(balances.len(), 3);
    assert_eq!(balances["account_cash"], Decimal::new(120, 0));
    assert_eq!(balances["account_revenue"], Decimal::new(-150, 0));
    assert_eq!(balances["account_expenses"], Decimal::new(30, 0));
}

#[test]
#[ignore = "spike: target API for derived claims (Example 5). Kernel \
            support lands in the implementation PR; see \
            docs/derived-claims-sketch.md for the design questions \
            this test surfaces. Remove the `ignore` attribute when \
            the implementation PR lands."]
fn trial_balance_as_derived_claim() {
    // TARGET BEHAVIOUR (pseudocode; does not compile or run today).
    //
    // The double-entry ledger program would expose a derived claim
    // alongside its invariants and transformations:
    //
    //     pub fn trial_balance_row() -> DerivedClaim {
    //         DerivedClaim {
    //             predicate: "TrialBalanceRow".to_string(),
    //             parameters: vec!["account".to_string(),
    //                              "balance".to_string()],
    //             body: Expr::Eq(
    //                 Box::new(Expr::Term(var("balance"))),
    //                 // Either Sub(Sum, Sum) - if we go with
    //                 // adding Expr::Sub - or a single Sum over
    //                 // an expression value - if we extend Sum's
    //                 // value position. The spike does not
    //                 // commit to one shape; the design doc lists
    //                 // both as open questions.
    //                 todo!("Expr::Sub or extended Sum"),
    //             ),
    //         }
    //     }
    //
    // The CLI / external caller would then evaluate the derived
    // claim against state and receive grounded ClaimInstances:
    //
    //     let state = small_ledger_state();
    //     let trial_balance = double_entry_ledger::trial_balance_row();
    //     let rows = enumerate_derived(&trial_balance, &state).unwrap();
    //
    //     assert_eq!(rows.len(), 3);
    //     assert!(rows.contains(&ClaimInstance {
    //         predicate: "TrialBalanceRow".to_string(),
    //         args: vec![subj("account_cash"), dec(120)],
    //     }));
    //     assert!(rows.contains(&ClaimInstance {
    //         predicate: "TrialBalanceRow".to_string(),
    //         args: vec![subj("account_revenue"), dec(-150)],
    //     }));
    //     assert!(rows.contains(&ClaimInstance {
    //         predicate: "TrialBalanceRow".to_string(),
    //         args: vec![subj("account_expenses"), dec(30)],
    //     }));
    //
    // What the spike forces the implementation PR to decide (per
    // docs/derived-claims-sketch.md):
    //
    //   - DerivedClaim struct shape and where it lives in the IR.
    //   - Subtraction primitive: Expr::Sub vs extended Sum value.
    //   - Enumeration semantics: how does the runtime know to iterate
    //     "one row per distinct account"?
    //   - The enumerate_derived signature itself.
    //   - Whether derived claims are added to Program or live elsewhere.

    panic!(
        "Spike test: kernel does not yet support derived claims. \
         This test pins the target behaviour for Example 5; see \
         docs/derived-claims-sketch.md. Remove the `#[ignore]` \
         attribute when the implementation PR lands."
    );
}
