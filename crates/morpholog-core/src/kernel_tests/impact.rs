//! The impact plan through defined calls. The planner may answer too
//! broadly, which costs work, but never too narrowly, which would change
//! what is admitted. Each fixture is checked exhaustively: every state over
//! a small claim universe, every single-claim admit or retract, every case.

use std::collections::BTreeMap;

use super::*;
use crate::derive::eval_invariant_cases;
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
        body,
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
        assert_eq!(admitted(&plan, universe[1].clone()), Impact::Unbounded);
        assert_eq!(
            admitted(&plan, universe[3].clone()),
            Impact::Untouched,
            "`D` is read nowhere in the body"
        );
    }
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
