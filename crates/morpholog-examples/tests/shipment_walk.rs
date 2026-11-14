//! The embedder guide's walk in a recorded order ("A walk in a recorded order"), run as the
//! guide prints it: a merge after allocation keeps each allocation's place in the shipment, and
//! the walk then splits and empties items as InvenTree's loop does.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

use morpholog_core::{EvalValue, Program, State};
use morpholog_test_support::{dec, must_accept, subj};
use std::collections::BTreeMap;

/// The first `morph` block under the guide's section on the walk.
fn guide_programme() -> Program {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/embedder-integration.md");
    let guide = std::fs::read_to_string(&path).expect("docs/embedder-integration.md");
    let section = guide
        .split_once("### A walk in a recorded order")
        .expect("the guide has the section")
        .1;
    let block = section
        .split_once("```morph\n")
        .expect("the section has a programme")
        .1
        .split_once("```")
        .expect("the block closes")
        .0;
    let program = morpholog_surface::parse_program(block).expect("the guide's programme parses");
    program.validate().expect("the guide's programme validates");
    program
}

fn act(program: &Program, state: State, name: &str, args: Vec<EvalValue>) -> State {
    must_accept(program.transformation(name).unwrap(), args, state, program)
}

/// Each item's quantity, keyed by subject.
fn items(state: &State) -> BTreeMap<String, EvalValue> {
    state
        .claims_for("Item")
        .map(|c| match &c.args[0] {
            EvalValue::Subject(s) => (s.to_string(), c.args[1].clone()),
            other => panic!("an item is a subject, not {other:?}"),
        })
        .collect()
}

/// The quantities shipped, children and whole items alike, in order.
fn shipped(state: &State) -> Vec<EvalValue> {
    let quantities = items(state);
    let mut out: Vec<EvalValue> = state
        .claims_for("Shipped")
        .map(|c| match &c.args[0] {
            EvalValue::Subject(s) => quantities[&s.to_string()].clone(),
            other => panic!("a shipped item is a subject, not {other:?}"),
        })
        .collect();
    out.sort_by_key(|v| format!("{v:?}"));
    out
}

fn shipment_with(program: &Program, stock: &[(&str, i64)]) -> State {
    let mut state = act(program, State::default(), "open_shipment", vec![subj("s")]);
    for (item, quantity) in stock {
        state = act(program, state, "stock", vec![subj(item), dec(*quantity)]);
    }
    state
}

/// Allocations of A and B into one shipment are each first for their own item. After B merges
/// into A, both draw from A, and their places in the shipment still order them, so the walk
/// takes 2 and then 2 from A's 10.
#[test]
fn a_merge_after_allocation_keeps_the_walk_in_order() {
    let program = guide_programme();
    let mut state = shipment_with(&program, &[("A", 6), ("B", 4)]);
    for (alloc, item) in [("a1", "A"), ("a2", "B")] {
        let args = vec![subj(alloc), subj("s"), subj(item), dec(2)];
        state = act(&program, state, "allocate", args);
    }
    state = act(&program, state, "merge_stock", vec![subj("B"), subj("A")]);
    state = act(&program, state, "ship", vec![subj("s")]);
    assert_eq!(shipped(&state), [dec(2), dec(2)]);
    let items = items(&state);
    assert_eq!(items["A"], dec(6), "{items:?}");
    assert!(!items.contains_key("B"), "{items:?}");
}

/// An item used up exactly: the first allocation splits off a child, and the last takes the
/// item whole with what is left.
#[test]
fn the_last_allocation_takes_what_is_left_whole() {
    let program = guide_programme();
    let mut state = shipment_with(&program, &[("A", 10)]);
    for (alloc, quantity) in [("a1", 6), ("a2", 4)] {
        let args = vec![subj(alloc), subj("s"), subj("A"), dec(quantity)];
        state = act(&program, state, "allocate", args);
    }
    state = act(&program, state, "ship", vec![subj("s")]);
    assert_eq!(shipped(&state), [dec(4), dec(6)]);
    let ships_a_whole: Vec<_> = state
        .claims_for("ShipsAs")
        .filter(|c| c.args[1] == subj("A"))
        .map(|c| c.args[0].clone())
        .collect();
    assert_eq!(ships_a_whole, [subj("a2")]);
}
