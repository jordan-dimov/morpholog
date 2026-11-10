//! The impact plan through defined calls. The planner may answer too
//! broadly, which costs work, but never too narrowly, which would change
//! what is admitted. Each fixture is checked exhaustively: every state over
//! a small claim universe, every single-claim admit or retract, every case.

use std::collections::BTreeMap;

use super::*;
use crate::derive::{eval_invariant, eval_invariant_cases};
use crate::impact::{Bounding, Widening};
use crate::ir::{Definition, DefinitionOrigin, Var};
use crate::ir_builder::{
    and, claim, defined, implies, invariant, params, subj as lit, term, var, wildcard,
};

fn subj(s: &str) -> EvalValue {
    EvalValue::Subject(s.into())
}

fn dec(n: i64) -> EvalValue {
    EvalValue::Decimal(Decimal::new(n, 0))
}

fn claim_instance(predicate: &str, args: &[EvalValue]) -> ClaimInstance {
    ClaimInstance {
        predicate: predicate.into(),
        args: args.to_vec(),
    }
}

fn def(name: &str, parameters: &[&str], body: Prop) -> Definition {
    Definition {
        name: name.into(),
        parameters: params(parameters),
        body: crate::ir::DefinitionBody::Prop(body),
        origin: DefinitionOrigin::default(),
    }
}

fn positive(v: &str) -> Prop {
    gt_(
        Box::new(term(var(v))),
        Box::new(term(crate::ir_builder::dec("0"))),
    )
}

fn covers(impact: &Impact, case: &BTreeMap<Var, EvalValue>) -> bool {
    match impact {
        Impact::Unbounded => true,
        Impact::Untouched => false,
        Impact::Bounded(cases) => cases
            .iter()
            .any(|c| c.iter().all(|(v, ev)| case.get(v) == Some(ev))),
    }
}

/// Every assignment of `vars` over `domain`.
fn assignments(vars: &[Var], domain: &[EvalValue]) -> Vec<BTreeMap<Var, EvalValue>> {
    let mut out = vec![BTreeMap::new()];
    for v in vars {
        out = out
            .into_iter()
            .flat_map(|case| {
                domain.iter().map(move |value| {
                    let mut case = case.clone();
                    case.insert(v.clone(), value.clone());
                    case
                })
            })
            .collect();
    }
    out
}

/// A case whose truth a single-claim change moves is always covered by
/// the plan's answer for that change. The cases range over the
/// antecedent's variable `x`, named here rather than taken from the plan,
/// so a plan that invents a case variable cannot shrink what it is judged
/// against.
fn assert_never_narrower(inv: &Invariant, defs: &[Definition], universe: &[ClaimInstance]) {
    assert_never_narrower_over(inv, defs, universe, &[Var::from("x")]);
}

/// [`assert_never_narrower`] over the case variables the fixture names.
fn assert_never_narrower_over(
    inv: &Invariant,
    defs: &[Definition],
    universe: &[ClaimInstance],
    vars: &[Var],
) {
    let plan = ImpactPlan::with_definitions(inv, defs);
    let bound = plan.bounding() == Bounding::Bound;
    let mut domain: Vec<EvalValue> = Vec::new();
    for value in universe.iter().flat_map(|c| &c.args) {
        if !domain.contains(value) {
            domain.push(value.clone());
        }
    }
    let cases = assignments(vars, &domain);
    let truth = |state: &State, case: &BTreeMap<Var, EvalValue>| {
        format!(
            "{:?}",
            eval_invariant_cases(inv, state, None, defs, std::slice::from_ref(case))
        )
    };
    for mask in 0..(1u32 << universe.len()) {
        let held = |i: usize| mask >> i & 1 == 1;
        let pre_claims: Vec<ClaimInstance> = (0..universe.len())
            .filter(|i| held(*i))
            .map(|i| universe[i].clone())
            .collect();
        let pre = State::from_claims(pre_claims.clone());
        for (i, changed) in universe.iter().enumerate() {
            let (post, impact) = if held(i) {
                let rest = pre_claims
                    .iter()
                    .filter(|c| *c != changed)
                    .cloned()
                    .collect();
                (
                    State::from_claims(rest),
                    plan.classify(&[], std::slice::from_ref(changed)),
                )
            } else {
                let mut more = pre_claims.clone();
                more.push(changed.clone());
                (
                    State::from_claims(more),
                    plan.classify(std::slice::from_ref(changed), &[]),
                )
            };
            assert!(
                !(bound && impact == Impact::Unbounded),
                "{}: the plan says bound, but {changed:?} checks it whole",
                inv.name
            );
            // The whole rule, before and after: a delta that breaks it must
            // be caught by the check admission runs, whatever the plan's
            // cases say and whatever evaluator confirms them.
            if eval_invariant(inv, &pre, None, defs) == Ok(true)
                && eval_invariant(inv, &post, None, defs) == Ok(false)
            {
                let caught = match &impact {
                    Impact::Untouched => false,
                    Impact::Unbounded => true,
                    Impact::Bounded(cases) => {
                        eval_invariant_cases(inv, &post, None, defs, cases) == Ok(false)
                    }
                };
                assert!(
                    caught,
                    "{}: {} {changed:?} over {pre_claims:?} breaks the whole rule, but the \
                     plan answered {impact:?} and the bounded check passed",
                    inv.name,
                    if held(i) { "retracting" } else { "admitting" },
                );
            }
            for case in &cases {
                if truth(&pre, case) != truth(&post, case) {
                    assert!(
                        covers(&impact, case),
                        "{}: {} {changed:?} over {pre_claims:?} changes case {case:?}, \
                         but the plan answered {impact:?}",
                        inv.name,
                        if held(i) { "retracting" } else { "admitting" },
                    );
                }
            }
        }
    }
}

