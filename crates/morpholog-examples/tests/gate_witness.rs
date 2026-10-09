//! A refused gate carries the bindings its failing part was judged under,
//! as an invariant refusal carries the bindings at the violation. One
//! descent names the failing part and the bindings, so the trace, the
//! refusal and a dry run (`explain`) say the same values.
//!
//! What a gate witness is: the parameters, what earlier statements bound,
//! and what the conjuncts before the failing one bound under the first
//! surviving context. It is the context the diagnosis was made in, not
//! every way the gate could have held. The actor is not among them.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{dec, subj, subjects};
use morpholog_core::{
    EvalValue, Outcome, Rejection, RejectionReason, State, TraceEntry, TracedProposal, Transition,
    Verdict, WitnessBinding,
};
use morpholog_surface::parse_program;
use morpholog_test_support::{claim_instance, explain, propose, propose_with_trace};

const DESK: &str = r#"
program desk
predicate Mandate(trader: Subject, instrument: Subject, max_qty: Decimal)
predicate Price(instrument: Subject, px: Decimal)
predicate Approved(instrument: Subject, max_qty: Decimal)
predicate Order(order: Subject, instrument: Subject, qty: Decimal)
define priced_within(instrument, qty):
    Price(instrument, px) and qty <= px
transformation place(instrument, qty):
    require within_mandate: Mandate(actor, instrument, max_qty) and qty <= max_qty
    let order = new Subject()
    admit Order(order, instrument, qty)
transformation quote(instrument, qty):
    require priced: priced_within(instrument, qty)
    let order = new Subject()
    admit Order(order, instrument, qty)
transformation reprice(instrument, qty):
    bind current: Price(instrument, px)
    let order = new Subject()
    admit Order(order, instrument, qty)
transformation place_approved(instrument, qty):
    require approved: Mandate(actor, instrument, max_qty) and Approved(instrument, max_qty)
    let order = new Subject()
    admit Order(order, instrument, qty)
"#;

fn desk() -> morpholog_core::Program {
    let p = parse_program(DESK).expect("parses");
    p.validate().expect("validates");
    p
}

fn transition(name: &str, args: Vec<EvalValue>) -> Transition {
    Transition {
        transformation_name: name.into(),
        args,
        actor: "trader_t".into(),
    }
}

fn refusal(state: &State, t: &Transition) -> RejectionReason {
    match propose(&desk(), t, state, &mut subjects(["o1"])).expect("no kernel error") {
        Outcome::Rejected { reason } => reason,
        Outcome::Accepted { .. } => panic!("expected a refusal"),
    }
}

fn vars(witness: &[WitnessBinding]) -> Vec<&str> {
    witness.iter().map(|w| w.var.as_str()).collect()
}

fn value<'a>(witness: &'a [WitnessBinding], var: &str) -> &'a EvalValue {
    &witness
        .iter()
        .find(|w| w.var.as_str() == var)
        .unwrap()
        .value
}

#[test]
fn a_failing_conjunct_carries_what_the_conjuncts_before_it_bound() {
    let state = State::from_claims(vec![claim_instance(
        "Mandate",
        &[subj("trader_t"), subj("power_q1"), dec(50)],
    )]);
    let reason = refusal(
        &state,
        &transition("place", vec![subj("power_q1"), dec(60)]),
    );
    let RejectionReason::Require { name, witness, .. } = &reason else {
        panic!("{reason:?}");
    };
    assert_eq!(name.as_ref().unwrap().as_str(), "within_mandate");
    assert_eq!(
        vars(witness),
        ["instrument", "max_qty", "qty"],
        "sorted, actor apart"
    );
    assert_eq!(value(witness, "max_qty"), &dec(50));
    assert_eq!(value(witness, "qty"), &dec(60));
}

#[test]
fn a_gate_failing_on_its_first_claim_carries_the_bindings_it_started_from() {
    let reason = refusal(
        &State::default(),
        &transition("place", vec![subj("power_q1"), dec(60)]),
    );
    let RejectionReason::Require { witness, .. } = &reason else {
        panic!("{reason:?}");
    };
    assert_eq!(vars(witness), ["instrument", "qty"]);
}

#[test]
fn inside_a_definition_the_witness_names_the_bodys_parameters() {
    let state = State::from_claims(vec![claim_instance("Price", &[subj("power_q1"), dec(40)])]);
    let reason = refusal(
        &state,
        &transition("quote", vec![subj("power_q1"), dec(60)]),
    );
    let RejectionReason::Require { witness, .. } = &reason else {
        panic!("{reason:?}");
    };
    assert_eq!(vars(witness), ["instrument", "px", "qty"]);
    assert_eq!(value(witness, "px"), &dec(40));
}

