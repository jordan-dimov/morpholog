//! Integration tests for the margin call run example
//! (`examples/14_margin_call_run/`).
//!
//! The gallery's first *set-valued proposal*: the risk engine hands the
//! runtime the whole batch of called accounts as one all-or-nothing
//! decision (a collection argument). These tests pin the example's
//! teaching point - completeness, not merely correctness: a *missing*
//! call is refused, not just a wrong one - alongside the exclusion control
//! (no adequately-margined account may be called) and the exact top-up.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::OnceLock;

use common::{Example, claim_instance, coll, date, has_claim, qty, subj, test_actor};
use morpholog_core::{EvalError, Outcome, State};
use morpholog_examples::margin_call_run;

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&margin_call_run::program()))
}

/// A book at the start of the day: two accounts have fallen below their
/// maintenance floor (each must be called), one sits comfortably above it
/// (it must not).
fn book() -> State {
    State::from_claims(vec![
        // acct_short_a: equity 60k below the 70k floor; tops up to 100k -> 40k call.
        claim_instance(
            "RequiredMargin",
            &[subj("acct_short_a"), qty("100000", "USD")],
        ),
        claim_instance(
            "MaintenanceMargin",
            &[subj("acct_short_a"), qty("70000", "USD")],
        ),
        claim_instance(
            "AccountEquity",
            &[subj("acct_short_a"), qty("60000", "USD")],
        ),
        // acct_short_b: equity 30k below the 35k floor; tops up to 50k -> 20k call.
        claim_instance(
            "RequiredMargin",
            &[subj("acct_short_b"), qty("50000", "USD")],
        ),
        claim_instance(
            "MaintenanceMargin",
            &[subj("acct_short_b"), qty("35000", "USD")],
        ),
        claim_instance(
            "AccountEquity",
            &[subj("acct_short_b"), qty("30000", "USD")],
        ),
        // acct_ok: equity 90k above the 70k floor; must NOT be called.
        claim_instance("RequiredMargin", &[subj("acct_ok"), qty("100000", "USD")]),
        claim_instance("MaintenanceMargin", &[subj("acct_ok"), qty("70000", "USD")]),
        claim_instance("AccountEquity", &[subj("acct_ok"), qty("90000", "USD")]),
    ])
}

/// Propose a margin run calling `accounts` - the batch handed over as one
/// collection argument.
fn run(accounts: Vec<&str>, state: &State) -> Result<Outcome, EvalError> {
    ex().propose_as(
        &margin_call_run::issue_margin_run(),
        vec![
            subj("run_2026_06_29"),
            date("2026-06-29"),
            coll(accounts.into_iter().map(subj).collect()),
        ],
        test_actor(),
        state,
    )
}

fn is_rejected(outcome: &Result<Outcome, EvalError>) -> bool {
    matches!(outcome, Ok(Outcome::Rejected { .. }))
}

#[test]
fn a_complete_and_exact_run_is_admitted() {
    let outcome = run(vec!["acct_short_a", "acct_short_b"], &book()).unwrap();
    let Outcome::Accepted {
        candidate_state, ..
    } = outcome
    else {
        panic!("expected the complete run to be admitted, got {outcome:?}");
    };
    // Each call is exactly the top-up to the required level.
    assert!(has_claim(
        &candidate_state,
        "MarginCall",
        &[
            subj("run_2026_06_29"),
            subj("acct_short_a"),
            qty("40000", "USD")
        ],
    ));
    assert!(has_claim(
        &candidate_state,
        "MarginCall",
        &[
            subj("run_2026_06_29"),
            subj("acct_short_b"),
            qty("20000", "USD")
        ],
    ));
}

#[test]
fn a_run_that_omits_an_undermargined_account_is_refused() {
    // The headline. Forgetting acct_short_b leaves it under-collateralised -
    // the dangerous mistake a margin process exists to prevent. The
    // completeness gate refuses the WHOLE run, so nothing is recorded until
    // every short account is included. A missing call is not a smaller
    // error than a wrong one; it is the one that matters.
    assert!(is_rejected(&run(vec!["acct_short_a"], &book())));
}

#[test]
fn calling_an_adequately_margined_account_is_refused() {
    // The other side: including acct_ok (above its floor) fails the
    // per-account gate. Only an account actually short of margin may be
    // called - the run cannot manufacture a demand against a healthy account.
    assert!(is_rejected(&run(
        vec!["acct_short_a", "acct_short_b", "acct_ok"],
        &book(),
    )));
}

#[test]
fn an_empty_book_admits_an_empty_run() {
    // Vacuous completeness: with nothing below its floor, a run that calls
    // no one is complete and is admitted. Totality refuses what is missing,
    // never demands calls that are not owed.
    let calm = State::from_claims(vec![
        claim_instance("RequiredMargin", &[subj("acct_ok"), qty("100000", "USD")]),
        claim_instance("MaintenanceMargin", &[subj("acct_ok"), qty("70000", "USD")]),
        claim_instance("AccountEquity", &[subj("acct_ok"), qty("90000", "USD")]),
    ]);
    assert!(matches!(
        run(vec![], &calm).unwrap(),
        Outcome::Accepted { .. }
    ));
}
