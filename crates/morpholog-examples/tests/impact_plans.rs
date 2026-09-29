//! Following defined calls may only change the plan of a rule that makes
//! one. Every call-free rule in the gallery keeps exactly the plan the
//! conservative planner gives it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::all_programs;
use morpholog_core::fold::{Node, walk_prop};
use morpholog_core::{ImpactPlan, Prop};

fn calls_a_definition(body: &Prop) -> bool {
    let mut found = false;
    walk_prop(body, &mut |n| {
        if matches!(n, Node::Prop(Prop::Defined { .. })) {
            found = true;
        }
    });
    found
}

#[test]
fn a_call_free_rule_keeps_its_plan() {
    let mut checked = 0;
    for program in all_programs() {
        for inv in &program.invariants {
            if calls_a_definition(&inv.body) {
                continue;
            }
            assert_eq!(
                ImpactPlan::with_definitions(inv, &program.definitions),
                ImpactPlan::new(inv),
                "{}::{}",
                program.name,
                inv.name
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "no call-free rule in the gallery");
}