fn case(pairs: &[(&str, EvalValue)]) -> BTreeMap<Var, EvalValue> {
    pairs
        .iter()
        .map(|(v, ev)| ((*v).into(), ev.clone()))
        .collect()
}

fn admitted(plan: &ImpactPlan, c: ClaimInstance) -> Impact {
    plan.classify(&[c], &[])
}

/// A case variable renamed through two definitions reaches the inner
/// body, and a literal argument becomes a guard there.
#[test]
fn a_case_variable_composes_through_nested_calls_and_a_literal_guards() {
    let defs = [
        def(
            "inner",
            &["w", "tag"],
            and(vec![
                claim("Holder", vec![var("w"), var("tag"), var("n")]),
                positive("n"),
            ]),
        ),
        def("outer", &["u"], defined("inner", vec![var("u"), lit("t")])),
    ];
    let inv = invariant(
        "outer_holds",
        implies(claim("A", vec![var("x")]), defined("outer", vec![var("x")])),
    );
    let holder =
        |w: &str, tag: &str, n: i64| claim_instance("Holder", &[subj(w), subj(tag), dec(n)]);
    let universe = [
        claim_instance("A", &[subj("a")]),
        claim_instance("A", &[subj("b")]),
        holder("a", "t", -1),
        holder("a", "t", 1),
        holder("b", "t", 1),
        holder("a", "s", 1),
    ];
    assert_never_narrower(&inv, &defs, &universe);
    let plan = ImpactPlan::with_definitions(&inv, &defs);
    assert_eq!(
        admitted(&plan, holder("a", "t", 1)),
        Impact::Bounded(vec![case(&[("x", subj("a"))])])
    );
    assert_eq!(admitted(&plan, holder("a", "s", 1)), Impact::Untouched);
    assert_eq!(
        ImpactPlan::new(&inv).classify(&[holder("a", "t", 1)], &[]),
        Impact::Unbounded,
        "without the definitions the call widens"
    );
}

/// One case variable passed twice must agree with itself: a claim whose
/// two positions differ can reach no case.
#[test]
fn a_repeated_argument_binds_both_parameters_to_one_case() {
    let defs = [def(
        "same",
        &["p", "q"],
        claim("Pair", vec![var("p"), var("q")]),
    )];
    let inv = invariant(
        "paired_with_itself",
        implies(
            claim("A", vec![var("x")]),
            defined("same", vec![var("x"), var("x")]),
        ),
    );
    let pair = |p: &str, q: &str| claim_instance("Pair", &[subj(p), subj(q)]);
    let universe = [
        claim_instance("A", &[subj("a")]),
        claim_instance("A", &[subj("b")]),
        pair("a", "a"),
        pair("a", "b"),
        pair("b", "a"),
        pair("b", "b"),
    ];
    assert_never_narrower(&inv, &defs, &universe);
    let plan = ImpactPlan::with_definitions(&inv, &defs);
    assert_eq!(
        admitted(&plan, pair("a", "a")),
        Impact::Bounded(vec![case(&[("x", subj("a"))])])
    );
    assert_eq!(admitted(&plan, pair("a", "b")), Impact::Bounded(vec![]));
}

