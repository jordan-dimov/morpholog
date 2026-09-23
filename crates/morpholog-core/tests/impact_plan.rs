//! The impact plan: which cases of an invariant a delta can affect,
//! decided once in core. Pinned here because both the kernel and the
//! PostgreSQL compiler act on these answers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use morpholog_core::ir_builder::{
    add, and, claim, dec as dec_term, defined, eq, implies, invariant, not, subj as lit_subject,
    sum, term, value_of, var, wildcard,
};
use morpholog_core::{EvalValue, Impact, ImpactPlan, Invariant};
use morpholog_test_support::{claim_instance, dec, subj};

fn ledger_balance() -> Invariant {
    // JournalEntry(entry, _, _) implies sum(d | Line(entry, _, d, _)) = sum(c | Line(entry, _, _, c))
    invariant(
        "balanced",
        implies(
            claim("JournalEntry", vec![var("entry"), wildcard(), wildcard()]),
            eq(
                sum(
                    term(var("d")),
                    claim("Line", vec![var("entry"), wildcard(), var("d"), wildcard()]),
                ),
                sum(
                    term(var("c")),
                    claim("Line", vec![var("entry"), wildcard(), wildcard(), var("c")]),
                ),
            ),
        ),
    )
}

fn bound(pairs: &[(&str, EvalValue)]) -> BTreeMap<morpholog_core::Var, EvalValue> {
    pairs
        .iter()
        .map(|(v, ev)| ((*v).into(), ev.clone()))
        .collect()
}

#[test]
fn a_posting_bounds_the_balance_check_to_its_entry() {
    let plan = ImpactPlan::new(&ledger_balance());
    let delta = vec![
        claim_instance("JournalEntry", &[subj("e42"), subj("d1"), subj("p1")]),
        claim_instance("Line", &[subj("e42"), subj("cash"), dec(100), dec(0)]),
        claim_instance("Line", &[subj("e42"), subj("rev"), dec(0), dec(100)]),
    ];
    assert_eq!(
        plan.classify(&delta, &[]),
        Impact::Bounded(vec![bound(&[("entry", subj("e42"))])])
    );
    // A retraction touches the same case.
    let retracted = vec![claim_instance(
        "Line",
        &[subj("e7"), subj("cash"), dec(5), dec(0)],
    )];
    assert_eq!(
        plan.classify(&[], &retracted),
        Impact::Bounded(vec![bound(&[("entry", subj("e7"))])])
    );
    // Two entries in one delta, two cases.
    let two = vec![
        claim_instance("Line", &[subj("e1"), subj("cash"), dec(1), dec(0)]),
        claim_instance("Line", &[subj("e2"), subj("cash"), dec(1), dec(0)]),
    ];
    assert_eq!(
        plan.classify(&two, &[]),
        Impact::Bounded(vec![
            bound(&[("entry", subj("e1"))]),
            bound(&[("entry", subj("e2"))])
        ])
    );
}

#[test]
fn a_delta_of_other_predicates_is_untouched() {
    let plan = ImpactPlan::new(&ledger_balance());
    let delta = vec![claim_instance("PeriodClosed", &[subj("p1")])];
    assert_eq!(plan.classify(&delta, &[]), Impact::Untouched);
    assert_eq!(plan.classify(&[], &[]), Impact::Untouched);
}

#[test]
fn a_literal_guard_excludes_the_deltas_it_mismatches() {
    // Flag(x, #hot) implies Cooled(x); a #cold flag cannot touch it.
    let inv = invariant(
        "hot_flags_cool",
        implies(
            claim("Flag", vec![var("x"), lit_subject("hot")]),
            claim("Cooled", vec![var("x")]),
        ),
    );
    let plan = ImpactPlan::new(&inv);
    let cold = vec![claim_instance("Flag", &[subj("a"), subj("cold")])];
    assert_eq!(plan.classify(&cold, &[]), Impact::Untouched);
    let hot = vec![claim_instance("Flag", &[subj("a"), subj("hot")])];
    assert_eq!(
        plan.classify(&hot, &[]),
        Impact::Bounded(vec![bound(&[("x", subj("a"))])])
    );
}

#[test]
fn an_occurrence_binding_no_case_variable_widens_to_the_whole_invariant() {
    // not (Open(p) and Closed(p)) has case variable p; a Marker claim in
    // the body binds nothing the cases know, so touching it widens.
    let inv = invariant(
        "never_both",
        not(and(vec![
            claim("Open", vec![var("p")]),
            claim("Closed", vec![var("p")]),
            claim("Marker", vec![wildcard()]),
        ])),
    );
    let plan = ImpactPlan::new(&inv);
    assert_eq!(
        plan.classify(&[claim_instance("Open", &[subj("p1")])], &[]),
        Impact::Bounded(vec![bound(&[("p", subj("p1"))])])
    );
    assert_eq!(
        plan.classify(&[claim_instance("Marker", &[subj("m")])], &[]),
        Impact::Unbounded
    );
}

#[test]
fn a_body_outside_the_proven_shapes_widens_on_any_change() {
    let inv = invariant(
        "via_definition",
        implies(claim("A", vec![var("x")]), defined("holds", vec![var("x")])),
    );
    let plan = ImpactPlan::new(&inv);
    assert_eq!(
        plan.classify(&[claim_instance("Unrelated", &[subj("z")])], &[]),
        Impact::Unbounded
    );
    // An empty delta touches nothing, whatever the body holds.
    assert_eq!(plan.classify(&[], &[]), Impact::Untouched);
}

#[test]
fn a_value_lookup_is_a_state_dependency_the_plan_never_hides() {
    // A(x) implies value Q(x, _) = 1: a change to Q shows no claim
    // pattern, so the plan must widen rather than call it untouched.
    let inv = invariant(
        "looked_up",
        implies(
            claim("A", vec![var("x")]),
            eq(
                value_of("Q", vec![var("x"), wildcard()]),
                term(dec_term("1")),
            ),
        ),
    );
    let plan = ImpactPlan::new(&inv);
    assert_eq!(
        plan.classify(&[claim_instance("Q", &[subj("a"), dec(1)])], &[]),
        Impact::Unbounded
    );
    // Arithmetic in a comparison is likewise outside the proven shapes.
    let arith = invariant(
        "computed",
        implies(
            claim("A", vec![var("x")]),
            eq(
                add(term(var("x")), term(dec_term("1"))),
                term(dec_term("2")),
            ),
        ),
    );
    assert_eq!(
        ImpactPlan::new(&arith).classify(&[claim_instance("A", &[subj("a")])], &[]),
        Impact::Unbounded
    );
}
