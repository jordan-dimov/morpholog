//! A condition that calls itself, through the surface: what `check`
//! refuses by name, how a gate and a rule read it under each pattern of
//! bound arguments, and that nothing it reports depends on the order the
//! claims were admitted in.
//!
//! The evaluator is held to a reference in `morpholog-core`'s own tests.

#![expect(
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

mod common;

use common::{claim_instance, dec, must_accept, must_reject, subj};
use morpholog_core::{Program, RecursionRefusal, State, ValidationError};
use morpholog_surface::parse_program;
use morpholog_test_support::{enumerate_derived, eval_invariant, explain, fresh, test_transition};

const TREE: &str = r#"
program variant_tree

predicate PartParent(part: Subject, parent: Subject)
predicate Filled(line_part: Subject, item_part: Subject)
predicate Lineage(part: Subject, ancestor: Subject, links: Decimal)
predicate Counted(part: Subject)

define descends_from(part, ancestor):
    PartParent(part, ancestor)
    or (PartParent(part, parent) and descends_from(parent, ancestor))

-- A plain condition that reaches the recursive one.
define fits(item_part, line_part):
    item_part = line_part or descends_from(item_part, line_part)

derived Lineage(part, ancestor):
    over descends_from(part, ancestor)
    value links = 1

invariant no_part_descends_from_itself:
    PartParent(p, _) implies not descends_from(p, p)

invariant what_filled_a_line_still_fits:
    Filled(line_part, item_part) implies fits(item_part, line_part)

transformation link(part, parent):
    admit PartParent(part, parent)

transformation unlink(part, parent):
    retract PartParent(part, parent)

transformation fill(line_part, item_part):
    require same_part: fits(item_part, line_part)
    admit Filled(line_part, item_part)

transformation count_ancestors(part, expected):
    require ancestors: sum(1 | descends_from(part, a)) = expected
    admit Counted(part)

transformation count_descendants(part, expected):
    require descendants: sum(1 | descends_from(d, part)) = expected
    admit Counted(part)
"#;

fn tree() -> Program {
    let program = parse_program(TREE).expect("the tree programme parses");
    program.validate().expect("the tree programme validates");
    program
}

fn parent(part: &str, of: &str) -> morpholog_core::ClaimInstance {
    claim_instance("PartParent", &[subj(part), subj(of)])
}

/// root <- frame <- (red_frame <- red_frame_xl, blue_frame), and a
/// wheel that is nobody's variant.
fn links() -> Vec<morpholog_core::ClaimInstance> {
    vec![
        parent("frame", "root"),
        parent("red_frame", "frame"),
        parent("red_frame_xl", "red_frame"),
        parent("blue_frame", "frame"),
    ]
}

#[test]
fn a_gate_follows_the_tree_as_far_as_it_goes() {
    let p = tree();
    let fill = p.transformation("fill").unwrap();
    let state = State::from_claims(links());
    // The part itself, a child, and a great-grandchild.
    for (line, item) in [
        ("frame", "frame"),
        ("frame", "red_frame"),
        ("root", "red_frame_xl"),
    ] {
        must_accept(fill, vec![subj(line), subj(item)], state.clone(), &p);
    }
    // A sibling, an ancestor, and a part outside the tree.
    for (line, item) in [
        ("red_frame", "blue_frame"),
        ("red_frame", "frame"),
        ("frame", "wheel"),
    ] {
        let reason = must_reject(fill, vec![subj(line), subj(item)], &state, &p);
        common::refused_by(&reason, "same_part");
    }
}

#[test]
fn a_call_binds_whichever_argument_arrives_unbound() {
    let p = tree();
    let state = State::from_claims(links());
    let ancestors = p.transformation("count_ancestors").unwrap();
    let descendants = p.transformation("count_descendants").unwrap();
    for (part, up, down) in [
        ("red_frame_xl", 3, 0),
        ("frame", 1, 3),
        ("root", 0, 4),
        ("wheel", 0, 0),
    ] {
        must_accept(ancestors, vec![subj(part), dec(up)], state.clone(), &p);
        must_accept(descendants, vec![subj(part), dec(down)], state.clone(), &p);
    }
    // Neither bound: the whole relation, each pair once.
    let rows = enumerate_derived(&p, p.derived_claim("Lineage").unwrap(), &state).unwrap();
    assert_eq!(rows.len(), 8, "{rows:?}");
}