/// A wildcard argument or a variable that is not a case variable leaves
/// its parameter tracing to nothing.
#[test]
fn a_wildcard_or_non_case_argument_never_becomes_a_case_variable() {
    let defs = [def(
        "link",
        &["p", "q"],
        claim("Link", vec![var("p"), var("q")]),
    )];
    let link = |p: &str, q: &str| claim_instance("Link", &[subj(p), subj(q)]);
    let universe = [
        claim_instance("A", &[subj("a")]),
        claim_instance("A", &[subj("b")]),
        link("a", "a"),
        link("a", "b"),
        link("b", "a"),
        link("b", "b"),
    ];
    let wildcard_second = invariant(
        "linked_somewhere",
        implies(
            claim("A", vec![var("x")]),
            defined("link", vec![var("x"), wildcard()]),
        ),
    );
    assert_never_narrower(&wildcard_second, &defs, &universe);
    assert_eq!(
        admitted(
            &ImpactPlan::with_definitions(&wildcard_second, &defs),
            link("a", "b")
        ),
        Impact::Bounded(vec![case(&[("x", subj("a"))])])
    );
    let free_first = invariant(
        "linked_from_somewhere",
        implies(
            claim("A", vec![var("x")]),
            defined("link", vec![var("y"), var("x")]),
        ),
    );
    assert_never_narrower(&free_first, &defs, &universe);
    assert_eq!(
        admitted(
            &ImpactPlan::with_definitions(&free_first, &defs),
            link("a", "b")
        ),
        Impact::Bounded(vec![case(&[("x", subj("b"))])])
    );
}

/// A body's own variable spelled like the caller's case variable is not
/// that variable: an occurrence reached only through it widens.
#[test]
fn a_body_variable_spelled_like_a_case_variable_traces_to_nothing() {
    let defs = [def(
        "has_route",
        &["p"],
        and(vec![
            claim("Route", vec![var("x"), var("p")]),
            claim("Ok", vec![var("x")]),
        ]),
    )];
    let inv = invariant(
        "routed_through_an_ok_hop",
        implies(
            claim("A", vec![var("x")]),
            defined("has_route", vec![var("x")]),
        ),
    );
    let route = |x: &str, p: &str| claim_instance("Route", &[subj(x), subj(p)]);
    let universe = [
        claim_instance("A", &[subj("a")]),
        claim_instance("A", &[subj("b")]),
        route("a", "a"),
        route("a", "b"),
        route("b", "a"),
        claim_instance("Ok", &[subj("a")]),
        claim_instance("Ok", &[subj("b")]),
    ];
    assert_never_narrower(&inv, &defs, &universe);
    let plan = ImpactPlan::with_definitions(&inv, &defs);
    assert_eq!(
        admitted(&plan, route("a", "b")),
        Impact::Bounded(vec![case(&[("x", subj("b"))])])
    );
    assert_eq!(
        admitted(&plan, claim_instance("Ok", &[subj("a")])),
        Impact::Unbounded
    );
}

/// A call in the antecedent carries the case variable whether it comes
/// before or after the claim that binds it.
#[test]
fn an_antecedent_call_carries_the_case_variable_in_either_order() {
    let defs = [def(
        "tagged",
        &["p"],
        claim("Tag", vec![var("p"), var("k")]),
    )];
    let universe = [
        claim_instance("A", &[subj("a")]),
        claim_instance("A", &[subj("b")]),
        claim_instance("Tag", &[subj("a"), subj("t")]),
        claim_instance("Tag", &[subj("b"), subj("t")]),
        claim_instance("C", &[subj("a")]),
        claim_instance("C", &[subj("b")]),
    ];
    for (name, antecedent) in [
        (
            "claim_then_call",
            and(vec![
                claim("A", vec![var("x")]),
                defined("tagged", vec![var("x")]),
            ]),
        ),
        (
            "call_then_claim",
            and(vec![
                defined("tagged", vec![var("x")]),
                claim("A", vec![var("x")]),
            ]),
        ),
    ] {
        let inv = invariant(name, implies(antecedent, claim("C", vec![var("x")])));
        assert_never_narrower(&inv, &defs, &universe);
        assert_eq!(
            admitted(
                &ImpactPlan::with_definitions(&inv, &defs),
                claim_instance("Tag", &[subj("a"), subj("t")])
            ),
            Impact::Bounded(vec![case(&[("x", subj("a"))])]),
            "{name}"
        );
    }
}