#[test]
fn a_bind_matching_nothing_carries_the_bindings_it_started_from() {
    let reason = refusal(
        &State::default(),
        &transition("reprice", vec![subj("power_q1"), dec(1)]),
    );
    let RejectionReason::BindNone { name, witness, .. } = &reason else {
        panic!("{reason:?}");
    };
    assert_eq!(name.as_ref().unwrap().as_str(), "current");
    assert_eq!(vars(witness), ["instrument", "qty"]);
}

/// Two mandates survive the first conjunct and both fail the second: the
/// walk diagnoses under the first surviving context, and the witness,
/// the trace's failing part and the refusal agree on that one context.
#[test]
fn with_several_surviving_contexts_the_witness_and_the_trace_blame_the_same_one() {
    let state = State::from_claims(vec![
        claim_instance("Mandate", &[subj("trader_t"), subj("power_q1"), dec(50)]),
        claim_instance("Mandate", &[subj("trader_t"), subj("power_q1"), dec(70)]),
    ]);
    let t = transition("place", vec![subj("power_q1"), dec(80)]);
    let TracedProposal::Completed { outcome, trace } =
        propose_with_trace(&desk(), &t, &state, &mut subjects(["o1"]))
    else {
        panic!("kernel error");
    };
    let Outcome::Rejected {
        reason: RejectionReason::Require { witness, .. },
    } = &outcome
    else {
        panic!("{outcome:?}");
    };
    assert_eq!(
        witness
            .iter()
            .filter(|w| w.var.as_str() == "max_qty")
            .count(),
        1,
        "one context, not both: {witness:?}"
    );
    let blamed = value(witness, "max_qty");
    assert!([dec(50), dec(70)].contains(blamed), "{blamed:?}");
    let failing = trace.iter().find_map(|e| match e {
        TraceEntry::Require {
            outcome:
                morpholog_core::RequireOutcome::Rejected {
                    failing_sub_expression,
                    directly_missing_claims,
                    ..
                },
            ..
        } => Some((
            failing_sub_expression.clone(),
            directly_missing_claims.len(),
        )),
        _ => None,
    });
    assert_eq!(
        failing,
        Some((Some("qty <= max_qty".to_string()), 0)),
        "the comparison is blamed and no claim is missing"
    );
    // The same state and the same proposal blame the same context again.
    assert_eq!(
        refusal(&state, &t),
        outcome_reason(&outcome),
        "deterministic across runs"
    );
}

fn outcome_reason(outcome: &Outcome) -> RejectionReason {
    match outcome {
        Outcome::Rejected { reason } => reason.clone(),
        Outcome::Accepted { .. } => panic!("accepted"),
    }
}

/// A dry run says the values a refusal would, from the same pre-state
/// and the same subject sequence: no second walk, no second decision.
#[test]
fn explain_carries_the_witness_a_refusal_would() {
    let state = State::from_claims(vec![claim_instance(
        "Mandate",
        &[subj("trader_t"), subj("power_q1"), dec(50)],
    )]);
    let t = transition("place", vec![subj("power_q1"), dec(60)]);
    let explanation = explain(&desk(), &t, &state, &mut subjects(["o1"]));
    let Verdict::Rejected(Rejection::Gate(gate)) = &explanation.verdict else {
        panic!("{explanation:?}");
    };
    let RejectionReason::Require { witness, .. } = refusal(&state, &t) else {
        panic!("not a gate refusal");
    };
    assert_eq!(gate.witness, witness);
    assert_eq!(gate.rule.as_deref(), Some("within_mandate"));
}

/// Two mandates survive the first conjunct and a positive claim kills
/// both: the missing-claim walker, implemented apart from the failure
/// walk, resolves the claim under the same context the witness names.
#[test]
fn the_missing_claim_is_resolved_under_the_context_the_witness_names() {
    let state = State::from_claims(vec![
        claim_instance("Mandate", &[subj("trader_t"), subj("power_q1"), dec(50)]),
        claim_instance("Mandate", &[subj("trader_t"), subj("power_q1"), dec(70)]),
    ]);
    let t = transition("place_approved", vec![subj("power_q1"), dec(10)]);
    let TracedProposal::Completed { outcome, trace } =
        propose_with_trace(&desk(), &t, &state, &mut subjects(["o1"]))
    else {
        panic!("kernel error");
    };
    let Outcome::Rejected {
        reason: RejectionReason::Require { witness, .. },
    } = &outcome
    else {
        panic!("{outcome:?}");
    };
    let blamed = value(witness, "max_qty");
    let EvalValue::Decimal(blamed_qty) = blamed else {
        panic!("{blamed:?}");
    };
    let other = if *blamed_qty == 50.into() { "70" } else { "50" };
    let missing = trace
        .iter()
        .find_map(|e| match e {
            TraceEntry::Require {
                outcome:
                    morpholog_core::RequireOutcome::Rejected {
                        directly_missing_claims,
                        ..
                    },
                ..
            } => Some(directly_missing_claims.clone()),
            _ => None,
        })
        .expect("the refusing gate is in the trace");
    assert_eq!(missing.len(), 1, "{missing:?}");
    assert_eq!(missing[0].predicate.as_str(), "Approved");
    assert!(
        missing[0].rendered.contains(&blamed_qty.to_string())
            && !missing[0].rendered.contains(other),
        "the missing claim is resolved under the witness's context: {} vs {blamed_qty}",
        missing[0].rendered
    );
}