#[test]
fn one_variable_passed_twice_keeps_both_arguments_equal() {
    let p = tree();
    let rule = p.invariant("no_part_descends_from_itself").unwrap();
    let state = State::from_claims(links());
    assert!(eval_invariant(&p, rule, &state, None).unwrap());

    let mut looped = links();
    looped.push(parent("root", "red_frame_xl"));
    let state = State::from_claims(looped);
    assert!(!eval_invariant(&p, rule, &state, None).unwrap());
}

// A rule that reads a recursive condition is checked whole, with the
// call named, whether it calls the condition itself or through another.
#[test]
fn a_rule_over_a_recursive_condition_is_checked_whole() {
    use morpholog_core::{Bounding, ImpactPlan, Widening};
    let p = tree();
    for rule in [
        "no_part_descends_from_itself",
        "what_filled_a_line_still_fits",
    ] {
        let plan = ImpactPlan::with_definitions(p.invariant(rule).unwrap(), &p.definitions);
        assert_eq!(
            plan.bounding(),
            Bounding::Whole(Widening::RecursiveCall("descends_from".into())),
            "{rule}"
        );
    }
}

// A change two steps up the tree reaches a line filled at the bottom:
// a case bounded to the claim that changed would not see it.
#[test]
fn a_change_far_up_the_tree_reaches_the_rule() {
    let p = tree();
    let state = must_accept(
        p.transformation("fill").unwrap(),
        vec![subj("root"), subj("red_frame_xl")],
        State::from_claims(links()),
        &p,
    );
    let reason = must_reject(
        p.transformation("unlink").unwrap(),
        vec![subj("frame"), subj("root")],
        &state,
        &p,
    );
    common::refused_by(&reason, "what_filled_a_line_still_fits");
    // A link that closes a loop four steps round, and one onto itself.
    for (part, parent) in [("root", "red_frame_xl"), ("wheel", "wheel")] {
        let reason = must_reject(
            p.transformation("link").unwrap(),
            vec![subj(part), subj(parent)],
            &state,
            &p,
        );
        common::refused_by(&reason, "no_part_descends_from_itself");
    }
    must_accept(
        p.transformation("link").unwrap(),
        vec![subj("wheel"), subj("root")],
        state,
        &p,
    );
}

// An act that writes nothing the rules read does not run them: a tree
// that already holds a loop still takes a fill.
#[test]
fn an_act_outside_the_footprint_does_not_run_the_rule() {
    let p = tree();
    let mut looped = links();
    looped.push(parent("root", "red_frame_xl"));
    let state = State::from_claims(looped);
    must_accept(
        p.transformation("fill").unwrap(),
        vec![subj("red_frame"), subj("root")],
        state,
        &p,
    );
}

// The same claims in another order must give the same refusal, the same
// explanation and the same derived rows, byte for byte.
#[test]
fn nothing_reported_depends_on_admission_order() {
    let p = tree();
    let fill = p.transformation("fill").unwrap();
    let refused = vec![subj("red_frame"), subj("blue_frame")];
    let mut seen: Option<(String, String, String)> = None;
    let orders: [[usize; 4]; 4] = [[0, 1, 2, 3], [3, 2, 1, 0], [2, 0, 3, 1], [1, 3, 0, 2]];
    for order in orders {
        let claims = order.iter().map(|i| links()[*i].clone()).collect();
        let state = State::from_claims(claims);
        let reason = format!("{:?}", must_reject(fill, refused.clone(), &state, &p));
        let explanation = format!(
            "{:?}",
            explain(
                &p,
                &test_transition(fill, refused.clone()),
                &state,
                &mut fresh()
            )
        );
        let rows = format!(
            "{:?}",
            enumerate_derived(&p, p.derived_claim("Lineage").unwrap(), &state).unwrap()
        );
        let got = (reason, explanation, rows);
        match &seen {
            None => seen = Some(got),
            Some(first) => assert_eq!(first, &got, "order {order:?}"),
        }
    }
}

