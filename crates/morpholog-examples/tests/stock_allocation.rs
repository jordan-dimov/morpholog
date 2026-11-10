//! The stock allocation example (`examples/25_stock_allocation/`).
//!
//! Proves: shipping an allocation that takes less than its item splits a
//! child off and conserves stock; one that takes the whole item ships the
//! item and draws no subject; the trace says which branch ran; and the
//! rules refuse an allocation beyond the line, a change that would
//! overflow it, a second allocation of one item on one shipment, shipping
//! twice, changing an allocation whose item has shipped, and closing a
//! shipment with stock still on the shelf.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::OnceLock;

use common::{
    Example, claim_instance, dec, eval_invariant, has_claim, propose_with_trace, refused_by, subj,
    subjects,
};
use morpholog_core::{EvalValue, Outcome, State, TraceEntry, TracedProposal, Transition};
use morpholog_examples::stock_allocation;

fn ex() -> &'static Example {
    static EX: OnceLock<Example> = OnceLock::new();
    EX.get_or_init(|| Example::new(&stock_allocation::program()))
}

/// A pallet of bolts on the shelf, an order line for some, and a
/// shipment open.
fn stocked(on_hand: i64, ordered: i64) -> State {
    let mut state = State::default();
    for (t, args) in [
        (stock_allocation::add_part(), vec![subj("bolt")]),
        (
            stock_allocation::receive(),
            vec![subj("pallet"), subj("bolt"), dec(on_hand)],
        ),
        (
            stock_allocation::place_order(),
            vec![subj("order"), subj("customer")],
        ),
        (
            stock_allocation::add_line(),
            vec![subj("line"), subj("order"), subj("bolt"), dec(ordered)],
        ),
        (
            stock_allocation::open_shipment(),
            vec![subj("shipment"), subj("order")],
        ),
    ] {
        state = ex().must_accept(&t, args, state);
    }
    state
}

fn allocated(state: State, quantity: i64) -> State {
    ex().must_accept(
        &stock_allocation::allocate(),
        vec![
            subj("alloc"),
            subj("line"),
            subj("shipment"),
            subj("pallet"),
            dec(quantity),
        ],
        state,
    )
}

fn quantity_of(state: &State, item: &EvalValue) -> EvalValue {
    state
        .claims_for("StockItem")
        .find(|c| c.args[0] == *item)
        .map(|c| c.args[2].clone())
        .unwrap_or_else(|| panic!("no stock item {item:?}"))
}