#[test]
fn arithmetic_over_case_bound_terms_keeps_the_case_bounded() {
    use crate::ir_builder::{dec as dec_term, le, mul, sub};
    // Entry(x, price, stop) implies (price - stop) * 100 <= 15 * price
    let inv = invariant(
        "risk_within_bound",
        implies(
            claim("Entry", vec![var("x"), var("price"), var("stop")]),
            le(
                mul(
                    sub(term(var("price")), term(var("stop"))),
                    term(dec_term("100")),
                ),
                mul(term(dec_term("15")), term(var("price"))),
            ),
        ),
    );
    let universe = [
        claim_instance("Entry", &[subj("a"), dec(10), dec(9)]),
        claim_instance("Entry", &[subj("b"), dec(10), dec(1)]),
        claim_instance("Exit", &[subj("a")]),
    ];
    assert_never_narrower_over(
        &inv,
        &[],
        &universe,
        &[Var::from("x"), Var::from("price"), Var::from("stop")],
    );
    let plan = ImpactPlan::with_definitions(&inv, &[]);
    assert_eq!(
        admitted(&plan, universe[1].clone()),
        Impact::Bounded(vec![case(&[
            ("price", dec(10)),
            ("stop", dec(1)),
            ("x", subj("b"))
        ])]),
        "the arithmetic reads only what the pattern bound"
    );
    assert_eq!(
        admitted(&plan, universe[2].clone()),
        Impact::Untouched,
        "an exit is not an entry"
    );
}

#[test]
fn a_delta_outside_the_footprint_touches_nothing_even_when_the_body_is_checked_whole() {
    use crate::ir_builder::or;
    let inv = invariant(
        "either_way",
        implies(
            claim("A", vec![var("x")]),
            or(vec![claim("B", vec![var("x")]), claim("C", vec![var("x")])]),
        ),
    );
    let universe = [
        claim_instance("A", &[subj("a")]),
        claim_instance("B", &[subj("a")]),
        claim_instance("C", &[subj("a")]),
        claim_instance("D", &[subj("a")]),
    ];
    assert_never_narrower(&inv, &[], &universe);
    for plan in [
        ImpactPlan::new(&inv),
        ImpactPlan::with_definitions(&inv, &[]),
    ] {
        // A consequent `or` is a truth test over the case's claims: a
        // change to a branch's pattern touches that case alone.
        assert_eq!(plan.bounding(), Bounding::Bound);
        assert_eq!(
            admitted(&plan, universe[1].clone()),
            Impact::Bounded(vec![case(&[("x", subj("a"))])])
        );
        assert_eq!(
            admitted(&plan, universe[3].clone()),
            Impact::Untouched,
            "`D` is read nowhere in the body"
        );
    }
}

/// An `or` in the antecedent takes part in which cases exist, so it
/// still widens; a consequent `or` whose branch binds no case variable
/// is whole on that pattern's touch and bounded on the others; and the
/// consequent `or` stays bounded through a definition the plan follows.
#[test]
fn an_or_widens_only_where_it_decides_the_cases() {
    use crate::ir_builder::or;
    let in_antecedent = invariant(
        "either_way_in",
        implies(
            or(vec![claim("A", vec![var("x")]), claim("B", vec![var("x")])]),
            claim("C", vec![var("x")]),
        ),
    );
    let universe = [
        claim_instance("A", &[subj("a")]),
        claim_instance("B", &[subj("a")]),
        claim_instance("C", &[subj("a")]),
    ];
    assert_never_narrower(&in_antecedent, &[], &universe);
    for plan in [
        ImpactPlan::new(&in_antecedent),
        ImpactPlan::with_definitions(&in_antecedent, &[]),
    ] {
        assert_eq!(plan.bounding(), Bounding::Whole(Widening::Or));
        assert_eq!(admitted(&plan, universe[0].clone()), Impact::Unbounded);
    }

    let free_branch = invariant(
        "marked_or_open",
        implies(
            claim("A", vec![var("x")]),
            or(vec![
                claim("B", vec![var("x")]),
                claim("Open", vec![var("o")]),
            ]),
        ),
    );
    let universe = [
        claim_instance("A", &[subj("a")]),
        claim_instance("B", &[subj("a")]),
        claim_instance("Open", &[subj("o")]),
    ];
    assert_never_narrower(&free_branch, &[], &universe);
    let plan = ImpactPlan::with_definitions(&free_branch, &[]);
    assert_eq!(
        plan.bounding(),
        Bounding::Whole(Widening::NoCaseVariable("Open".into()))
    );
    assert_eq!(
        admitted(&plan, universe[1].clone()),
        Impact::Bounded(vec![case(&[("x", subj("a"))])])
    );
    assert_eq!(admitted(&plan, universe[2].clone()), Impact::Unbounded);

    let defs = [def(
        "marked",
        &["p"],
        or(vec![claim("B", vec![var("p")]), claim("C", vec![var("p")])]),
    )];
    let through_a_call = invariant(
        "either_way_called",
        implies(
            claim("A", vec![var("x")]),
            defined("marked", vec![var("x")]),
        ),
    );
    let universe = [
        claim_instance("A", &[subj("a")]),
        claim_instance("B", &[subj("a")]),
        claim_instance("C", &[subj("a")]),
    ];
    assert_never_narrower(&through_a_call, &defs, &universe);
    let plan = ImpactPlan::with_definitions(&through_a_call, &defs);
    assert_eq!(plan.bounding(), Bounding::Bound);
    assert_eq!(
        admitted(&plan, universe[2].clone()),
        Impact::Bounded(vec![case(&[("x", subj("a"))])])
    );
    let called_in_antecedent = invariant(
        "either_way_called_in",
        implies(
            defined("marked", vec![var("x")]),
            claim("A", vec![var("x")]),
        ),
    );
    assert_never_narrower(&called_in_antecedent, &defs, &universe);
    assert_eq!(
        ImpactPlan::with_definitions(&called_in_antecedent, &defs).bounding(),
        Bounding::Whole(Widening::Or)
    );
}