// ============================================================
// The two values the failing comparison compared
// ============================================================

use morpholog_core::Compared;

fn compared_of(reason: &RejectionReason) -> Option<Compared> {
    match reason {
        RejectionReason::Require { compared, .. }
        | RejectionReason::BindNone { compared, .. }
        | RejectionReason::Invariant { compared, .. } => compared.clone(),
    }
}

/// A gate failing at a comparison carries the two values it compared,
/// under the bindings the witness names, and the operator as the source
/// spells it.
#[test]
fn a_failing_comparison_carries_its_two_values() {
    let state = State::from_claims(vec![claim_instance(
        "Mandate",
        &[subj("trader_t"), subj("power_q1"), dec(50)],
    )]);
    let reason = refusal(
        &state,
        &transition("place", vec![subj("power_q1"), dec(60)]),
    );
    assert_eq!(
        compared_of(&reason),
        Some(Compared {
            op: "<=".to_string(),
            left: dec(60),
            right: dec(50),
        })
    );
}

/// The whole gate is the comparison: nothing more specific to blame, so
/// no failing sub-expression, and still the two values.
#[test]
fn a_top_level_comparison_carries_its_values_with_no_failing_sub_expression() {
    let source = DESK.replace(
        "transformation reprice(instrument, qty):\n    bind current: Price(instrument, px)",
        "transformation reprice(instrument, qty):\n    require small: qty <= 5",
    );
    let p = parse_program(&source).expect("parses");
    p.validate().expect("validates");
    let t = transition("reprice", vec![subj("power_q1"), dec(9)]);
    let TracedProposal::Completed { outcome, trace } =
        propose_with_trace(&p, &t, &State::default(), &mut subjects(["o1"]))
    else {
        panic!("kernel error");
    };
    let Outcome::Rejected { reason } = &outcome else {
        panic!("{outcome:?}");
    };
    assert_eq!(
        compared_of(reason),
        Some(Compared {
            op: "<=".to_string(),
            left: dec(9),
            right: dec(5),
        })
    );
    let failing = trace.iter().find_map(|e| match e {
        TraceEntry::Require {
            outcome:
                morpholog_core::RequireOutcome::Rejected {
                    failing_sub_expression,
                    ..
                },
            ..
        } => Some(failing_sub_expression.clone()),
        _ => None,
    });
    assert_eq!(failing, Some(None), "a leaf has nothing more specific");
}

/// Inside a definition the values are the body's, under its frame.
#[test]
fn a_comparison_inside_a_definition_carries_the_bodys_values() {
    let state = State::from_claims(vec![claim_instance("Price", &[subj("power_q1"), dec(40)])]);
    let reason = refusal(
        &state,
        &transition("quote", vec![subj("power_q1"), dec(60)]),
    );
    assert_eq!(
        compared_of(&reason),
        Some(Compared {
            op: "<=".to_string(),
            left: dec(60),
            right: dec(40),
        })
    );
}

/// A gate failing on a missing claim blames no comparison.
#[test]
fn a_missing_claim_carries_no_comparison() {
    let reason = refusal(
        &State::default(),
        &transition("place", vec![subj("power_q1"), dec(60)]),
    );
    assert_eq!(compared_of(&reason), None);
}

const LIMITS: &str = r#"
program limits
predicate Entry(entry: Subject, desk: Subject, risk: Decimal)
    unique by (entry)
    append only
predicate Limit(desk: Subject, cap: Decimal)
    unique by (desk)
predicate Booked(entry: Subject, on: Date)
    unique by (entry)
transformation record(e, d, r):
    admit Entry(e, d, r)
transformation book(e, on):
    admit Booked(e, on)
transformation cap(d, c):
    admit Limit(d, c)
