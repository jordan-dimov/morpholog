//! The stock allocation example (`examples/25_stock_allocation/`).
//!
//! Proves: shipping a shipment visits every allocation on it, splitting
//! an item an allocation takes part of and conserving stock, shipping
//! whole an item an allocation takes all of; the children drawn are the
//! same whatever order the record was loaded in; the trace says which
//! allocation each pass ran for and which branch it took; cancelling an
//! order takes every line and allocation with it; and the rules refuse
//! an allocation beyond the line, a change that would overflow it, a
//! second allocation of one item on one shipment, shipping twice,
//! changing an allocation on a shipment that has gone, cancelling an
//! order with stock already shipped, and closing an order short without
//! the authority to or with a flag that is not a Boolean.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use std::sync::OnceLock;

use common::{
    Example, claim_instance, dec, eval_invariant, explain, has_claim, propose_with_trace,
    refused_by, subj, subjects,
};
use morpholog_core::{
    EvalValue, Outcome, Rejection, State, TraceEntry, TracedProposal, Transition, Verdict,
};
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

fn allocate(state: State, allocation: &str, item: &str, quantity: i64) -> State {
    ex().must_accept(
        &stock_allocation::allocate(),
        vec![
            subj(allocation),
            subj("line"),
            subj("shipment"),
            subj(item),
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

/// The rows of a derived read, in the kernel's order.
fn read(state: &State, predicate: &str) -> Vec<Vec<EvalValue>> {
    let program = stock_allocation::program();
    let derived = program
        .derived_claims
        .iter()
        .find(|d| d.predicate.as_str() == predicate)
        .unwrap();
    morpholog_test_support::enumerate_derived(&program, derived, state)
        .unwrap()
        .into_iter()
        .map(|c| c.args)
        .collect()
}

/// Ship the shipment with a trace, drawing from `fresh`, and return the
/// state that same execution produced and, per allocation visited, the
/// allocation and which branch its pass took.
fn ship_traced<const N: usize>(state: &State, fresh: [&str; N]) -> (State, Vec<(EvalValue, bool)>) {
    let transition = Transition {
        transformation_name: "ship".into(),
        args: vec![subj("shipment")],
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
    let loops: Vec<_> = trace
        .iter()
        .filter_map(|e| match e {
            TraceEntry::ForEach { iterations, .. } => Some(iterations),
            _ => None,
        })
        .collect();
    assert_eq!(loops.len(), 1, "one loop in the act: {trace:?}");
    let passes = loops[0]
        .iter()
        .map(|pass| {
            let branches: Vec<bool> = pass
                .trace
                .iter()
                .filter_map(|e| match e {
                    TraceEntry::If { taken, .. } => Some(*taken),
                    _ => None,
                })
                .collect();
            assert_eq!(branches.len(), 1, "one if per pass: {:?}", pass.trace);
            (pass.claim.args[0].clone(), branches[0])
        })
        .collect();
    (candidate_state, passes)
}

#[test]
fn an_allocation_that_takes_part_of_the_item_splits_it_and_conserves_stock() {
    let state = allocate(stocked(10, 8), "alloc", "pallet", 3);
    let (after, passes) = ship_traced(&state, ["child"]);
    assert_eq!(
        passes,
        vec![(subj("alloc"), true)],
        "3 of 10 takes the split branch"
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
    // parent stays; the allocation now names the child; the shipment
    // has gone and the line's shipped figure reads back from it.
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
    assert!(has_claim(&after, "ShipmentShipped", &[subj("shipment")]));
    assert_eq!(
        read(&after, "LineShipped"),
        vec![vec![subj("line"), dec(3)]]
    );
}

#[test]
fn an_allocation_that_takes_the_whole_item_ships_it_and_draws_nothing() {
    let state = allocate(stocked(8, 8), "alloc", "pallet", 8);
    let (after, passes) = ship_traced(&state, ["unused"]);
    assert_eq!(
        passes,
        vec![(subj("alloc"), false)],
        "8 of 8 takes the whole branch"
    );
    assert!(after.claims_for("SplitFrom").next().is_none(), "no split");
    assert_eq!(quantity_of(&after, &subj("pallet")), dec(8));
    assert!(has_claim(
        &after,
        "ShippedTo",
        &[subj("pallet"), subj("customer")]
    ));
    assert!(!has_claim(&after, "InStock", &[subj("pallet")]));
    assert!(
        !has_claim(&after, "StockItem", &[subj("unused"), subj("bolt"), dec(8)]),
        "the whole branch drew nothing"
    );
    assert_eq!(
        read(&after, "LineShipped"),
        vec![vec![subj("line"), dec(8)]]
    );
}

/// Two allocations on one shipment, on two items: one pass each, in the
/// order of the allocations, not of the record. Loaded the other way
/// round, the same child goes to the same allocation.
#[test]
fn a_shipment_with_several_allocations_ships_each_in_one_order_however_the_record_was_loaded() {
    let state = ex().must_accept(
        &stock_allocation::receive(),
        vec![subj("crate"), subj("bolt"), dec(5)],
        stocked(10, 8),
    );
    let state = allocate(
        allocate(state, "alloc_b", "pallet", 3),
        "alloc_a",
        "crate",
        5,
    );
    let mut reversed = state.claims().to_vec();
    reversed.reverse();
    for state in [state, State::from_claims(reversed)] {
        let (after, passes) = ship_traced(&state, ["child"]);
        assert_eq!(
            passes,
            vec![(subj("alloc_a"), false), (subj("alloc_b"), true)],
            "alloc_a ships the crate whole, then alloc_b splits the pallet"
        );
        assert!(has_claim(
            &after,
            "SplitFrom",
            &[subj("child"), subj("pallet"), dec(3)]
        ));
        assert!(has_claim(
            &after,
            "ShippedTo",
            &[subj("crate"), subj("customer")]
        ));
        assert_eq!(quantity_of(&after, &subj("pallet")), dec(7));
        assert_eq!(
            read(&after, "LineShipped"),
            vec![vec![subj("line"), dec(8)]]
        );
    }
}

#[test]
fn cancelling_an_order_takes_every_line_and_allocation_with_it() {
    let state = ex().must_accept(
        &stock_allocation::add_line(),
        vec![subj("line_2"), subj("order"), subj("bolt"), dec(2)],
        allocate(stocked(10, 8), "alloc", "pallet", 3),
    );
    let cancelled = ex().must_accept(
        &stock_allocation::cancel_order(),
        vec![subj("order")],
        state.clone(),
    );
    for predicate in ["SalesOrder", "SalesLine", "Allocation", "Shipment"] {
        assert!(
            cancelled.claims_for(predicate).next().is_none(),
            "{predicate} is gone"
        );
    }
    assert!(has_claim(&cancelled, "InStock", &[subj("pallet")]));
    assert_eq!(quantity_of(&cancelled, &subj("pallet")), dec(10));

    // Once stock has shipped on any line, the order can no longer be
    // cancelled: the gate inside the loop refuses at that line.
    let shipped = ex().must_accept(&stock_allocation::ship(), vec![subj("shipment")], state);
    refused_by(
        &ex().must_reject(
            &stock_allocation::cancel_order(),
            vec![subj("order")],
            &shipped,
        ),
        "nothing_shipped",
    );
    // And the explanation finds that gate inside the loop.
    let transition = Transition {
        transformation_name: "cancel_order".into(),
        args: vec![subj("order")],
        actor: "clerk".into(),
        decided_at: None,
    };
    let explanation = explain(
        &stock_allocation::program(),
        &transition,
        &shipped,
        &mut subjects([]),
    );
    assert!(
        matches!(
            &explanation.verdict,
            Verdict::Rejected(Rejection::Gate(g)) if g.rule.as_deref() == Some("nothing_shipped")
        ),
        "{:?}",
        explanation.verdict
    );
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
    // Shipping with nothing allocated.
    refused_by(
        &ex().must_reject(&stock_allocation::ship(), vec![subj("shipment")], &state),
        "has_allocations",
    );
    let state = allocate(state, "alloc", "pallet", 3);
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
    let shipped = ex().must_accept(&stock_allocation::ship(), vec![subj("shipment")], state);
    // Shipping the same shipment again.
    refused_by(
        &ex().must_reject(&stock_allocation::ship(), vec![subj("shipment")], &shipped),
        "not_yet_shipped",
    );
    // Changing an allocation on a shipment that has gone.
    refused_by(
        &ex().must_reject(
            &stock_allocation::change_allocation(),
            vec![subj("alloc"), dec(2)],
            &shipped,
        ),
        "not_yet_shipped",
    );
}

#[test]
fn a_change_within_the_stock_is_admitted_and_the_read_shows_it() {
    let state = allocate(stocked(10, 8), "alloc", "pallet", 3);
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
    assert_eq!(
        read(&changed, "Allocated"),
        vec![vec![subj("pallet"), dec(5)]]
    );
    assert_eq!(
        read(&changed, "LineShipped"),
        vec![vec![subj("line"), dec(0)]]
    );
}

/// No act can move a shipped allocation's figure off what its item holds,
/// so the rule is asked directly: the record a split leaves satisfies it,
/// and the same record with the allocation changed breaks it.
#[test]
fn a_shipped_allocation_may_not_drift_from_what_its_item_holds() {
    let state = allocate(stocked(10, 8), "alloc", "pallet", 3);
    let (shipped, _) = ship_traced(&state, ["child"]);
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

/// An order closes once every line has shipped what it ordered. Short,
/// it closes only on the caller's flag and only for an actor who may;
/// a flag that is not a Boolean is an error before any gate runs.
#[test]
fn an_order_closes_complete_or_short_on_authority() {
    let state = allocate(stocked(10, 8), "alloc", "pallet", 3);
    let complete = stock_allocation::complete_order();
    let ask = |allow: bool| vec![subj("order"), EvalValue::Bool(allow)];
    // Three of eight shipped: not complete, and not closable without the flag.
    let shipped = ex().must_accept(&stock_allocation::ship(), vec![subj("shipment")], state);
    refused_by(
        &ex().must_reject(&complete, ask(false), &shipped),
        "every_line_shipped_or_allowed",
    );
    // With the flag, only an actor who may close an incomplete order.
    refused_by(
        &ex().must_reject(&complete, ask(true), &shipped),
        "override_authorised",
    );
    let granted = ex().must_accept(
        &stock_allocation::grant_close_incomplete(),
        vec![EvalValue::Subject(common::test_actor())],
        shipped.clone(),
    );
    let closed = ex().must_accept(&complete, ask(true), granted);
    assert!(has_claim(&closed, "OrderComplete", &[subj("order")]));
    refused_by(
        &ex().must_reject(
            &stock_allocation::cancel_order(),
            vec![subj("order")],
            &closed,
        ),
        "not_complete",
    );
    // The flag is declared a Boolean: a decimal there is an error, not a refusal.
    let transition = Transition {
        transformation_name: "complete_order".into(),
        args: vec![subj("order"), dec(1)],
        actor: "clerk".into(),
        decided_at: None,
    };
    let errored = propose_with_trace(
        &stock_allocation::program(),
        &transition,
        &shipped,
        &mut subjects([]),
    );
    assert!(
        matches!(
            &errored,
            TracedProposal::Errored { error: morpholog_core::EvalError::ArgumentKindMismatch { parameter, .. }, .. }
                if parameter == "allow_incomplete"
        ),
        "{errored:?}"
    );
}