/// A delta of several claims across two cases, an admit and a retract,
/// is the union of the cases each touches.
#[test]
fn a_delta_over_two_cases_is_bounded_to_both() {
    use crate::ir_builder::or;
    let inv = invariant(
        "either_way",
        implies(
            claim("A", vec![var("x")]),
            or(vec![claim("B", vec![var("x")]), claim("C", vec![var("x")])]),
        ),
    );
    let plan = ImpactPlan::with_definitions(&inv, &[]);
    let impact = plan.classify(
        &[claim_instance("B", &[subj("a")])],
        &[claim_instance("C", &[subj("b")])],
    );
    assert_eq!(
        impact,
        Impact::Bounded(vec![case(&[("x", subj("a"))]), case(&[("x", subj("b"))])])
    );
}

#[test]
fn a_value_lookup_is_part_of_the_footprint() {
    use crate::ir_builder::{le, value_of};
    // A(x, v) implies v <= value Limit(l, _)
    let inv = invariant(
        "under_the_limit",
        implies(
            claim("A", vec![var("x"), var("v")]),
            le(
                term(var("v")),
                value_of("Limit", vec![lit("l"), wildcard()]),
            ),
        ),
    );
    let universe = [
        claim_instance("A", &[subj("a"), dec(1)]),
        claim_instance("Limit", &[subj("l"), dec(5)]),
        claim_instance("D", &[subj("a")]),
    ];
    assert_never_narrower(&inv, &[], &universe);
    let plan = ImpactPlan::with_definitions(&inv, &[]);
    assert_eq!(
        admitted(&plan, universe[1].clone()),
        Impact::Unbounded,
        "the lookup reads `Limit`, which the walk hands as no claim pattern"
    );
    assert_eq!(admitted(&plan, universe[2].clone()), Impact::Untouched);
}

#[test]
fn an_unfollowed_call_leaves_the_footprint_unknown() {
    let defs = [def(
        "tagged",
        &["p"],
        claim("Tag", vec![var("p"), var("k")]),
    )];
    let inv = invariant(
        "tagged_things",
        implies(
            claim("A", vec![var("x")]),
            defined("tagged", vec![var("x")]),
        ),
    );
    let outside = claim_instance("D", &[subj("a")]);
    assert_eq!(
        admitted(&ImpactPlan::new(&inv), outside.clone()),
        Impact::Unbounded,
        "without the definitions, what the call reads is unknown"
    );
    assert_eq!(
        admitted(&ImpactPlan::with_definitions(&inv, &defs), outside),
        Impact::Untouched
    );
}

