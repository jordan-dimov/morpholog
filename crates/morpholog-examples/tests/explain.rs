//! Integration tests for the explanation engine (`morpholog_core::explain`).
//!
//! The flagship cases run against the real `approval_controls` example -
//! a rejected `approve_document` is the canonical "Morpholog explains
//! legitimacy" artifact: it names the directly-missing authority claim
//! and the transformation that could supply it. The remaining cases pin
//! the v0 boundary: comparator failures and present blockers carry an
//! empty missing-claims list, invariant violations and kernel errors use
//! their own rejection shapes, and the JSON and prose surfaces are
//! deterministic.
//!
//! Test layers: the flagship and boundary cases run against the real
//! `approval_controls` example; small bespoke scenarios (a sanctions
//! blocker, a missing supplier, an invariant violation) are authored as
//! inline `.morph` and parsed, so they read as models rather than IR
//! struct-construction. The "transformation minus one statement"
//! invariant-teeth tests (chess, insurance) are a different layer -
//! deliberately adversarial, constructing shapes a correct programme
//! never would, where the Rust IR builders are the right tool.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{claim_instance, dec, subj};
use morpholog_core::{GateKind, Rejection, State, Subject, Transition, Verdict, explain};
use morpholog_examples::approval_controls;
use morpholog_surface::parse_program;

fn transition(name: &str, args: Vec<morpholog_core::EvalValue>, actor: &str) -> Transition {
    Transition {
        transformation_name: name.to_string(),
        args,
        actor: Subject::from(actor),
    }
}

// ============================================================
// Flagship: directly-missing authority claim + candidate supplier,
// against the real approval_controls example.
// ============================================================

#[test]
fn rejected_approve_names_the_missing_authority_claim_and_its_supplier() {
    let program = approval_controls::program();
    // No MayApprove granted: approve_document's require fails.
    let state = State::default();
    let t = transition(
        "approve_document",
        vec![subj("doc-42"), subj("contract")],
        "alice",
    );

    let explanation = explain(&program, &t, &state);

    let Verdict::Rejected(Rejection::Gate(gate)) = &explanation.verdict else {
        panic!("expected a gate rejection, got {:?}", explanation.verdict);
    };
    assert_eq!(gate.statement_kind, GateKind::Require);
    // The gate renders the rule as written; the missing claim renders the
    // concrete absent instance under the proposing actor and arguments.
    assert_eq!(gate.gate, "MayApprove(actor, doc_type)");
    assert_eq!(gate.directly_missing_claims.len(), 1);
    let missing = &gate.directly_missing_claims[0];
    assert_eq!(missing.predicate, "MayApprove");
    assert_eq!(missing.rendered, "MayApprove(alice, contract)");
    assert_eq!(
        missing.candidate_supplier_transformations,
        vec!["grant_approval_authority"],
    );
}

#[test]
fn rendered_rejection_is_the_auditor_facing_artifact() {
    let program = approval_controls::program();
    let t = transition(
        "approve_document",
        vec![subj("doc-42"), subj("contract")],
        "alice",
    );
    let explanation = explain(&program, &t, &State::default());

    let expected = "\
Rejected: approve_document(doc-42, contract) proposed by alice

Gate not satisfied:
  MayApprove(actor, doc_type)

Directly missing claims:
  - MayApprove(alice, contract)
      candidate supplier transformations:
        - grant_approval_authority";

    assert_eq!(explanation.render(), expected);
    // Deterministic: the same object renders identically every time.
    assert_eq!(explanation.render(), explanation.render());
}

#[test]
fn admissible_transition_explains_as_admissible() {
    let program = approval_controls::program();
    // Grant authority first, then approve: admissible.
    let state = State::from_claims(vec![claim_instance(
        "MayApprove",
        &[subj("alice"), subj("contract")],
    )]);
    let t = transition(
        "approve_document",
        vec![subj("doc-42"), subj("contract")],
        "alice",
    );

    let explanation = explain(&program, &t, &state);

    assert_eq!(explanation.verdict, Verdict::Admissible);
    assert_eq!(
        explanation.render(),
        "Admissible: approve_document(doc-42, contract) proposed by alice",
    );
}

// ============================================================
// The v0 boundary: comparator failures and present blockers are
// faithful rejections with NO directly-missing claim.
// ============================================================

#[test]
fn comparator_failure_carries_no_directly_missing_claim() {
    let program = approval_controls::program();
    // Authority granted at limit 100; propose 500 (over limit). The And
    // gate's chain-killer is the `<=` comparator, not a positive claim.
    let state = State::from_claims(vec![claim_instance(
        "ApprovalLimit",
        &[subj("alice"), subj("contract"), dec(100)],
    )]);
    let t = transition(
        "approve_within_limit",
        vec![subj("doc-9"), subj("contract"), dec(500)],
        "alice",
    );

    let explanation = explain(&program, &t, &state);

    let Verdict::Rejected(Rejection::Gate(gate)) = &explanation.verdict else {
        panic!("expected a gate rejection, got {:?}", explanation.verdict);
    };
    assert!(
        gate.directly_missing_claims.is_empty(),
        "a comparator failure is not a missing claim: {:?}",
        gate.directly_missing_claims,
    );
}

