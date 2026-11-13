//! A condition that calls itself, held to a reference that builds its
//! whole relation: start empty, evaluate the body with the call to
//! itself reading the table so far, stop when a round adds nothing.

use std::collections::BTreeSet;

use proptest::prelude::*;

use crate::definitions::DefinitionTable;
use crate::eval::{EvalContext, find_matches};
use crate::ir::{Definition, Prop, Term};
use crate::ir_builder::{and, claim, defined, definition, neq, or, params, var};
use crate::kernel_tests::support::subj;
use crate::state::{Bindings, ClaimInstance, EvalValue, cmp_value};
use crate::{State, ValidationError};

const TABLE: &str = "table_so_far";

fn v(names: &[&str]) -> Vec<Term> {
    names.iter().map(|n| var(n)).collect()
}

/// The recursive conditions under test, each named `r`.
fn family() -> Vec<(&'static str, Definition)> {
    vec![
        (
            "walks up",
            definition(
                "r",
                params(&["x", "y"]),
                or(vec![
                    claim("E", v(&["x", "y"])),
                    and(vec![
                        claim("E", v(&["x", "m"])),
                        defined("r", v(&["m", "y"])),
                    ]),
                ]),
            ),
        ),
        (
            "walks down",
            definition(
                "r",
                params(&["x", "y"]),
                or(vec![
                    claim("E", v(&["x", "y"])),
                    and(vec![
                        claim("E", v(&["m", "y"])),
                        defined("r", v(&["x", "m"])),
                    ]),
                ]),
            ),
        ),
        (
            "changes the free argument on the way back",
            definition(
                "r",
                params(&["x", "y"]),
                or(vec![
                    claim("E", v(&["x", "y"])),
                    and(vec![
                        claim("S", v(&["x", "x2", "y2", "y"])),
                        defined("r", v(&["x2", "y2"])),
                    ]),
                ]),
            ),
        ),
        (
            "asks the question turned round",
            definition(
                "r",
                params(&["x", "y"]),
                or(vec![
                    claim("E", v(&["x", "y"])),
                    and(vec![
                        claim("F", v(&["y", "m"])),
                        defined("r", v(&["m", "x"])),
                    ]),
                ]),
            ),
        ),
        (
            "two recursive branches and a filter",
            definition(
                "r",
                params(&["x", "y"]),
                or(vec![
                    claim("E", v(&["x", "y"])),
                    and(vec![
                        claim("E", v(&["x", "m"])),
                        neq(var("m"), var("x")),
                        defined("r", v(&["m", "y"])),
                    ]),
                    and(vec![
                        claim("F", v(&["x", "m"])),
                        defined("r", v(&["m", "y"])),
                    ]),
                ]),
            ),
        ),
        (
            "carries a third argument through",
            definition(
                "r",
                params(&["x", "y", "k"]),
                or(vec![
                    claim("S", v(&["x", "y", "k", "k"])),
                    and(vec![
                        claim("S", v(&["x", "m", "k", "k"])),
                        defined("r", v(&["m", "y", "k"])),
                    ]),
                ]),
            ),
        ),
        (
            "passes one variable twice",
            definition(
                "r",
                params(&["x", "y"]),
                or(vec![
                    claim("E", v(&["x", "y"])),
                    and(vec![
                        claim("E", v(&["x", "m"])),
                        defined("r", v(&["m", "m"])),
                        claim("F", v(&["m", "y"])),
                    ]),
                ]),
            ),
        ),
    ]
}

/// The body with the call to itself read as a claim over [`TABLE`].
fn over_the_table(prop: &Prop) -> Prop {
    match prop {
        Prop::Or(items) => Prop::Or(items.iter().map(over_the_table).collect()),
        Prop::And(items) => Prop::And(items.iter().map(over_the_table).collect()),
        Prop::Defined { name, args } if name.as_str() == "r" => claim(TABLE, args.clone()),
        other => other.clone(),
    }
}

/// The whole relation of `def` over `state`.
fn reference(def: &Definition, state: &State) -> BTreeSet<Vec<String>> {
    let body = over_the_table(def.body.as_prop().unwrap());
    let mut state = state.clone();
    let mut table: BTreeSet<Vec<String>> = BTreeSet::new();
    loop {
        let bindings = Bindings::new();
        let ctx = EvalContext::new(&state, None, &bindings, None, DefinitionTable::new(&[]));
        let mut fresh = Vec::new();
        for m in find_matches(&body, &ctx).unwrap() {
            let tuple: Vec<EvalValue> = def.parameters.iter().map(|p| (*m[p]).clone()).collect();
            if table.insert(tuple.iter().map(text).collect()) {
                fresh.push(ClaimInstance {
                    predicate: TABLE.into(),
                    args: tuple,
                });
            }
        }
        if fresh.is_empty() {
            return table;
        }
        state.apply(&fresh, &[]);
    }
}

fn text(value: &EvalValue) -> String {
    format!("{value:?}")
}

const PARTS: [&str; 4] = ["a", "b", "c", "d"];