#[test]
fn a_rule_over_pre_is_never_dismissed_by_a_delta_outside_its_reads() {
    use crate::ir_builder::pre;
    // Something moved, and the count did not: the rule reads only `Count`.
    let inv = invariant(
        "count_rises",
        implies(
            pre(claim("Count", vec![var("n")])),
            claim("Count", vec![var("n")]),
        ),
    );
    let outside = claim_instance("Piece", &[subj("a")]);
    for plan in [
        ImpactPlan::new(&inv),
        ImpactPlan::with_definitions(&inv, &[]),
    ] {
        assert_eq!(admitted(&plan, outside.clone()), Impact::Unbounded);
        assert_eq!(
            plan.classify(&[], &[]),
            Impact::Untouched,
            "nothing changed"
        );
    }
}

#[test]
fn an_extremum_over_a_domain_is_bounded_to_the_case_its_body_binds() {
    use crate::ir::{ExtremumOp, Term, ValueExpr};
    use crate::ir_builder::le;
    // AccountLimit(account, limit) implies max(v | Observation(account, v)) <= limit
    let inv = invariant(
        "peak_within_limit",
        implies(
            claim("AccountLimit", vec![var("account"), var("limit")]),
            le(
                ValueExpr::Extremum {
                    op: ExtremumOp::Max,
                    value: Term::Var(Var::from("v")),
                    body: Box::new(claim("Observation", vec![var("account"), var("v")])),
                },
                term(var("limit")),
            ),
        ),
    );
    // Account `a` already breaches; `b` is clean.
    let universe = [
        claim_instance("AccountLimit", &[subj("a"), dec(5)]),
        claim_instance("AccountLimit", &[subj("b"), dec(5)]),
        claim_instance("Observation", &[subj("a"), dec(3)]),
        claim_instance("Observation", &[subj("a"), dec(9)]),
        claim_instance("Observation", &[subj("b"), dec(2)]),
        claim_instance("Observation", &[subj("b"), dec(7)]),
    ];
    assert_never_narrower_over(
        &inv,
        &[],
        &universe,
        &[Var::from("account"), Var::from("limit")],
    );
    let plan = ImpactPlan::with_definitions(&inv, &[]);
    let bounded_to_b = Impact::Bounded(vec![case(&[("account", subj("b"))])]);
    assert_eq!(
        admitted(&plan, universe[5].clone()),
        bounded_to_b,
        "an observation on `b` reaches `b`'s peak only"
    );
    assert_eq!(
        plan.classify(&[], std::slice::from_ref(&universe[4])),
        bounded_to_b,
        "retracting one of `b`'s observations likewise"
    );
    assert_eq!(
        admitted(&plan, claim_instance("Deposit", &[subj("b")])),
        Impact::Untouched
    );
}

#[test]
fn a_lookup_inside_a_builtin_still_widens() {
    use crate::ir_builder::{abs, le, value_of};
    // A(x, v) implies abs(value Limit(l, _)) <= v
    let inv = invariant(
        "within_the_absolute_limit",
        implies(
            claim("A", vec![var("x"), var("v")]),
            le(
                abs(value_of("Limit", vec![lit("l"), wildcard()])),
                term(var("v")),
            ),
        ),
    );
    let universe = [
        claim_instance("A", &[subj("a"), dec(1)]),
        claim_instance("Limit", &[subj("l"), dec(5)]),
        claim_instance("D", &[subj("a")]),
    ];
    assert_never_narrower(&inv, &[], &universe);
    let plan = ImpactPlan::with_definitions(&inv, &[]);
    assert_eq!(admitted(&plan, universe[1].clone()), Impact::Unbounded);
    assert_eq!(admitted(&plan, universe[2].clone()), Impact::Untouched);
}

/// Each construct the bounding proof does not cover is named as the
/// source spells it, and the plan's verdict is derived from what
/// `classify` consults.
#[test]
fn bounding_names_the_construct_that_checks_a_rule_whole() {
    use crate::ir_builder::{add, cond, in_, le, or, pre, sum, value_of, xor};
    let a = || claim("A", vec![var("x"), var("v")]);
    let b = || claim("B", vec![var("x")]);
    let v = || term(var("v"));
    let shapes: Vec<(Prop, Widening)> = vec![
        (implies(pre(b()), b()), Widening::Pre),
        (
            implies(or(vec![a(), claim("C", vec![var("x")])]), b()),
            Widening::Or,
        ),
        (
            implies(a(), xor(b(), claim("C", vec![var("x")]))),
            Widening::Xor,
        ),
        (implies(a(), in_(var("x"), var("v"))), Widening::In),
        (
            implies(a(), le(value_of("Limit", vec![lit("l"), wildcard()]), v())),
            Widening::ValueLookup,
        ),
        (
            implies(a(), le(cond(b(), v(), v()), v())),
            Widening::ConditionalValue,
        ),
        (
            implies(a(), le(sum(add(v(), v()), b()), v())),
            Widening::SumOverExpression,
        ),
    ];
    for (body, expected) in shapes {
        let inv = invariant("shape", body);
        for plan in [
            ImpactPlan::new(&inv),
            ImpactPlan::with_definitions(&inv, &[]),
        ] {
            assert_eq!(plan.bounding(), Bounding::Whole(expected.clone()));
        }
    }
    let plain = invariant("plain", implies(a(), b()));
    assert_eq!(ImpactPlan::new(&plain).bounding(), Bounding::Bound);
    assert_eq!(
        Widening::SumOverExpression.to_string(),
        "a sum over an expression"
    );
}

