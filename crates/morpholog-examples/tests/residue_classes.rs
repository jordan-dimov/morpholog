//! Integration tests for the residue-classes example
//! (`examples/22_residue_classes/`).
//!
//! An explorer proposes a change of representation: the residues modulo
//! 4 and a table of their squares. The record recomputes every entry,
//! verifies two finite statements exhaustively over a domain it has
//! confirmed complete, keeps a checker's report about a lemma apart from
//! its own verification, and lets a conclusion stand only on both. These
//! tests walk that path end to end and pin each refusal: a wrong square,
//! an incomplete or non-integer domain, a statement the table does not
//! bear out (squares modulo 8), a statement the rules do not know, a
//! deduction with a missing ground, and a check recorded by a login that
//! is not the checker. The login binding itself is a database policy,
//! tested in the CLI suite.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::OnceLock;

use common::{Example, coll, dec, dec_str, has_claim, subj};
use morpholog_core::{EvalError, EvalValue, Outcome, RejectionReason, State};
use morpholog_examples::residue_classes as rc;
use morpholog_test_support::enumerate_derived;

const EXPLORER: &str = "explorer";
const OPERATOR: &str = "operator";
const CHECKER: &str = "lean4";
const LEMMA: &str = "reduction_respects_arithmetic";
const SQUARES: &str = "squares_are_zero_or_one";
const ODD_LEGS: &str = "no_square_is_a_sum_of_two_odd_squares";

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&rc::program()))
}

fn accept_as(
    state: State,
    t: &morpholog_core::Transformation,
    args: Vec<EvalValue>,
    actor: &str,
) -> State {
    ex().must_accept_as(t, args, actor, state)
}

fn rejected_by(outcome: &Result<Outcome, EvalError>, rule: &str) -> bool {
    matches!(
        outcome,
        Ok(Outcome::Rejected {
            reason: RejectionReason::Invariant { name, .. },
            ..
        }) if name.as_str() == rule
    )
}

fn gate_rejected(outcome: &Result<Outcome, EvalError>) -> bool {
    matches!(
        outcome,
        Ok(Outcome::Rejected {
            reason: RejectionReason::Require { .. },
            ..
        })
    )
}

fn residues(modulus: i64) -> EvalValue {
    coll((0..modulus).map(dec).collect())
}

/// The residues modulo `modulus`, sealed, with every square tabulated
/// correctly.
fn tabulated(modulus: i64) -> State {
    let mut s = State::default();
    s = accept_as(
        s,
        &rc::declare_domain(),
        vec![dec(modulus), residues(modulus)],
        EXPLORER,
    );
    for r in 0..modulus {
        s = accept_as(
            s,
            &rc::tabulate_square(),
            vec![dec(modulus), dec(r), dec((r * r) % modulus)],
            EXPLORER,
        );
    }
    s
}

/// Modulo 4 tabulated, both finite statements verified, the operator
/// and the checker appointed.
fn verified_mod4() -> State {
    let mut s = tabulated(4);
    s = accept_as(s, &rc::verify(), vec![subj(SQUARES), dec(4)], EXPLORER);
    s = accept_as(s, &rc::verify(), vec![subj(ODD_LEGS), dec(4)], EXPLORER);
    s = accept_as(
        s,
        &rc::appoint_operator(),
        vec![subj(OPERATOR), subj("operator_login")],
        OPERATOR,
    );
    accept_as(
        s,
        &rc::appoint_checker(),
        vec![subj(CHECKER), subj("checker_login")],
        OPERATOR,
    )
}

fn checked(state: State) -> State {
    accept_as(
        state,
        &rc::record_check(),
        vec![subj(LEMMA), subj("lean4:v4.35.0"), subj("sha256:reduction")],
        CHECKER,
    )
}

fn deduce_both(state: State) -> State {
    let s = accept_as(
        state,
        &rc::deduce(),
        vec![
            subj("every_square_is_zero_or_one_mod_four"),
            subj(SQUARES),
            dec(4),
            subj(LEMMA),
        ],
        EXPLORER,
    );
    accept_as(
        s,
        &rc::deduce(),
        vec![
            subj("no_pythagorean_triple_has_two_odd_legs"),
            subj(ODD_LEGS),
            dec(4),
            subj(LEMMA),
        ],
        EXPLORER,
    )
}

fn grounded(state: &State) -> Vec<Vec<EvalValue>> {
    let mut rows: Vec<Vec<EvalValue>> = enumerate_derived(&rc::program(), &rc::grounded(), state)
        .expect("the read enumerates")
        .into_iter()
        .map(|c| c.args)
        .collect();
    rows.sort_by_key(|r| format!("{r:?}"));
    rows
}

#[test]
fn both_exercises_are_verified_checked_and_deduced() {
    let state = deduce_both(checked(verified_mod4()));
    assert!(has_claim(&state, "Verified", &[subj(SQUARES), dec(4)]));
    assert!(has_claim(&state, "Verified", &[subj(ODD_LEGS), dec(4)]));
    let rows = grounded(&state);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(
        rows.iter()
            .all(|r| r[2] == subj(LEMMA) && r[3] == subj(CHECKER) && r[4] == dec(4))
    );
}

