//! Integration tests for derived claims.
//!
//! Uses the double-entry ledger's trial balance to pin `enumerate_derived`:
//! one row per account with the right balance, a stable row order, no
//! derived rows written into state, and the subtraction contract on its own.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::OnceLock;

use common::{Example, claim_instance, dec, subj};
use morpholog_core::{
    ArithOp, DerivedClaim, DerivedValue, EvalValue, Prop, State, Term, ValueExpr, enumerate_derived,
};
use morpholog_examples::double_entry_ledger;

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&double_entry_ledger::program()))
}
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
    ex().must_accept(
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
    )
}

/// The small ledger fixture used by the tests below:
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
        enumerate_derived(&trial_balance, &state, &[]).expect("enumerate_derived should not error");

    assert_eq!(rows.len(), 3, "one row per distinct account");

    let expected = [
        ("account_cash", 120),
        ("account_revenue", -150),
        ("account_expenses", 30),
    ];
    for (account, balance) in expected {
        let expected_row = claim_instance("TrialBalanceRow", &[subj(account), dec(balance)]);
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

    let a = enumerate_derived(&trial_balance, &state, &[]).unwrap();
    let b = enumerate_derived(&trial_balance, &state, &[]).unwrap();

    // Two back-to-back evaluations must return rows in the same order.
    assert_eq!(a, b, "enumerate_derived must be deterministic across runs");

    // Subjects sort by their string content, so the accounts come out
    // alphabetically: cash < expenses < revenue.
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
    // A derived row is a computed view, never an admitted claim, so it
    // must not appear in state.claims. The `&State` signature already
    // forbids mutation; comparing against a clone catches any change to it.
    let state = small_ledger_state();
    let snapshot = state.clone();

    let _rows = enumerate_derived(
        &double_entry_ledger::trial_balance_row(),
        &state,
        &double_entry_ledger::definitions(),
    )
    .expect("enumerate_derived should not error");

    assert_eq!(
        state, snapshot,
        "enumerate_derived must not mutate the input State"
    );

    // No derived predicate name may appear in the live state either.
    assert!(
        state.claims_for("TrialBalanceRow").next().is_none(),
        "TrialBalanceRow must not appear among admitted claims after enumeration"
    );
}

#[test]
fn enumerate_derived_on_empty_state_is_empty() {
    let empty = State::default();
    let rows = enumerate_derived(
        &double_entry_ledger::trial_balance_row(),
        &empty,
        &double_entry_ledger::definitions(),
    )
    .unwrap();
    assert!(
        rows.is_empty(),
        "empty state means empty domain means no derived rows"
    );
}

#[test]
fn expr_sub_subtracts_decimals_and_rejects_other_types() {
    // Subtract two literal decimals over a one-claim domain, with no
    // ledger fixture.
    use morpholog_core::Value;

    let state = State::from_claims(vec![claim_instance("Tag", &[subj("only")])]);

    let derived_decimal_ok = DerivedClaim {
        predicate: "DecimalSub".into(),
        keys: vec!["k".into()],
        values: vec![DerivedValue {
            name: "result".into(),
            expr: ValueExpr::Arith {
                op: ArithOp::Sub,
                left: Box::new(ValueExpr::Term(Term::Literal(Value::Decimal(
                    "100".to_string(),
                )))),
                right: Box::new(ValueExpr::Term(Term::Literal(Value::Decimal(
                    "30".to_string(),
                )))),
            },
        }],
        domain: Prop::Claim {
            predicate: "Tag".into(),
            args: vec![Term::Var("k".into())],
        },
    };

    let rows = enumerate_derived(&derived_decimal_ok, &state, &[]).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].args[1], EvalValue::Decimal(Decimal::new(70, 0)));

    // Subtracting a subject from a decimal should be a TypeMismatch.
    let derived_type_error = DerivedClaim {
        predicate: "TypeError".into(),
        keys: vec!["k".into()],
        values: vec![DerivedValue {
            name: "result".into(),
            expr: ValueExpr::Arith {
                op: ArithOp::Sub,
                left: Box::new(ValueExpr::Term(Term::Literal(Value::Decimal(
                    "1".to_string(),
                )))),
                right: Box::new(ValueExpr::Term(Term::Literal(Value::Subject(
                    "not_a_number".into(),
                )))),
            },
        }],
        domain: Prop::Claim {
            predicate: "Tag".into(),
            args: vec![Term::Var("k".into())],
        },
    };

    let err = enumerate_derived(&derived_type_error, &state, &[]).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("no arithmetic rule for decimal Sub subject"),
        "the no-rule error names both operand kinds; got: {msg}"
    );
}

/// The trial balance reads only `JournalLine`, so the database adapter can
/// skip loading the ledger's other predicates.
#[test]
fn predicates_referenced_by_trial_balance_derived_excludes_unused_predicates() {
    use morpholog_core::{PredicateName, predicates_referenced_by_derived};
    use std::collections::BTreeSet;

    let derived = double_entry_ledger::trial_balance_row();
    let footprint = predicates_referenced_by_derived(&derived, &[]);
    let expected: BTreeSet<PredicateName> = ["JournalLine"]
        .iter()
        .map(|s| PredicateName::from(*s))
        .collect();
    assert_eq!(
        footprint, expected,
        "trial_balance_row reads only JournalLine; the derived's own \
         predicate (TrialBalanceRow) must not appear in the footprint, \
         and unrelated ledger predicates (JournalEntry, PeriodClosed, \
         etc.) must not appear either"
    );
}