#[test]
fn present_blocker_carries_no_directly_missing_claim() {
    // require not Sanctioned(customer); Sanctioned(alice) holds, so the
    // gate fails on a present blocker - which v0 reports as a faithful
    // rejection, not a missing claim.
    let program = parse_program(
        "program blocker_demo

predicate Sanctioned(customer: Subject)
predicate Onboarded(customer: Subject)

transformation onboard(customer):
    require not Sanctioned(customer)
    admit Onboarded(customer)
",
    )
    .expect("blocker_demo must parse");
    let state = State::from_claims(vec![claim_instance("Sanctioned", &[subj("alice")])]);
    let t = transition("onboard", vec![subj("alice")], "officer");

    let explanation = explain(&program, &t, &state);

    let Verdict::Rejected(Rejection::Gate(gate)) = &explanation.verdict else {
        panic!("expected a gate rejection, got {:?}", explanation.verdict);
    };
    assert!(
        gate.directly_missing_claims.is_empty(),
        "a present blocker is not a missing claim: {:?}",
        gate.directly_missing_claims,
    );
}

// ============================================================
// A directly-missing claim that no transformation can supply:
// the explanation still renders cleanly, with an honest empty list.
// ============================================================

#[test]
fn missing_claim_with_no_supplier_renders_cleanly() {
    // `issue` requires `Accredited(actor)`, which no transformation in the
    // model asserts - so the candidate-supplier list is honestly empty.
    let program = parse_program(
        "program no_supplier_demo

predicate Accredited(who: Subject)
predicate Certificate(cert: Subject)

transformation issue(cert):
    require Accredited(actor)
    admit Certificate(cert)
",
    )
    .expect("no_supplier_demo must parse");
    let t = transition("issue", vec![subj("cert-1")], "officer");

    let explanation = explain(&program, &t, &State::default());

    let Verdict::Rejected(Rejection::Gate(gate)) = &explanation.verdict else {
        panic!("expected a gate rejection, got {:?}", explanation.verdict);
    };
    assert_eq!(gate.directly_missing_claims.len(), 1);
    let missing = &gate.directly_missing_claims[0];
    assert_eq!(missing.predicate, "Accredited");
    assert_eq!(missing.rendered, "Accredited(officer)");
    assert!(missing.candidate_supplier_transformations.is_empty());
    // The renderer states the absence rather than omitting the line.
    assert!(
        explanation
            .render()
            .contains("(no transformation in this model asserts Accredited)"),
        "render was:\n{}",
        explanation.render(),
    );
}

// ============================================================
// Invariant violation and kernel error use their own rejection
// shapes, never a fabricated gate explanation.
// ============================================================

#[test]
fn invariant_violation_uses_the_invariant_rejection_shape() {
    // Every Flagged customer must have a Permit. `flag` asserts Flagged
    // without a Permit, so the candidate state violates the invariant.
    let program = parse_program(
        "program invariant_demo

predicate Flagged(customer: Subject)
predicate Permit(customer: Subject)

invariant flagged_requires_permit:
    forall c in Flagged(c): Permit(c)

transformation flag(customer):
    admit Flagged(customer)
",
    )
    .expect("invariant_demo must parse");
    let t = transition("flag", vec![subj("alice")], "officer");

    let explanation = explain(&program, &t, &State::default());

    let Verdict::Rejected(Rejection::Invariant(inv)) = &explanation.verdict else {
        panic!(
            "expected an invariant rejection, got {:?}",
            explanation.verdict
        );
    };
    assert_eq!(inv.name, "flagged_requires_permit");
    assert!(explanation.render().contains("Would violate invariant"));
}

#[test]
fn kernel_error_uses_the_error_rejection_shape() {
    let program = approval_controls::program();
    // approve_document expects two arguments; one is a kernel-level error,
    // not a business rejection - so it must not become a fake gate.
    let t = transition("approve_document", vec![subj("doc-1")], "alice");

    let explanation = explain(&program, &t, &State::default());

    let Verdict::Rejected(Rejection::Error(err)) = &explanation.verdict else {
        panic!("expected an error rejection, got {:?}", explanation.verdict);
    };
    assert!(
        err.message.contains("expects 2 args"),
        "message was: {}",
        err.message,
    );
}

#[test]
fn unknown_transformation_is_an_error_rejection() {
    let program = approval_controls::program();
    let t = transition("no_such_transformation", vec![], "alice");

    let explanation = explain(&program, &t, &State::default());

    let Verdict::Rejected(Rejection::Error(err)) = &explanation.verdict else {
        panic!("expected an error rejection, got {:?}", explanation.verdict);
    };
    assert!(err.message.contains("no transformation named"));
}

// ============================================================
// JSON: the explanation is now an external surface; pin its shape.
// ============================================================

#[test]
fn explanation_json_is_stable_and_round_trips() {
    let program = approval_controls::program();
    let t = transition(
        "approve_document",
        vec![subj("doc-42"), subj("contract")],
        "alice",
    );
    let explanation = explain(&program, &t, &State::default());

    let json = serde_json::to_string(&explanation).unwrap();
    // Key structure of the external surface.
    assert!(json.contains(r#""kind":"gate""#), "json: {json}");
    assert!(
        json.contains(r#""statement_kind":"require""#),
        "json: {json}"
    );
    assert!(
        json.contains(r#""candidate_supplier_transformations":["grant_approval_authority"]"#),
        "json: {json}",
    );
    // Round-trips back to an identical structured object.
    let parsed: morpholog_core::Explanation = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, explanation);
}