#[test]
fn a_withdrawn_check_drops_the_conclusions_from_the_read_and_erases_nothing() {
    let state = deduce_both(checked(verified_mod4()));
    let state = accept_as(
        state,
        &rc::withdraw_check(),
        vec![subj(LEMMA), subj("proof_regressed")],
        CHECKER,
    );
    assert!(grounded(&state).is_empty());
    assert!(has_claim(
        &state,
        "Checked",
        &[
            subj(LEMMA),
            subj(CHECKER),
            subj("lean4:v4.35.0"),
            subj("sha256:reduction")
        ]
    ));
    assert!(has_claim(
        &state,
        "Deduced",
        &[
            subj("every_square_is_zero_or_one_mod_four"),
            subj(SQUARES),
            dec(4),
            subj(LEMMA)
        ]
    ));
}

#[test]
fn squares_modulo_eight_are_not_all_zero_or_one() {
    let state = tabulated(8);
    let outcome = ex().propose_as(&rc::verify(), vec![subj(SQUARES), dec(8)], EXPLORER, &state);
    assert!(
        rejected_by(&outcome, "squares_are_zero_or_one"),
        "got: {outcome:?}"
    );
    let Ok(Outcome::Rejected {
        reason: RejectionReason::Invariant { witness, .. },
        ..
    }) = outcome
    else {
        unreachable!()
    };
    assert!(
        witness
            .iter()
            .any(|b| b.var.as_str() == "s" && b.value == dec(4)),
        "the refusal names the square that is 4: {witness:?}"
    );
}

#[test]
fn a_wrong_square_is_refused_by_the_recompute() {
    let state = accept_as(
        State::default(),
        &rc::declare_domain(),
        vec![dec(3), residues(3)],
        EXPLORER,
    );
    let outcome = ex().propose_as(
        &rc::tabulate_square(),
        vec![dec(3), dec(2), dec(2)],
        EXPLORER,
        &state,
    );
    assert!(
        rejected_by(&outcome, "square_is_the_recompute"),
        "got: {outcome:?}"
    );
}

#[test]
fn a_domain_that_is_not_the_whole_set_of_residues_is_refused() {
    let missing = ex().propose_as(
        &rc::declare_domain(),
        vec![dec(3), coll(vec![dec(0), dec(1)])],
        EXPLORER,
        &State::default(),
    );
    assert!(
        rejected_by(&missing, "sealed_domain_is_complete"),
        "got: {missing:?}"
    );
    let fractional = ex().propose_as(
        &rc::declare_domain(),
        vec![dec(3), coll(vec![dec(0), dec(1), dec_str("1.5")])],
        EXPLORER,
        &State::default(),
    );
    assert!(
        rejected_by(&fractional, "residue_is_an_integer_in_range"),
        "got: {fractional:?}"
    );
}

#[test]
fn a_statement_is_verified_only_over_a_sealed_domain_with_every_square() {
    let unsealed = ex().propose_as(
        &rc::verify(),
        vec![subj(SQUARES), dec(5)],
        EXPLORER,
        &State::default(),
    );
    assert!(
        rejected_by(&unsealed, "verified_domain_is_sealed"),
        "got: {unsealed:?}"
    );

    let mut partial = accept_as(
        State::default(),
        &rc::declare_domain(),
        vec![dec(4), residues(4)],
        EXPLORER,
    );
    for r in [0, 1, 3] {
        partial = accept_as(
            partial,
            &rc::tabulate_square(),
            vec![dec(4), dec(r), dec((r * r) % 4)],
            EXPLORER,
        );
    }
    let outcome = ex().propose_as(
        &rc::verify(),
        vec![subj(SQUARES), dec(4)],
        EXPLORER,
        &partial,
    );
    assert!(
        rejected_by(&outcome, "verified_squares_are_total"),
        "the row for 2 is missing: {outcome:?}"
    );
}

#[test]
fn a_statement_the_rules_do_not_know_cannot_be_verified() {
    let outcome = ex().propose_as(
        &rc::verify(),
        vec![subj("every_square_is_even"), dec(4)],
        EXPLORER,
        &tabulated(4),
    );
    assert!(
        rejected_by(&outcome, "only_a_stated_statement_is_verified"),
        "got: {outcome:?}"
    );
}

#[test]
fn a_deduction_with_a_missing_ground_is_refused() {
    let state = verified_mod4();
    let unchecked = ex().propose_as(
        &rc::deduce(),
        vec![subj("c"), subj(SQUARES), dec(4), subj(LEMMA)],
        EXPLORER,
        &state,
    );
    assert!(
        rejected_by(&unchecked, "deduction_rests_on_a_checked_lemma"),
        "got: {unchecked:?}"
    );
    let unverified = ex().propose_as(
        &rc::deduce(),
        vec![subj("c"), subj(SQUARES), dec(8), subj(LEMMA)],
        EXPLORER,
        &checked(state),
    );
    assert!(
        rejected_by(&unverified, "deduction_rests_on_a_verified_statement"),
        "got: {unverified:?}"
    );
}

#[test]
fn only_an_appointed_checker_records_a_check_and_only_the_operator_appoints() {
    let state = verified_mod4();
    let forged = ex().propose_as(
        &rc::record_check(),
        vec![subj(LEMMA), subj("v"), subj("sha256:x")],
        EXPLORER,
        &state,
    );
    assert!(gate_rejected(&forged), "got: {forged:?}");
    let self_appointed = ex().propose_as(
        &rc::appoint_checker(),
        vec![subj(EXPLORER), subj("explorer_login")],
        EXPLORER,
        &state,
    );
    assert!(gate_rejected(&self_appointed), "got: {self_appointed:?}");
}
