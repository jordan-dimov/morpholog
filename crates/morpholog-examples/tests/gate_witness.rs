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
