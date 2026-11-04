//! A bounded obligation evaluates nothing outside its cases. The case
//! bindings are in place before the antecedent is matched, so the work
//! follows the cases rather than the state, and a case the change did
//! not reach can raise no error. Where several cases fail at once the
//! witness is the first failing match of the earliest case in the plan's
//! order, whatever order the state holds the claims in.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use crate::definitions::DefinitionTable;
use crate::derive::{eval_invariant, eval_invariant_cases, invariant_witness_cases};
use crate::eval::{EvalContext, find_matches};
use crate::impact::{Impact, ImpactPlan};
use crate::ir::{Prop, Var};
use crate::ir_builder::{
    and, claim, dec, div, implies, invariant, le, not, predicate, program, term, var,
};
use crate::kernel_tests::support::{claim_instance, dec as dv, subj};
use crate::state::Bindings;
use crate::{ClaimInstance, EvalError, EvalValue, Invariant, Program, State};

/// `Row(k, v) and 0 <= 1 / v implies Ok(k)`: an antecedent that errors
/// on a row holding zero.
fn rows() -> Program {
    program("rows")
        .predicates(vec![
            predicate("Row").subject("k").decimal("v").build(),
            predicate("Ok").subject("k").build(),
        ])
        .invariants(vec![invariant(
            "rows_are_ok",
            implies(
                and(vec![
                    claim("Row", vec![var("k"), var("v")]),
                    le(term(dec("0")), div(term(dec("1")), term(var("v")))),
                ]),
                claim("Ok", vec![var("k")]),
            ),
        )])
        .build()
}

/// `A(x, y) and B(y, z) implies C(x, z)`: two antecedent patterns whose
/// cases overlap on `y`.
fn chained() -> Program {
    program("chained")
        .predicates(vec![
            predicate("A").subject("x").subject("y").build(),
            predicate("B").subject("y").subject("z").build(),
            predicate("C").subject("x").subject("z").build(),
        ])
        .invariants(vec![invariant(
            "chained",
            implies(
                and(vec![
                    claim("A", vec![var("x"), var("y")]),
                    claim("B", vec![var("y"), var("z")]),
                ]),
                claim("C", vec![var("x"), var("z")]),
            ),
        )])
        .build()
}

fn case(pairs: &[(&str, EvalValue)]) -> BTreeMap<Var, EvalValue> {
    pairs
        .iter()
        .map(|(v, ev)| (Var::from(*v), ev.clone()))
        .collect()
}

/// The cases the plan bounds a delta of `asserted` to.
fn cases_of(inv: &Invariant, asserted: &[ClaimInstance]) -> Vec<BTreeMap<Var, EvalValue>> {
    match ImpactPlan::new(inv).classify(asserted, &[]) {
        Impact::Bounded(cases) => cases,
        other => panic!("the delta bounds the rule, got {other:?}"),
    }
}

/// What the evaluator did before: every antecedent match over the whole
/// state, then the touched cases kept. The oracle for the verdict.
fn whole_then_filter(
    inv: &Invariant,
    state: &State,
    cases: &[BTreeMap<Var, EvalValue>],
) -> Result<bool, EvalError> {
    let bindings = Bindings::new();
    let ctx = EvalContext::new(state, None, &bindings, None, DefinitionTable::new(&[]));
    let Prop::Implies { left, right } = &inv.body else {
        panic!("an implication");
    };
    let mut holds = true;
    for m in find_matches(left, &ctx)? {
        if !cases
            .iter()
            .any(|c| c.iter().all(|(v, ev)| m.get(v).map(|b| &**b) == Some(ev)))
        {
            continue;
        }
        if find_matches(right, &ctx.with_bindings(&m))?.is_empty() {
            holds = false;
        }
    }
    Ok(holds)
}

fn a(x: &str, y: &str) -> ClaimInstance {
    claim_instance("A", &[subj(x), subj(y)])
}
fn b(y: &str, z: &str) -> ClaimInstance {
    claim_instance("B", &[subj(y), subj(z)])
}
fn c(x: &str, z: &str) -> ClaimInstance {
    claim_instance("C", &[subj(x), subj(z)])
}