fn refusal(definitions: &str) -> RecursionRefusal {
    let source = format!(
        "program p\n\npredicate Seed(n: Decimal)\npredicate Base(x: Decimal, y: Decimal)\npredicate PartParent(part: Subject, parent: Subject)\n\n{definitions}\n"
    );
    let program = parse_program(&source).expect("the programme parses");
    let errors = program.validate().expect_err("the programme is refused");
    errors
        .iter()
        .find_map(|e| match e {
            ValidationError::RecursionNotBounded { refusal, .. } => Some(refusal.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no recursion refusal in {errors:?}"))
}

// Each of these would let a call to itself reach a value no admitted
// claim holds, so its answers, or the questions asked on the way, would
// have no end.
#[test]
fn a_recursion_that_could_make_new_values_is_refused_by_name() {
    assert_eq!(
        refusal("define counts_up(n):\n    Seed(n) or (counts_up(m) and n = m + 1)"),
        RecursionRefusal::ArgumentNotFromClaim {
            argument: "m".into()
        },
    );
    assert_eq!(
        refusal(
            "define next(m, n):\n    n = m + 1\n\ndefine counts_up(n):\n    Seed(n) or (Seed(m) and counts_up(m) and next(m, n))"
        ),
        RecursionRefusal::ParameterNotFromClaim {
            parameter: "n".into()
        },
    );
    assert_eq!(
        refusal("define r(x, y):\n    Base(x, y) or (Base(x, _) and k = x + 1 and r(k, y))"),
        RecursionRefusal::ArgumentNotFromClaim {
            argument: "k".into()
        },
    );
    assert_eq!(
        refusal(
            "define variant_of(part, ancestor):\n    part = ancestor or (PartParent(part, parent) and variant_of(parent, ancestor))"
        ),
        RecursionRefusal::ParameterNotFromClaim {
            parameter: "part".into()
        },
    );
    assert_eq!(
        refusal("define r(x, y):\n    Base(x, y) or (Base(x, m) and r(x, m) and r(m, y))"),
        RecursionRefusal::SeveralSelfCalls,
    );
    assert_eq!(
        refusal("define r(x, y):\n    Base(x, y) and not r(y, x)"),
        RecursionRefusal::Position,
    );
    assert_eq!(
        refusal("define r(x, y):\n    Base(x, y) or (Base(x, m) and (exists z: r(m, z)))"),
        RecursionRefusal::Position,
    );
}

// A cycle through another definition is still a cycle.
#[test]
fn two_definitions_that_call_each_other_are_still_refused() {
    let source = "program p\n\npredicate Base(x: Decimal, y: Decimal)\n\ndefine r(x, y):\n    Base(x, y) or (Base(x, m) and s(m, y))\n\ndefine s(x, y):\n    Base(x, m) and r(m, y)\n";
    let program = parse_program(source).expect("the programme parses");
    let errors = program.validate().expect_err("the programme is refused");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::DefinitionCycle { .. })),
        "{errors:?}"
    );
}

// The call to itself is held to the kinds the claims give the
// parameters: here it passes the count where the part belongs.
#[test]
fn a_call_to_itself_with_the_wrong_kind_is_refused() {
    use morpholog_core::{PredicateArgKind, ValidationContext};
    let source = "program p\n\npredicate Step(part: Subject, count: Decimal)\n\ndefine r(x, n):\n    Step(x, n) or (Step(x, m) and r(n, x))\n";
    let program = parse_program(source).expect("the programme parses");
    let errors = program.validate().expect_err("the programme is refused");
    assert_eq!(
        errors,
        vec![ValidationError::VariableKindConflict {
            variable: "x".into(),
            previous: PredicateArgKind::Subject,
            new: PredicateArgKind::Decimal,
            context: ValidationContext::Definition { name: "r".into() },
        }]
    );
}

// The same body with the arguments the right way round is accepted.
#[test]
fn a_call_to_itself_with_the_right_kinds_is_accepted() {
    let source = "program p\n\npredicate Step(part: Subject, count: Decimal)\npredicate Next(part: Subject, next: Subject)\n\ndefine r(x, n):\n    Step(x, n) or (Next(x, m) and r(m, n))\n";
    let program = parse_program(source).expect("the programme parses");
    program.validate().expect("the programme validates");
}

