//! Adversarial pins for claim disciplines, at the IR level the parser
//! never produces: lowering idempotence and ordering, and the loud
//! failure for hand-built IR that declares a discipline but skips
//! `lower_disciplines` (which would otherwise carry a silently
//! unenforced commitment).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::ir_builder::{
    claim, invariant, params, predicate, program, transformation, var,
};
use morpholog_core::{Discipline, InvariantOrigin, ValidationError, lower_disciplines};

fn disciplined_program() -> morpholog_core::Program {
    program("registry")
        .predicates(vec![
            predicate("Figure")
                .subject("figure_id")
                .subject("owner")
                .disciplines(vec![Discipline::UniqueBy {
                    fields: vec!["figure_id".to_string()],
                }])
                .build(),
        ])
        .invariants(vec![invariant(
            "owners_exist",
            claim("Figure", vec![var("f"), var("o")]),
        )])
        .transformations(vec![transformation(
            "record",
            params(&["figure_id", "owner"]),
            vec![morpholog_core::ir_builder::assert_(
                "Figure",
                vec![var("figure_id"), var("owner")],
            )],
        )])
        .build()
}

// Lowering is idempotent, and generated invariants sit FIRST: a
// discipline is a precondition of sense for the authored rules, so a
// proposal violating both is refused with the root cause named.
#[test]
fn lowering_is_idempotent_and_generated_invariants_check_first() {
    let mut once = disciplined_program();
    lower_disciplines(&mut once);
    let mut twice = once.clone();
    lower_disciplines(&mut twice);
    assert_eq!(once, twice, "a second lowering changes nothing");

    assert_eq!(once.invariants[0].name, "figure_unique_by_figure_id");
    assert_eq!(once.invariants[0].origin, InvariantOrigin::Discipline);
    assert_eq!(once.invariants[1].name, "owners_exist");
    assert_eq!(once.invariants[1].origin, InvariantOrigin::Authored);
}

// Hand-built IR that declares a discipline but never lowers it fails
// validation loudly, with the guidance to run `lower_disciplines` -
// never a programme whose declared commitment is silently unenforced.
#[test]
fn a_declared_discipline_without_its_lowering_fails_validation() {
    let unlowered = disciplined_program();
    let errors = unlowered
        .validate()
        .expect_err("an unlowered discipline must not validate");
    let rendered: Vec<String> = errors.iter().map(ToString::to_string).collect();
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::DisciplineNotLowered { .. })),
        "got {rendered:?}"
    );
    assert!(
        rendered.iter().any(|m| m.contains("lower_disciplines")),
        "the message names the fix: {rendered:?}"
    );

    let mut lowered = unlowered;
    lower_disciplines(&mut lowered);
    lowered.validate().expect("the lowered programme validates");
}

// A duplicate clause is a validation error - but even before
// validation runs, the lowering itself never lets the invalid shape
// leak into generated IR: one clause's worth of invariants, not two.
#[test]
fn a_duplicate_clause_lowers_once_even_on_the_invalid_programme() {
    let mut p = program("doubled")
        .predicates(vec![
            predicate("Figure")
                .subject("figure_id")
                .subject("owner")
                .disciplines(vec![
                    Discipline::UniqueBy {
                        fields: vec!["figure_id".to_string()],
                    },
                    Discipline::UniqueBy {
                        fields: vec!["figure_id".to_string()],
                    },
                ])
                .build(),
        ])
        .build();
    lower_disciplines(&mut p);
    let generated: Vec<_> = p
        .invariants
        .iter()
        .filter(|i| i.origin == InvariantOrigin::Discipline)
        .collect();
    assert_eq!(generated.len(), 1, "the duplicate clause lowers once");
    let errors = p.validate().expect_err("the duplicate clause still errors");
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::DisciplineDuplicateClause { .. })),
        "got {errors:?}"
    );
}