invariant risk_within_limit:
    Entry(e, desk, risk) and Limit(desk, cap) implies risk <= cap
invariant booked_this_year:
    Booked(e, on) implies on on_or_before @2026-12-31
invariant desks_named_alike:
    Entry(e, desk, _) and Limit(d, _) implies desk = d
"#;

fn limits() -> morpholog_core::Program {
    let p = parse_program(LIMITS).expect("parses");
    p.validate().expect("validates");
    p
}

fn limits_refusal(state: &State, name: &str, args: Vec<EvalValue>) -> RejectionReason {
    let t = transition(name, args);
    match propose(&limits(), &t, state, &mut subjects(["x"])).expect("no kernel error") {
        Outcome::Rejected { reason } => reason,
        Outcome::Accepted { .. } => panic!("expected a refusal"),
    }
}

/// An invariant refusal carries the comparison beside its witness, from
/// the one failing case the witness names: with a pre-existing breach
/// on another desk, admission's bounded check never reports it.
#[test]
fn an_invariant_refusal_carries_the_touched_cases_comparison() {
    let state = State::from_claims(vec![
        claim_instance("Limit", &[subj("desk_a"), dec(10)]),
        claim_instance("Limit", &[subj("desk_b"), dec(20)]),
        // desk_a already breaches: history admission does not reach.
        claim_instance("Entry", &[subj("old"), subj("desk_a"), dec(99)]),
    ]);
    let reason = limits_refusal(&state, "record", vec![subj("new"), subj("desk_b"), dec(25)]);
    let RejectionReason::Invariant {
        name,
        witness,
        compared,
        ..
    } = &reason
    else {
        panic!("{reason:?}");
    };
    assert_eq!(name.as_str(), "risk_within_limit");
    assert_eq!(value(witness, "e"), &subj("new"));
    assert_eq!(
        value(witness, "cap"),
        &dec(20),
        "desk_b's cap, not desk_a's"
    );
    assert_eq!(
        *compared,
        Some(Compared {
            op: "<=".to_string(),
            left: dec(25),
            right: dec(20),
        })
    );
}

/// A date comparison carries the operator the source spells and two
/// dates; an equality carries `=` and two subjects.
#[test]
fn a_date_comparison_and_an_equality_carry_their_own_operators() {
    use morpholog_test_support::date;
    let reason = limits_refusal(
        &State::default(),
        "book",
        vec![subj("e1"), date("2027-01-05")],
    );
    assert_eq!(
        compared_of(&reason),
        Some(Compared {
            op: "on_or_before".to_string(),
            left: date("2027-01-05"),
            right: date("2026-12-31"),
        })
    );
    let state = State::from_claims(vec![claim_instance("Limit", &[subj("desk_b"), dec(20)])]);
    let reason = limits_refusal(&state, "record", vec![subj("e1"), subj("desk_a"), dec(1)]);
    let RejectionReason::Invariant { name, compared, .. } = &reason else {
        panic!("{reason:?}");
    };
    assert_eq!(name.as_str(), "desks_named_alike");
    assert_eq!(
        *compared,
        Some(Compared {
            op: "=".to_string(),
            left: subj("desk_a"),
            right: subj("desk_b"),
        })
    );
}

/// A dry run says the values a refusal would, for a gate and for a rule.
#[test]
fn explain_carries_the_comparison_a_refusal_would() {
    let state = State::from_claims(vec![claim_instance(
        "Mandate",
        &[subj("trader_t"), subj("power_q1"), dec(50)],
    )]);
    let t = transition("place", vec![subj("power_q1"), dec(60)]);
    let explanation = explain(&desk(), &t, &state, &mut subjects(["o1"]));
    let Verdict::Rejected(Rejection::Gate(gate)) = &explanation.verdict else {
        panic!("{explanation:?}");
    };
    assert_eq!(gate.compared, compared_of(&refusal(&state, &t)));
    assert!(
        explanation
            .render()
            .contains("compared: 60 <= 50 did not hold")
    );

    let state = State::from_claims(vec![claim_instance("Limit", &[subj("desk_b"), dec(20)])]);
    let t = transition("record", vec![subj("new"), subj("desk_b"), dec(25)]);
    let explanation = explain(&limits(), &t, &state, &mut subjects(["x"]));
    let Verdict::Rejected(Rejection::Invariant(inv)) = &explanation.verdict else {
        panic!("{explanation:?}");
    };
    assert_eq!(
        inv.compared,
        compared_of(&limits_refusal(
            &state,
            "record",
            vec![subj("new"), subj("desk_b"), dec(25)]
        ))
    );
    assert!(inv.compared.is_some());
}