#[test]
fn the_bounded_verdict_is_the_whole_verdict_filtered_to_the_cases() {
    let p = chained();
    let inv = &p.invariants[0];
    let state = State::from_claims(vec![
        a("x1", "y1"),
        b("y1", "z1"),
        c("x1", "z1"),
        a("x2", "y2"),
        b("y2", "z2"),
        a("x3", "y1"),
        b("y1", "z3"),
    ]);
    let case_lists: Vec<Vec<BTreeMap<Var, EvalValue>>> = vec![
        vec![],
        vec![case(&[("x", subj("x1")), ("y", subj("y1"))])],
        vec![case(&[("x", subj("x2")), ("y", subj("y2"))])],
        vec![case(&[("y", subj("y1")), ("z", subj("z3"))])],
        vec![case(&[("y", subj("y1"))])],
        vec![
            case(&[("x", subj("x1")), ("y", subj("y1"))]),
            case(&[("y", subj("y1")), ("z", subj("z1"))]),
        ],
        vec![case(&[("x", subj("nobody")), ("y", subj("y1"))])],
        vec![case(&[("z", subj("z2"))]), case(&[("x", subj("x1"))])],
    ];
    for cases in &case_lists {
        assert_eq!(
            eval_invariant_cases(inv, &state, None, &[], cases),
            whole_then_filter(inv, &state, cases),
            "{cases:?}"
        );
    }
}

#[test]
fn an_untouched_case_raises_nothing_and_a_touched_one_still_does() {
    let p = rows();
    let inv = &p.invariants[0];
    let pre = State::from_claims(vec![
        claim_instance("Row", &[subj("zero"), dv(0)]),
        claim_instance("Row", &[subj("good"), dv(1)]),
        claim_instance("Ok", &[subj("good")]),
    ]);

    // A change beside the zero row: the whole walk met the zero and
    // errored; the bounded one never reaches it.
    let beside = cases_of(inv, &[claim_instance("Ok", &[subj("good")])]);
    assert_eq!(beside, vec![case(&[("k", subj("good"))])]);
    assert!(matches!(
        whole_then_filter(inv, &pre, &beside),
        Err(EvalError::DivisionByZero)
    ));
    assert_eq!(
        eval_invariant_cases(inv, &pre, None, &[], &beside),
        Ok(true)
    );
    let witness = invariant_witness_cases(inv, &pre, None, &[], &beside).unwrap();
    assert!(witness.witness.is_empty());

    // The zero row itself, touched, errors.
    let touched = cases_of(inv, &[claim_instance("Row", &[subj("zero"), dv(0)])]);
    assert!(matches!(
        eval_invariant_cases(inv, &pre, None, &[], &touched),
        Err(EvalError::DivisionByZero)
    ));

    // A failing case is refused and witnessed, not lost behind the error.
    let post = pre.with_delta(&[claim_instance("Row", &[subj("new"), dv(2)])], &[]);
    let failing = cases_of(inv, &[claim_instance("Row", &[subj("new"), dv(2)])]);
    assert_eq!(
        eval_invariant_cases(inv, &post, None, &[], &failing),
        Ok(false)
    );
    let witness = invariant_witness_cases(inv, &post, None, &[], &failing).unwrap();
    assert_eq!(witness.witness.len(), 2, "{:?}", witness.witness);
    assert_eq!(witness.witness[0].var.as_str(), "k");
    assert_eq!(witness.witness[0].value, subj("new"));
}

#[test]
fn overlapping_partial_cases_judge_one_match_once_and_witness_it_whole() {
    let p = chained();
    let inv = &p.invariants[0];
    let state = State::from_claims(vec![a("x1", "y1"), b("y1", "z1")]);
    let cases = cases_of(inv, &[a("x1", "y1"), b("y1", "z1")]);
    assert_eq!(
        cases,
        vec![
            case(&[("x", subj("x1")), ("y", subj("y1"))]),
            case(&[("y", subj("y1")), ("z", subj("z1"))]),
        ]
    );
    assert_eq!(
        eval_invariant_cases(inv, &state, None, &[], &cases),
        Ok(false)
    );
    let witness = invariant_witness_cases(inv, &state, None, &[], &cases).unwrap();
    let named: Vec<(&str, &EvalValue)> = witness
        .witness
        .iter()
        .map(|w| (w.var.as_str(), &w.value))
        .collect();
    assert_eq!(
        named,
        vec![("x", &subj("x1")), ("y", &subj("y1")), ("z", &subj("z1"))]
    );
}

