//! Static validation of a programme.

use super::*;

/// Tiny one-claim programme with a `predicate` declaration that
/// matches by default. Per-test mutations exercise each validator
/// branch.
fn one_claim_program() -> Program {
    use ir_builder::*;
    program("tiny")
        .predicates(vec![
            predicate("Echo").subject("id").decimal("amount").build(),
        ])
        .transformations(vec![transformation(
            "echo",
            params(&["id", "amount"]),
            vec![assert_("Echo", vec![var("id"), var("amount")])],
        )])
        .build()
}

#[test]
fn validate_succeeds_when_every_predicate_use_matches_declared_arity() {
    let p = one_claim_program();
    assert_eq!(p.validate(), Ok(()));
}

#[test]
fn validate_reports_undeclared_predicate_in_transformation_body() {
    use ir_builder::*;
    let mut p = one_claim_program();
    p.transformations[0]
        .body
        .push(assert_("MissingPredicate", vec![var("id")]));
    let errors = p.validate().expect_err("expected validation errors");
    assert!(
        errors.iter().any(|e| matches!(
            e,
            ValidationError::Undeclared { vocabulary: VocabularyKind::Predicate, name, .. }
                if name == "MissingPredicate"
        )),
        "expected Undeclared(Predicate, MissingPredicate); got: {errors:?}"
    );
}

#[test]
fn validate_reports_arity_mismatch_in_transformation_body() {
    use ir_builder::*;
    let mut p = one_claim_program();
    // Echo is declared with arity 2; calling with 1 arg trips
    // ArityMismatch.
    p.transformations[0].body = vec![assert_("Echo", vec![var("id")])];
    let errors = p.validate().expect_err("expected validation errors");
    assert!(
        errors.iter().any(|e| matches!(
            e,
            ValidationError::ArityMismatch {
                vocabulary: VocabularyKind::Predicate,
                name,
                expected: 2,
                actual: 1,
                ..
            } if name == "Echo"
        )),
        "expected ArityMismatch(Echo, 2, 1); got: {errors:?}"
    );
}

#[test]
fn validate_reports_arity_mismatch_in_invariant_body() {
    use ir_builder::*;
    let mut p = one_claim_program();
    p.invariants.push(invariant(
        "bad_inv", // Echo has arity 2; invariant body uses arity 3.
        claim("Echo", vec![var("x"), var("y"), var("z")]),
    ));
    let errors = p.validate().expect_err("expected validation errors");
    assert!(
        errors.iter().any(|e| matches!(
            e,
            ValidationError::ArityMismatch {
                vocabulary: VocabularyKind::Predicate,
                name: pred_name,
                expected: 2,
                actual: 3,
                context: ValidationContext::Invariant { name },
                ..
            } if pred_name == "Echo" && name == "bad_inv"
        )),
        "expected ArityMismatch in invariant context; got: {errors:?}"
    );
}

#[test]
fn validate_reports_duplicate_predicate_decl() {
    use ir_builder::*;
    let mut p = one_claim_program();
    p.predicates
        .push(predicate("Echo").subject("a").subject("b").build());
    let errors = p.validate().expect_err("expected validation errors");
    assert!(
        errors.iter().any(|e| matches!(
            e,
            ValidationError::DuplicateDecl { vocabulary: VocabularyKind::Predicate, name }
                if name == "Echo"
        )),
        "expected DuplicateDecl(Predicate, Echo); got: {errors:?}"
    );
}

#[test]
fn validate_reports_undeclared_derived_predicate() {
    use ir_builder::*;
    let mut p = one_claim_program();
    p.derived_claims.push(DerivedClaim {
        predicate: "Computed".into(),
        keys: vec!["id".into()],
        values: vec![DerivedValue {
            name: "n".into(),
            expr: term(var("id")),
        }],
        domain: claim("Echo", vec![var("id"), wildcard()]),
    });
    let errors = p.validate().expect_err("expected validation errors");
    assert!(
        errors.iter().any(|e| matches!(
            e,
            ValidationError::Undeclared { vocabulary: VocabularyKind::Predicate, name, .. }
                if name == "Computed"
        )),
        "expected Undeclared(Predicate, Computed); got: {errors:?}"
    );
}

#[test]
fn validate_reports_derived_claim_arity_mismatch_against_declared_predicate() {
    use ir_builder::*;
    let mut p = one_claim_program();
    // Declare Computed with arity 3 but build it with keys=1,
    // values=1 (total arity 2 - one short).
    p.predicates.push(
        predicate("Computed")
            .subject("id")
            .subject("category")
            .decimal("balance")
            .build(),
    );
    p.derived_claims.push(DerivedClaim {
        predicate: "Computed".into(),
        keys: vec!["id".into()],
        values: vec![DerivedValue {
            name: "balance".into(),
            expr: term(var("id")),
        }],
        domain: claim("Echo", vec![var("id"), wildcard()]),
    });
    let errors = p.validate().expect_err("expected validation errors");
    assert!(
        errors.iter().any(|e| matches!(
            e,
            ValidationError::ArityMismatch {
                vocabulary: VocabularyKind::Predicate,
                name,
                expected: 3,
                actual: 2,
                context: ValidationContext::DerivedClaim { .. },
                ..
            } if name == "Computed"
        )),
        "expected ArityMismatch on derived claim Computed; got: {errors:?}"
    );
}

/// The validator returns every error, not just the first.
#[test]
fn validate_returns_all_errors_not_just_the_first() {
    use ir_builder::*;
    let mut p = one_claim_program();
    p.transformations[0].body.push(assert_("MissingA", vec![]));
    p.transformations[0].body.push(assert_("MissingB", vec![]));
    let errors = p.validate().expect_err("expected validation errors");
    assert!(
        errors.len() >= 2,
        "expected at least 2 errors; got: {errors:?}"
    );
    let names: Vec<&str> = errors
        .iter()
        .filter_map(|e| match e {
            ValidationError::Undeclared {
                vocabulary: VocabularyKind::Predicate,
                name,
                ..
            } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert!(names.contains(&"MissingA"));
    assert!(names.contains(&"MissingB"));
}

// ============================================================
// propose_with_trace - structured per-statement diagnostic trace.
//
// The contract these pin: every statement that ran produces one
// entry (For wraps its iterations in one); rejections produce
// Completed { Rejected, trace }; kernel errors produce
// Errored { error, trace } - the trace is NOT dropped on error.
// ============================================================