/// Every way to call a condition of `arity`: each argument a known part,
/// or one of the variables `u0`, `u1` (so two arguments may share one).
fn calls(arity: usize) -> Vec<Vec<Option<&'static str>>> {
    let mut all: Vec<Vec<Option<&'static str>>> = vec![vec![]];
    for _ in 0..arity {
        all = all
            .into_iter()
            .flat_map(|prefix| {
                PARTS
                    .iter()
                    .map(|p| Some(*p))
                    .chain([None])
                    .map(move |next| prefix.iter().copied().chain([next]).collect())
            })
            .collect();
    }
    all
}

/// The kernel's answers to one call, as the values of the call's free
/// variables, in the order it returned them. `shared` gives every free
/// argument the same variable.
fn asked(
    def: &Definition,
    state: &State,
    call: &[Option<&str>],
    shared: bool,
) -> Vec<Vec<EvalValue>> {
    let mut bindings = Bindings::new();
    let mut free = Vec::new();
    let args: Vec<Term> = call
        .iter()
        .enumerate()
        .map(|(i, known)| {
            let name = match known {
                Some(part) => {
                    bindings.insert(format!("k{i}").as_str().into(), subj(part).into());
                    format!("k{i}")
                }
                None if shared => "u".to_string(),
                None => format!("u{i}"),
            };
            if known.is_none() && !free.contains(&name) {
                free.push(name.clone());
            }
            var(&name)
        })
        .collect();
    let definitions = [def.clone()];
    let ctx = EvalContext::new(
        state,
        None,
        &bindings,
        None,
        DefinitionTable::new(&definitions),
    );
    find_matches(&defined("r", args), &ctx)
        .unwrap()
        .iter()
        .map(|m| {
            free.iter()
                .map(|name| (*m[&crate::ir::Var::from(name.as_str())]).clone())
                .collect()
        })
        .collect()
}

/// What the reference says the same call answers.
fn expected(
    relation: &BTreeSet<Vec<String>>,
    call: &[Option<&str>],
    shared: bool,
) -> BTreeSet<Vec<String>> {
    relation
        .iter()
        .filter(|tuple| {
            call.iter()
                .zip(tuple.iter())
                .all(|(known, value)| known.is_none_or(|part| &text(&subj(part)) == value))
        })
        .filter_map(|tuple| {
            let free: Vec<String> = call
                .iter()
                .zip(tuple)
                .filter(|(known, _)| known.is_none())
                .map(|(_, value)| value.clone())
                .collect();
            if !shared {
                return Some(free);
            }
            let Some(first) = free.first().cloned() else {
                return Some(free);
            };
            free.iter().all(|f| f == &first).then(|| vec![first])
        })
        .collect()
}

fn pairs(predicate: &'static str) -> impl Strategy<Value = Vec<ClaimInstance>> {
    prop::collection::vec((0..PARTS.len(), 0..PARTS.len()), 0..7).prop_map(move |edges| {
        edges
            .into_iter()
            .map(|(a, b)| ClaimInstance {
                predicate: predicate.into(),
                args: vec![subj(PARTS[a]), subj(PARTS[b])],
            })
            .collect()
    })
}

fn steps() -> impl Strategy<Value = Vec<ClaimInstance>> {
    let part = 0..PARTS.len();
    prop::collection::vec((part.clone(), part.clone(), part.clone(), part), 0..6).prop_map(|rows| {
        rows.into_iter()
            .map(|(a, b, c, d)| ClaimInstance {
                predicate: "S".into(),
                args: vec![
                    subj(PARTS[a]),
                    subj(PARTS[b]),
                    subj(PARTS[c]),
                    subj(PARTS[d]),
                ],
            })
            .collect()
    })
}