// A caller is held to those kinds too: a count, bound or written out,
// is not a part.
#[test]
fn a_caller_passing_the_wrong_kind_is_refused() {
    use morpholog_core::{PredicateArgKind, ValidationContext, VocabularyKind};
    let with_gate = |binds: &str, gate: &str| {
        let source = format!(
            "program p\n\npredicate Parent(part: Subject, parent: Subject)\npredicate Count(part: Subject, n: Decimal)\npredicate Out(part: Subject)\n\ndefine descends_from(part, ancestor):\n    Parent(part, ancestor) or (Parent(part, between) and descends_from(between, ancestor))\n\ntransformation t(part):\n{binds}    require {gate}\n    admit Out(part)\n"
        );
        let program = parse_program(&source).expect("the programme parses");
        program.validate().expect_err("the programme is refused")
    };
    assert_eq!(
        with_gate("    bind Count(part, n)\n", "descends_from(part, n)"),
        vec![ValidationError::VariableKindConflict {
            variable: "n".into(),
            previous: PredicateArgKind::Decimal,
            new: PredicateArgKind::Subject,
            context: ValidationContext::Transformation {
                name: "t".into(),
                statement: Some(1),
            },
        }]
    );
    assert_eq!(
        with_gate("", "descends_from(part, 5)"),
        vec![ValidationError::ArgKindMismatch {
            vocabulary: VocabularyKind::Definition,
            name: "descends_from".into(),
            position: 1,
            expected: PredicateArgKind::Subject,
            actual: PredicateArgKind::Decimal,
            context: ValidationContext::Transformation {
                name: "t".into(),
                statement: Some(0),
            },
        }]
    );
}

// Two answers that would each raise a different error. Which one is
// reported follows the order of the claims, as it does for any
// condition; the tables add no order of their own, so the same claims
// in the same order report the same error on every run, each run with
// freshly seeded hash maps.
#[test]
fn competing_errors_are_chosen_by_claim_order_alone() {
    use morpholog_core::EvalError;
    let source = "program errors\n\npredicate Step(part: Subject, next: Subject)\npredicate Weight(part: Subject, w: Decimal)\npredicate Probe(part: Subject)\n\ndefine reaches(x, y):\n    Step(x, y)\n    or (Step(x, m) and reaches(m, y) and 1 / (value Weight(y, _)) > 0)\n\ninvariant nothing_is_reached:\n    Probe(p) implies not reaches(p, _)\n";
    let p = parse_program(source).expect("the programme parses");
    p.validate().expect("the programme validates");
    let rule = p.invariant("nothing_is_reached").unwrap();
    // From `a` through `m`: `zero` weighs nothing, so dividing by its
    // weight fails; `none` has no weight to look up.
    let mut claims = vec![
        claim_instance("Probe", &[subj("a")]),
        claim_instance("Step", &[subj("a"), subj("m")]),
        claim_instance("Step", &[subj("m"), subj("zero")]),
        claim_instance("Step", &[subj("m"), subj("none")]),
        claim_instance("Weight", &[subj("m"), dec(1)]),
        claim_instance("Weight", &[subj("zero"), dec(0)]),
    ];
    let mut seen = Vec::new();
    for _ in 0..2 {
        let first = eval_invariant(&p, rule, &State::from_claims(claims.clone()), None)
            .expect_err("one of the two errors");
        for _ in 0..32 {
            let again = eval_invariant(&p, rule, &State::from_claims(claims.clone()), None)
                .expect_err("one of the two errors");
            assert_eq!(format!("{again:?}"), format!("{first:?}"));
        }
        seen.push(first);
        claims.reverse();
    }
    assert!(
        matches!(seen[0], EvalError::DivisionByZero),
        "{:?}",
        seen[0]
    );
    assert!(
        matches!(&seen[1], EvalError::ValueOfZeroMatches(predicate) if predicate.as_str() == "Weight"),
        "{:?}",
        seen[1]
    );
}
