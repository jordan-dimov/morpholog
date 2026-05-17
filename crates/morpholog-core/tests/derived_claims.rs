//! Integration tests for derived claims (Example 5).
//!
//! Replaces the earlier `tests/derived_claims_spike.rs` from PR #19,
//! whose purpose was to document the target API before any kernel
//! support existed. With `DerivedClaim`, `DerivedValue`, `Expr::Sub`,
//! and `enumerate_derived` now in `morpholog-core`, the trial balance
//! over the double-entry ledger is expressible and testable.
//!
//! Tests:
//!
//! - `trial_balance_over_simple_ledger_enumerates_one_row_per_account`:
//!   the load-bearing case. Post three journal entries against the
//!   ledger, evaluate `double_entry_ledger::trial_balance_row()`, and
//!   assert one TrialBalanceRow per distinct account with the
//!   expected debit-minus-credit balance.
//!
//! - `trial_balance_returns_deterministic_order`: pin that
//!   `enumerate_derived` returns rows in a stable order across runs.
//!
//! - `derived_claims_do_not_pollute_admitted_state`: pin the v0
//!   contract that derived results are NOT added to `State.claims`.
//!
//! - `enumerate_derived_on_empty_state_is_empty`: edge case; no
//!   rows in domain means no derived rows.
//!
//! - `expr_sub_subtracts_decimals_and_rejects_other_types`: pins the
//!   new `Expr::Sub` primitive's contract independently of the
//!   trial-balance use case.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{dec, must_accept, subj};
use morpholog_core::examples::double_entry_ledger;
use morpholog_core::{
    ClaimInstance, DerivedClaim, DerivedValue, EvalValue, Expr, State, Term, enumerate_derived,
};
use rust_decimal::Decimal;

/// Helper: post one journal entry against the ledger.
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

/// The small ledger used in the spike test and now in the real test:
///   1. cash debit 100 / revenue credit 100
///   2. cash debit  50 / revenue credit  50
///   3. expenses debit 30 / cash credit 30
///
/// Expected balances: cash = +120, revenue = -150, expenses = +30.
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
fn trial_balance_over_simple_ledger_enumerates_one_row_per_account() {
    let state = small_ledger_state();
    let trial_balance = double_entry_ledger::trial_balance_row();
    let rows =
        enumerate_derived(&trial_balance, &state).expect("enumerate_derived should not error");

    assert_eq!(rows.len(), 3, "one row per distinct account");

    let expected = [
        ("account_cash", 120),
        ("account_revenue", -150),
        ("account_expenses", 30),
    ];
    for (account, balance) in expected {
        let expected_row = ClaimInstance {
            predicate: "TrialBalanceRow".to_string(),
            args: vec![subj(account), dec(balance)],
        };
        assert!(
            rows.contains(&expected_row),
            "expected row for `{account}` with balance {balance}; got: {rows:#?}"
        );
    }
}

#[test]
fn trial_balance_returns_deterministic_order() {
    let state = small_ledger_state();
    let trial_balance = double_entry_ledger::trial_balance_row();

    let a = enumerate_derived(&trial_balance, &state).unwrap();
    let b = enumerate_derived(&trial_balance, &state).unwrap();

    // Two back-to-back evaluations must return rows in the same order.
    // The ordering is contracted as deterministic; the specific order
    // is whatever the dedup BTreeSet produces (key-tuple ascending by
    // JSON serialisation, per the kernel docstring on EvalValueOrd).
    assert_eq!(a, b, "enumerate_derived must be deterministic across runs");

    // Spot-check the actual order is the JSON-ordering on the subject
    // strings. The three accounts sort as cash < expenses < revenue
    // (alphabetic on the codec's `{"type":"subject","value":"..."}`).
    let order: Vec<&str> = a
        .iter()
        .map(|r| match &r.args[0] {
            EvalValue::Subject(s) => s.as_str(),
            _ => panic!("first arg must be the account subject"),
        })
        .collect();
    assert_eq!(
        order,
        vec!["account_cash", "account_expenses", "account_revenue"]
    );
}

#[test]
fn derived_claims_do_not_pollute_admitted_state() {
    // v0 contract: a ClaimInstance returned by enumerate_derived is a
    // computed view, NOT an admitted assertion. Nothing in the
    // runtime adds it to state.claims. Pin this explicitly so any
    // future refactor that tries to "be helpful" by integrating
    // derived rows into State has to break a test.
    let state_before = small_ledger_state();
    let claim_count_before = state_before.claims.len();
    let predicates_before: Vec<&str> = state_before
        .claims
        .iter()
        .map(|c| c.predicate.as_str())
        .collect();

    let _rows = enumerate_derived(&double_entry_ledger::trial_balance_row(), &state_before)
        .expect("enumerate_derived should not error");

    // The state passed in is by reference (`&State`); even if it were
    // not, the function does not mutate. Verify both: the same state
    // value still has the same claim count and no `TrialBalanceRow`
    // predicate has snuck in.
    assert_eq!(state_before.claims.len(), claim_count_before);
    assert!(
        !predicates_before.contains(&"TrialBalanceRow"),
        "TrialBalanceRow must not appear among admitted claims"
    );
}

#[test]
fn enumerate_derived_on_empty_state_is_empty() {
    let empty = State::default();
    let rows = enumerate_derived(&double_entry_ledger::trial_balance_row(), &empty).unwrap();
    assert!(
        rows.is_empty(),
        "empty state means empty domain means no derived rows"
    );
}

#[test]
fn expr_sub_subtracts_decimals_and_rejects_other_types() {
    // Build a derived claim whose value is a simple subtraction of
    // two literal decimals, so we can test Expr::Sub directly without
    // needing a ledger fixture. The domain is one synthetic claim
    // that yields one key binding.
    use morpholog_core::Value;

    let mut state = State::default();
    state.claims.push(ClaimInstance {
        predicate: "Tag".to_string(),
        args: vec![subj("only")],
    });

    let derived_decimal_ok = DerivedClaim {
        predicate: "DecimalSub".to_string(),
        keys: vec!["k".to_string()],
        values: vec![DerivedValue {
            name: "result".to_string(),
            expr: Expr::Sub(
                Box::new(Expr::Term(Term::Literal(Value::Decimal("100".to_string())))),
                Box::new(Expr::Term(Term::Literal(Value::Decimal("30".to_string())))),
            ),
        }],
        domain: Expr::Claim {
            predicate: "Tag".to_string(),
            args: vec![Term::Var("k".to_string())],
        },
    };

    let rows = enumerate_derived(&derived_decimal_ok, &state).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].args[1], EvalValue::Decimal(Decimal::new(70, 0)));

    // Subtracting a subject from a decimal should be a TypeMismatch.
    let derived_type_error = DerivedClaim {
        predicate: "TypeError".to_string(),
        keys: vec!["k".to_string()],
        values: vec![DerivedValue {
            name: "result".to_string(),
            expr: Expr::Sub(
                Box::new(Expr::Term(Term::Literal(Value::Decimal("1".to_string())))),
                Box::new(Expr::Term(Term::Literal(Value::Subject(
                    "not_a_number".to_string(),
                )))),
            ),
        }],
        domain: Expr::Claim {
            predicate: "Tag".to_string(),
            args: vec![Term::Var("k".to_string())],
        },
    };

    let err = enumerate_derived(&derived_type_error, &state).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("Sub expects decimal"),
        "expected Sub type-mismatch error, got: {msg}"
    );
}