/// Ship the one allocation with a trace, drawing from `fresh`, and
/// return the state that same execution produced, which branch ran,
/// and that branch's entries.
fn ship_traced(state: &State, fresh: [&str; 1]) -> (State, bool, Vec<TraceEntry>) {
    let transition = Transition {
        transformation_name: "ship_allocation".into(),
        args: vec![subj("alloc")],
        actor: "picker".into(),
        decided_at: None,
    };
    let traced = propose_with_trace(
        &stock_allocation::program(),
        &transition,
        state,
        &mut subjects(fresh),
    );
    let TracedProposal::Completed {
        outcome: Outcome::Accepted {
            candidate_state, ..
        },
        trace,
    } = traced
    else {
        panic!("shipping should be admitted: {traced:?}");
    };
    let branches: Vec<_> = trace
        .iter()
        .filter_map(|e| match e {
            TraceEntry::If { taken, trace, .. } => Some((*taken, trace.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(branches.len(), 1, "one if in the act: {trace:?}");
    (candidate_state, branches[0].0, branches[0].1.clone())
}

#[test]
fn an_allocation_that_takes_part_of_the_item_splits_it_and_conserves_stock() {
    let state = allocated(stocked(10, 8), 3);
    let (after, taken, branch) = ship_traced(&state, ["child"]);
    assert!(taken, "3 of 10 takes the split branch");
    assert!(
        branch.iter().any(
            |e| matches!(e, TraceEntry::LetNewSubject { subject, .. } if *subject == subj("child"))
        ),
        "the child is minted inside the branch: {branch:?}"
    );

    // The lineage names the child the trace drew; the figures read back
    // conserve the ten that arrived.
    let lineage = after
        .claims_for("SplitFrom")
        .next()
        .expect("the split is recorded")
        .clone();
    let child = lineage.args[0].clone();
    assert_eq!(child, subj("child"));
    assert_eq!(lineage.args[1], subj("pallet"));
    assert_eq!(lineage.args[2], dec(3));
    assert_eq!(quantity_of(&after, &subj("pallet")), dec(7));
    assert_eq!(quantity_of(&after, &child), dec(3));
    assert!(has_claim(&after, "Received", &[subj("pallet"), dec(10)]));
    // The child went to the customer and was never on the shelf; the
    // parent stays; the allocation now names the child.
    assert!(has_claim(
        &after,
        "ShippedTo",
        &[child.clone(), subj("customer")]
    ));
    assert!(!has_claim(&after, "InStock", std::slice::from_ref(&child)));
    assert!(has_claim(&after, "InStock", &[subj("pallet")]));
    assert!(has_claim(
        &after,
        "Allocation",
        &[subj("alloc"), subj("line"), subj("shipment"), child, dec(3)]
    ));
    assert!(has_claim(&after, "LineShipped", &[subj("line"), dec(3)]));
}

#[test]
fn an_allocation_that_takes_the_whole_item_ships_it_and_draws_nothing() {
    let state = allocated(stocked(8, 8), 8);
    let (after, taken, branch) = ship_traced(&state, ["unused"]);
    assert!(!taken, "8 of 8 takes the whole branch");
    assert!(
        !branch
            .iter()
            .any(|e| matches!(e, TraceEntry::LetNewSubject { .. })),
        "the whole branch mints nothing: {branch:?}"
    );
    assert!(after.claims_for("SplitFrom").next().is_none(), "no split");
    assert_eq!(quantity_of(&after, &subj("pallet")), dec(8));
    assert!(has_claim(
        &after,
        "ShippedTo",
        &[subj("pallet"), subj("customer")]
    ));
    assert!(!has_claim(&after, "InStock", &[subj("pallet")]));
    assert!(has_claim(&after, "LineShipped", &[subj("line"), dec(8)]));
}

#[test]
fn the_rules_refuse_what_the_acts_may_not_do() {
    let state = stocked(10, 8);
    // Beyond the line.
    refused_by(
        &ex().must_reject(
            &stock_allocation::allocate(),
            vec![
                subj("alloc"),
                subj("line"),
                subj("shipment"),
                subj("pallet"),
                dec(9),
            ],
            &state,
        ),
        "within_line",
    );
    let state = allocated(state, 3);
    // A change that no longer fits is refused by a rule: the act repeats
    // no check.
    refused_by(
        &ex().must_reject(
            &stock_allocation::change_allocation(),
            vec![subj("alloc"), dec(9)],
            &state,
        ),
        "lines_are_not_overallocated",
    );
    // A second allocation of the same item on the same shipment.
    refused_by(
        &ex().must_reject(
            &stock_allocation::allocate(),
            vec![
                subj("alloc_2"),
                subj("line"),
                subj("shipment"),
                subj("pallet"),
                dec(2),
            ],
            &state,
        ),
        "once_per_shipment",
    );
    // Closing with stock still on the shelf.
    refused_by(
        &ex().must_reject(
            &stock_allocation::close_shipment(),
            vec![subj("shipment")],
            &state,
        ),
        "every_item_shipped",
    );
    let shipped = ex().must_accept(
        &stock_allocation::ship_allocation(),
        vec![subj("alloc")],
        state,
    );
    // Shipping the same allocation again: its item is now the child,
    // which was never on the shelf.
    refused_by(
        &ex().must_reject(
            &stock_allocation::ship_allocation(),
            vec![subj("alloc")],
            &shipped,
        ),
        "still_in_stock",
    );
    // Changing an allocation whose stock has gone to the customer, with
    // the shipment still open.
    refused_by(
        &ex().must_reject(
            &stock_allocation::change_allocation(),
            vec![subj("alloc"), dec(2)],
            &shipped,
        ),
        "not_already_shipped",
    );
    let closed = ex().must_accept(
        &stock_allocation::close_shipment(),
        vec![subj("shipment")],
        shipped,
    );
    assert!(has_claim(&closed, "ShipmentShipped", &[subj("shipment")]));
    // And nothing more may be allocated on, or changed under, a shipment
    // that has gone.
    refused_by(
        &ex().must_reject(
            &stock_allocation::change_allocation(),
            vec![subj("alloc"), dec(2)],
            &closed,
        ),
        "not_yet_shipped",
    );
}

#[test]
fn a_change_within_the_stock_is_admitted_and_the_read_shows_it() {
    let state = allocated(stocked(10, 8), 3);
    let changed = ex().must_accept(
        &stock_allocation::change_allocation(),
        vec![subj("alloc"), dec(5)],
        state,
    );
    assert!(has_claim(
        &changed,
        "Allocation",
        &[
            subj("alloc"),
            subj("line"),
            subj("shipment"),
            subj("pallet"),
            dec(5)
        ]
    ));
    let program = stock_allocation::program();
    let allocated = program
        .derived_claims
        .iter()
        .find(|d| d.predicate.as_str() == "Allocated")
        .unwrap();
    let rows = morpholog_test_support::enumerate_derived(&program, allocated, &changed).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].args, vec![subj("pallet"), dec(5)]);
}

/// No act can move a shipped allocation's figure off what its item holds,
/// so the rule is asked directly: the record a split leaves satisfies it,
/// and the same record with the allocation changed breaks it.
#[test]
fn a_shipped_allocation_may_not_drift_from_what_its_item_holds() {
    let state = allocated(stocked(10, 8), 3);
    let (shipped, _, _) = ship_traced(&state, ["child"]);
    let program = stock_allocation::program();
    let rule = stock_allocation::shipped_allocations_are_settled();
    assert!(eval_invariant(&program, &rule, &shipped, None).unwrap());

    let allocation = |quantity: i64| {
        claim_instance(
            "Allocation",
            &[
                subj("alloc"),
                subj("line"),
                subj("shipment"),
                subj("child"),
                dec(quantity),
            ],
        )
    };
    let mut drifted = shipped.claims().to_vec();
    let at = drifted
        .iter()
        .position(|c| *c == allocation(3))
        .expect("the allocation names the child");
    drifted[at] = allocation(2);
    assert!(
        !eval_invariant(&program, &rule, &State::from_claims(drifted), None).unwrap(),
        "an allocation of 2 against a shipped child of 3 breaks the rule"
    );
}