fn is_in_canonical_order(rows: &[Vec<EvalValue>]) -> bool {
    rows.windows(2).all(|w| {
        w[0].iter()
            .zip(&w[1])
            .map(|(l, r)| cmp_value(l, r))
            .find(|o| o.is_ne())
            .is_some_and(std::cmp::Ordering::is_lt)
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    // Every call pattern of every condition in the family, over random
    // small graphs with cycles and self-loops, answers what the whole
    // relation says, each answer once, in the canonical order.
    #[test]
    fn a_recursive_call_answers_what_the_whole_relation_holds(
        e in pairs("E"),
        f in pairs("F"),
        s in steps(),
    ) {
        let state = State::from_claims(e.into_iter().chain(f).chain(s).collect());
        for (label, def) in family() {
            prop_assert_eq!(crate::recursion::refusal(&def), None, "{}", label);
            let relation = reference(&def, &state);
            for call in calls(def.parameters.len()) {
                for shared in [false, true] {
                    let rows = asked(&def, &state, &call, shared);
                    let got: BTreeSet<Vec<String>> =
                        rows.iter().map(|r| r.iter().map(text).collect()).collect();
                    prop_assert_eq!(rows.len(), got.len(), "{}: {:?} repeats an answer", label, &call);
                    prop_assert_eq!(&got, &expected(&relation, &call, shared), "{}: {:?} shared={}", label, &call, shared);
                    prop_assert!(is_in_canonical_order(&rows), "{}: {:?} out of order", label, &call);
                }
            }
        }
    }
}

fn edge(predicate: &str, args: &[&str]) -> ClaimInstance {
    ClaimInstance {
        predicate: predicate.into(),
        args: args.iter().map(|a| subj(a)).collect(),
    }
}

// A walk that stops at a question already in progress answers this with
// `one` alone: the step leads from `a` back to `a`, and each pass turns
// the answer it was given into the next.
#[test]
fn an_answer_found_late_reaches_the_question_that_asked_itself() {
    let def = family().remove(2).1;
    let state = State::from_claims(vec![
        edge("E", &["a", "one"]),
        edge("S", &["a", "a", "one", "two"]),
        edge("S", &["a", "a", "two", "three"]),
    ]);
    let rows = asked(&def, &state, &[Some("a"), None], false);
    let got: BTreeSet<String> = rows.iter().map(|r| text(&r[0])).collect();
    let want: BTreeSet<String> = ["one", "two", "three"]
        .iter()
        .map(|p| text(&subj(p)))
        .collect();
    assert_eq!(got, want);
}

// Two questions that read each other: `r(a, ?)` asks `r(b, ?)`, which
// asks `r(a, ?)` back. Each must end with both parts.
#[test]
fn two_questions_that_ask_each_other_both_get_every_answer() {
    let def = family().remove(0).1;
    let state = State::from_claims(vec![edge("E", &["a", "b"]), edge("E", &["b", "a"])]);
    for from in ["a", "b"] {
        let rows = asked(&def, &state, &[Some(from), None], false);
        assert_eq!(rows, vec![vec![subj("a")], vec![subj("b")]], "from {from}");
    }
}

// The stack must not grow with the data: this chain is far deeper than
// the nesting limit a programme may have.
#[test]
fn a_long_chain_is_walked_without_nesting() {
    let def = family().remove(0).1;
    let claims = (1..20_000)
        .map(|i| edge("E", &[&format!("p{i}"), &format!("p{}", i - 1)]))
        .collect();
    let state = State::from_claims(claims);
    assert_eq!(
        asked(&def, &state, &[Some("p19999"), Some("p0")], false).len(),
        1
    );
    assert!(asked(&def, &state, &[Some("p0"), Some("p19999")], false).is_empty());
}

fn refused(def: Definition) -> crate::RecursionRefusal {
    let program = crate::ir_builder::program("p")
        .definitions(vec![def])
        .build();
    match crate::validate::structural_errors(&program) {
        Err(errors) => errors
            .into_iter()
            .find_map(|e| match e {
                ValidationError::RecursionNotBounded { refusal, .. } => Some(refusal),
                _ => None,
            })
            .expect("a recursion refusal"),
        Ok(()) => panic!("the definition was accepted"),
    }
}

#[test]
fn two_calls_to_itself_in_one_branch_are_refused() {
    let def = definition(
        "r",
        params(&["x", "y"]),
        or(vec![
            claim("E", v(&["x", "y"])),
            and(vec![
                claim("E", v(&["x", "m"])),
                defined("r", v(&["x", "m"])),
                defined("r", v(&["m", "y"])),
            ]),
        ]),
    );
    assert_eq!(refused(def), crate::RecursionRefusal::SeveralSelfCalls);
}

#[test]
fn a_call_to_itself_under_not_is_refused() {
    let def = definition(
        "r",
        params(&["x", "y"]),
        and(vec![
            claim("E", v(&["x", "y"])),
            crate::ir_builder::not(defined("r", v(&["y", "x"]))),
        ]),
    );
    assert_eq!(refused(def), crate::RecursionRefusal::Position);
}

// The variable the call to itself is passed must already be bound when
// the matcher reaches the call, or the call would ask about everything.
#[test]
fn an_argument_bound_only_after_the_call_is_refused() {
    let def = definition(
        "r",
        params(&["x", "y"]),
        or(vec![
            claim("E", v(&["x", "y"])),
            and(vec![
                defined("r", v(&["m", "y"])),
                claim("E", v(&["x", "m"])),
            ]),
        ]),
    );
    assert_eq!(
        refused(def),
        crate::RecursionRefusal::ArgumentNotFromClaim {
            argument: "m".into()
        }
    );
}

#[test]
fn a_parameter_a_branch_takes_from_no_claim_is_refused() {
    let def = definition(
        "r",
        params(&["x", "y"]),
        or(vec![
            crate::ir_builder::eq(
                crate::ir_builder::term(var("x")),
                crate::ir_builder::term(var("y")),
            ),
            and(vec![
                claim("E", v(&["x", "m"])),
                defined("r", v(&["m", "y"])),
            ]),
        ]),
    );
    assert_eq!(
        refused(def),
        crate::RecursionRefusal::ParameterNotFromClaim {
            parameter: "x".into()
        }
    );
}