/// A claim pattern binding no case variable makes the rule whole for a
/// delta touching that pattern only; its neighbours stay bounded, and the
/// verdict says whole while naming the pattern.
#[test]
fn a_pattern_without_a_case_variable_reports_whole_and_widens_only_its_own_deltas() {
    let inv = invariant(
        "while_open",
        implies(
            and(vec![
                claim("A", vec![var("x")]),
                claim("Open", vec![wildcard()]),
            ]),
            claim("B", vec![var("x")]),
        ),
    );
    let universe = [
        claim_instance("A", &[subj("a")]),
        claim_instance("Open", &[subj("o")]),
        claim_instance("B", &[subj("a")]),
    ];
    assert_never_narrower(&inv, &[], &universe);
    for plan in [
        ImpactPlan::new(&inv),
        ImpactPlan::with_definitions(&inv, &[]),
    ] {
        assert_eq!(
            plan.bounding(),
            Bounding::Whole(Widening::NoCaseVariable("Open".into()))
        );
        assert_eq!(
            admitted(&plan, universe[0].clone()),
            Impact::Bounded(vec![case(&[("x", subj("a"))])])
        );
        assert_eq!(admitted(&plan, universe[1].clone()), Impact::Unbounded);
    }
}

/// A call the plan follows bounds what the bare plan checks whole, and
/// the bare plan names the call.
#[test]
fn a_followed_call_is_bound_where_the_bare_plan_names_the_call() {
    let defs = [def("has", &["p"], claim("Tag", vec![var("p"), var("k")]))];
    let inv = invariant(
        "called",
        implies(
            and(vec![
                claim("A", vec![var("x")]),
                defined("has", vec![var("x")]),
            ]),
            claim("B", vec![var("x")]),
        ),
    );
    assert_eq!(
        ImpactPlan::new(&inv).bounding(),
        Bounding::Whole(Widening::UnfollowedCall("has".into()))
    );
    assert_eq!(
        ImpactPlan::with_definitions(&inv, &defs).bounding(),
        Bounding::Bound
    );
    assert_eq!(
        ImpactPlan::with_definitions(&inv, &[]).bounding(),
        Bounding::Whole(Widening::UnfollowedCall("has".into())),
        "an undeclared call cannot be followed"
    );
}

/// A closed scope that reads a case name before the rule binds it is a
/// fresh binding of that name, not the case, so the plan checks such a
/// rule whole; once the binding comes first the plan bounds as before.
#[test]
fn a_scope_reading_a_case_name_before_its_binding_widens() {
    use crate::ir_builder::{exists, not};
    let tag = |x: &str, t: &str| claim_instance("Tag", &[subj(x), subj(t)]);
    let universe = [
        tag("old", "t"),
        claim_instance("Row", &[subj("new")]),
        claim_instance("Ok", &[subj("new")]),
    ];
    let rule = |name: &str, antecedent: Vec<Prop>| {
        invariant(name, implies(and(antecedent), claim("Ok", vec![var("x")])))
    };
    let widened = |inv: &Invariant| {
        matches!(
            ImpactPlan::new(inv).bounding(),
            Bounding::Whole(Widening::ScopeReadsBeforeBinding(v)) if v.as_str() == "x"
        )
    };

    // `(exists y: Tag(x, y)) and Row(x)`: the existential is global, so a
    // tag anywhere changes every row's obligation.
    let early = rule(
        "early",
        vec![
            exists("y", claim("Tag", vec![var("x"), var("y")])),
            claim("Row", vec![var("x")]),
        ],
    );
    assert!(widened(&early), "{:?}", ImpactPlan::new(&early).bounding());
    assert_eq!(
        ImpactPlan::new(&early).classify(&[tag("old", "t")], &[]),
        Impact::Unbounded
    );
    assert_never_narrower(&early, &[], &universe);

    // `Row(x) and (exists y: Tag(x, y))`: the scope reads the row's x.
    let late = rule(
        "late",
        vec![
            claim("Row", vec![var("x")]),
            exists("y", claim("Tag", vec![var("x"), var("y")])),
        ],
    );
    assert_eq!(ImpactPlan::new(&late).bounding(), Bounding::Bound);
    assert_never_narrower(&late, &[], &universe);

    // A negation before the binding, and a binder that reuses the case
    // name, are fresh bindings the same way.
    let negated = rule(
        "negated",
        vec![
            not(claim("Tag", vec![var("x"), wildcard()])),
            claim("Row", vec![var("x")]),
        ],
    );
    assert!(widened(&negated));
    assert_never_narrower(&negated, &[], &universe);
    let rebound = rule(
        "rebound",
        vec![
            exists("x", claim("Tag", vec![var("x"), var("x")])),
            claim("Row", vec![var("x")]),
        ],
    );
    assert!(widened(&rebound));
}

