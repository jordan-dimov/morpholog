//! Behavioural tests for named gates: a `require` or `bind` carrying the
//! author's own identifier, so a refusal names the rule instead of quoting
//! the expression that failed.
//!
//! The property under test throughout is *stability*. Quoted expression
//! text reads well and identifies nothing - it changes the moment anyone
//! rewords the rule - so the tests here reword deliberately and check that
//! what a caller holds does not move.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{Program, RejectionReason, State, ValidationError};
use morpholog_surface::parse_program;
use morpholog_test_support::{claim_instance, must_reject, subj};

fn parsed(source: &str) -> Program {
    let program = parse_program(source).expect("scenario programme should parse");
    program
        .validate()
        .expect("scenario programme should validate");
    program
}

/// One transformation, two gates that can each refuse, and a lookup that
/// can refuse before either. The wording of the authority gate is what the
/// stability test varies.
fn programme(authority_gate: &str) -> String {
    format!(
        "program gates

predicate MayApprove(approver: Subject, doc: Subject)
predicate Submitted(doc: Subject)
predicate Approved(doc: Subject)

transformation approve(doc):
    bind the_submission: Submitted(doc)
    require approver_is_authorised: {authority_gate}
    require not_already_approved: not Approved(doc)
    admit Approved(doc)
"
    )
}

fn refuse(source: &str, pre: &State) -> RejectionReason {
    let program = parsed(source);
    let transformation = program
        .transformations
        .iter()
        .find(|t| t.name == "approve")
        .expect("the scenario declares `approve`");
    must_reject(
        transformation,
        vec![subj("doc_1")],
        pre,
        &program.invariants,
        &program.definitions,
    )
}

fn submitted() -> State {
    State::from_claims(vec![claim_instance("Submitted", &[subj("doc_1")])])
}

/// The whole point: rewording a gate leaves its identifier alone.
///
/// Both spellings below mean the same thing and refuse the same proposal,
/// but they render differently - so an assertion on the rendered text would
/// pass for one and fail for the other. That is the fragility a trial hit,
/// and the name is what removes it.
#[test]
fn rewording_a_gate_does_not_move_its_name() {
    let plain = refuse(&programme("MayApprove(actor, doc)"), &submitted());
    let reworded = refuse(
        &programme("MayApprove(actor, doc) and Submitted(doc)"),
        &submitted(),
    );

    for reason in [&plain, &reworded] {
        assert!(
            matches!(
                reason,
                RejectionReason::Require { name: Some(n), .. } if n == "approver_is_authorised"
            ),
            "the name must survive rewording; got {reason:?}"
        );
    }

    // And the rendered text really did change, so the test above is not
    // passing because nothing moved.
    assert_ne!(
        plain.to_string(),
        reworded.to_string(),
        "the two spellings must render differently, or this proves nothing"
    );
}

/// A named `bind` reports which lookup found nothing. Without the name, a
/// refusal here and a refusal at either gate are the same string shape to a
/// caller, which is how a trial's tests came to pass for the wrong reason.
#[test]
fn a_named_bind_says_which_lookup_failed() {
    // Nothing submitted, so the lookup refuses before any gate runs.
    let reason = refuse(&programme("MayApprove(actor, doc)"), &State::default());
    assert!(
        matches!(
            &reason,
            RejectionReason::BindNone { name: Some(n), .. } if n == "the_submission"
        ),
        "got {reason:?}"
    );
}

/// The acceptance side: a gate with no name still refuses, and still
/// reports its rendered text exactly as it always did. Naming is optional,
/// and an unnamed programme must be unaffected by the feature existing.
#[test]
fn an_unnamed_gate_still_reports_its_rendered_text() {
    let source = "program unnamed

predicate MayApprove(approver: Subject, doc: Subject)
predicate Approved(doc: Subject)

transformation approve(doc):
    require MayApprove(actor, doc)
    admit Approved(doc)
";
    let reason = refuse(source, &State::default());
    assert!(
        matches!(&reason, RejectionReason::Require { name: None, .. }),
        "got {reason:?}"
    );
    assert_eq!(
        reason.to_string(),
        "require failed: MayApprove(actor, doc) did not hold over pre-state"
    );
}

/// A name identifies one rule, so two rules cannot share one inside a
/// transformation - a refusal would be ambiguous, which is the defect the
/// name exists to fix.
#[test]
fn two_rules_in_one_transformation_cannot_share_a_name() {
    let source = "program dup

predicate MayApprove(approver: Subject, doc: Subject)
predicate Approved(doc: Subject)

transformation approve(doc):
    require authorised: MayApprove(actor, doc)
    require authorised: not Approved(doc)
    admit Approved(doc)
";
    let program = parse_program(source).expect("parses - the clash is semantic");
    let errs = program.validate().expect_err("must not validate");
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::DuplicateRuleName { name, .. } if name == "authorised"
        )),
        "got {errs:?}"
    );
}

/// The acceptance side of that check, and the reason it is scoped to one
/// transformation: two acts legitimately carry the same gate verbatim, and
/// programme-uniqueness would force meaningless suffixes on them.
#[test]
fn two_transformations_may_share_a_rule_name() {
    let source = "program shared

predicate MayApprove(approver: Subject, doc: Subject)
predicate Approved(doc: Subject)
predicate Rejected(doc: Subject)

transformation approve(doc):
    require authorised: MayApprove(actor, doc)
    admit Approved(doc)

transformation reject(doc):
    require authorised: MayApprove(actor, doc)
    admit Rejected(doc)
";
    let program = parse_program(source).expect("parses");
    program
        .validate()
        .expect("the same gate in two acts is legitimate");
}

/// A name inside a `for` body competes for the same names, so the check
/// descends: a duplicate hidden one level down is still a duplicate.
#[test]
fn a_name_inside_a_for_body_is_not_a_hiding_place() {
    let source = "program nested

predicate MayApprove(approver: Subject, doc: Subject)
predicate Approved(doc: Subject)

transformation approve_many(docs):
    require authorised: MayApprove(actor, actor)
    for d in docs:
        require authorised: MayApprove(actor, d)
        admit Approved(d)
";
    let program = parse_program(source).expect("parses");
    let errs = program.validate().expect_err("must not validate");
    assert!(
        errs.iter().any(|e| matches!(
            e,
            ValidationError::DuplicateRuleName { name, .. } if name == "authorised"
        )),
        "got {errs:?}"
    );
}

/// Formatting must not silently downgrade a stable identifier back to
/// prose. If the formatter dropped the name, a programme would round-trip
/// into one whose refusals identify nothing.
#[test]
fn names_survive_format_and_reparse() {
    let source = programme("MayApprove(actor, doc)");
    let once = parsed(&source);
    let formatted = morpholog_core::format::format_program(&once);
    let twice = parse_program(&formatted).expect("formatted output must reparse");
    assert_eq!(once, twice, "formatted:\n{formatted}");
    assert!(
        formatted.contains("require approver_is_authorised:")
            && formatted.contains("bind the_submission:"),
        "formatted:\n{formatted}"
    );
}