#[test]
fn the_witness_is_the_earliest_case_in_plan_order_not_state_order() {
    let p = chained();
    let inv = &p.invariants[0];
    // Two failing cases; the state holds x1 first.
    let state = State::from_claims(vec![
        a("x1", "y1"),
        b("y1", "z1"),
        a("x2", "y2"),
        b("y2", "z2"),
    ]);
    let x1 = case(&[("x", subj("x1")), ("y", subj("y1"))]);
    let x2 = case(&[("x", subj("x2")), ("y", subj("y2"))]);
    let first = |cases: &[BTreeMap<Var, EvalValue>]| {
        let d = invariant_witness_cases(inv, &state, None, &[], cases).unwrap();
        d.witness
            .iter()
            .find(|w| w.var.as_str() == "x")
            .map(|w| w.value.clone())
            .unwrap()
    };
    assert_eq!(first(&[x1.clone(), x2.clone()]), subj("x1"));
    assert_eq!(first(&[x2, x1]), subj("x2"));
    // The plan lists cases in the delta's order, admitted claims first.
    let cases = cases_of(inv, &[a("x2", "y2"), a("x1", "y1")]);
    assert_eq!(first(&cases), subj("x2"));
}

/// The guard on the work: with a hundred thousand rows the bounded
/// evaluation touches a handful of claims, on the verdict and on the
/// witness, where the whole evaluation touches them all.
#[test]
fn bounded_evaluation_visits_the_cases_not_the_state() {
    let p = rows();
    let inv = &p.invariants[0];
    let mut claims = Vec::with_capacity(200_000);
    for i in 0..100_000 {
        let k = format!("k{i}");
        claims.push(claim_instance("Row", &[subj(&k), dv(1)]));
        claims.push(claim_instance("Ok", &[subj(&k)]));
    }
    let pre = State::from_claims(claims);

    let whole_before = pre.claims_visited();
    assert_eq!(eval_invariant(inv, &pre, None, &[]), Ok(true));
    assert!(
        pre.claims_visited() - whole_before >= 100_000,
        "the whole evaluation walks the state"
    );

    // Admitted: a new row with its Ok.
    let delta = [
        claim_instance("Row", &[subj("new"), dv(1)]),
        claim_instance("Ok", &[subj("new")]),
    ];
    let post = pre.with_delta(&delta, &[]);
    let cases = cases_of(inv, &delta);
    let before = post.claims_visited();
    assert_eq!(
        eval_invariant_cases(inv, &post, None, &[], &cases),
        Ok(true)
    );
    let visited = post.claims_visited() - before;
    assert!(
        visited <= 16,
        "the bounded verdict visited {visited} claims"
    );

    // Refused: a new row without its Ok, witnessed from the case alone.
    let delta = [claim_instance("Row", &[subj("lone"), dv(1)])];
    let post = pre.with_delta(&delta, &[]);
    let cases = cases_of(inv, &delta);
    let before = post.claims_visited();
    assert_eq!(
        eval_invariant_cases(inv, &post, None, &[], &cases),
        Ok(false)
    );
    let witness = invariant_witness_cases(inv, &post, None, &[], &cases).unwrap();
    assert_eq!(witness.witness[0].value, subj("lone"));
    let visited = post.claims_visited() - before;
    assert!(
        visited <= 16,
        "the bounded refusal visited {visited} claims"
    );
}

/// `not (Row(k, v) and 0 <= 1 / v)` over a row that matches and a row
/// that errors: the error wins whichever case comes first, because every
/// touched case is evaluated before the rule answers.
#[test]
fn a_negated_rule_evaluates_every_touched_case_before_answering() {
    let p = program("neg")
        .predicates(vec![predicate("Row").subject("k").decimal("v").build()])
        .invariants(vec![invariant(
            "no_positive_rows",
            not(and(vec![
                claim("Row", vec![var("k"), var("v")]),
                le(term(dec("0")), div(term(dec("1")), term(var("v")))),
            ])),
        )])
        .build();
    let inv = &p.invariants[0];
    let state = State::from_claims(vec![
        claim_instance("Row", &[subj("a"), dv(1)]),
        claim_instance("Row", &[subj("b"), dv(0)]),
    ]);
    let a = case(&[("k", subj("a"))]);
    let b = case(&[("k", subj("b"))]);
    for cases in [vec![a.clone(), b.clone()], vec![b, a]] {
        assert!(
            matches!(
                eval_invariant_cases(inv, &state, None, &[], &cases),
                Err(EvalError::DivisionByZero)
            ),
            "{cases:?}"
        );
    }
}