/// What a large delta is bounded to, built one claim at a time: the
/// cases of each claim alone, in delta order, each once. A delta is
/// untouched only when each of its claims is, and checks the rule whole
/// when any of its claims does.
fn one_claim_at_a_time(
    plan: &ImpactPlan,
    asserted: &[ClaimInstance],
    retracted: &[ClaimInstance],
) -> Impact {
    let mut cases: Vec<BTreeMap<Var, EvalValue>> = Vec::new();
    let mut touched = false;
    for claim in asserted.iter().chain(retracted) {
        match admitted(plan, claim.clone()) {
            Impact::Untouched => {}
            Impact::Unbounded => return Impact::Unbounded,
            Impact::Bounded(found) => {
                touched = true;
                for case in found {
                    if !cases.contains(&case) {
                        cases.push(case);
                    }
                }
            }
        }
    }
    if touched {
        Impact::Bounded(cases)
    } else {
        Impact::Untouched
    }
}

proptest::proptest! {
    /// A delta of many claims names the cases its claims name one at a
    /// time, for patterns that bind the case variables in two orders, a
    /// repeated variable that can contradict itself, a literal guard, a
    /// pattern binding no case variable, and a body checked whole. The
    /// claims come from a small universe, so cases repeat, and the
    /// distinct cases of a large delta cross the point where a set takes
    /// over.
    #[test]
    fn a_delta_names_the_cases_its_claims_name(
        asserted in proptest::collection::vec((0..3u8, 0..8u8, 0..8u8), 0..160),
        retracted in proptest::collection::vec((0..3u8, 0..8u8, 0..8u8), 0..40),
        shape in 0..5usize,
    ) {
        let s = |i: u8| subj(&format!("p{i}"));
        let to_claims = |rows: &[(u8, u8, u8)]| -> Vec<ClaimInstance> {
            rows.iter()
                .map(|&(p, a, b)| match p {
                    0 => claim_instance("R", &[s(a), s(b)]),
                    1 => claim_instance("S", &[s(a)]),
                    _ => claim_instance("U", &[s(a)]),
                })
                .collect()
        };
        let consequent = claim("S", vec![var("x")]);
        let body = match shape {
            0 => implies(
                and(vec![
                    claim("R", vec![var("x"), var("y")]),
                    claim("R", vec![var("y"), var("x")]),
                ]),
                consequent,
            ),
            1 => implies(claim("R", vec![var("x"), var("x")]), consequent),
            2 => implies(claim("R", vec![var("x"), lit("p1")]), consequent),
            3 => implies(
                claim("R", vec![var("x"), var("y")]),
                claim("S", vec![wildcard()]),
            ),
            _ => implies(
                crate::ir_builder::or(vec![
                    claim("R", vec![var("x"), var("y")]),
                    claim("S", vec![var("x")]),
                ]),
                claim("S", vec![var("y")]),
            ),
        };
        let plan = ImpactPlan::with_definitions(&invariant("rule", body), &[]);
        let (asserted, retracted) = (to_claims(&asserted), to_claims(&retracted));
        proptest::prop_assert_eq!(
            plan.classify(&asserted, &retracted),
            one_claim_at_a_time(&plan, &asserted, &retracted)
        );
    }
}
