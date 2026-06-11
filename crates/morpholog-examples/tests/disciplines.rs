//! Behavioural tests for claim disciplines: declared properties of
//! claim shapes, enforced by lowering to generated invariants
//! (`unique by`, `current pointer by`, `superseded via`) or statically
//! (`append only`). Scenario programmes are inline `.morph`; the
//! strengthening pins run against the real worked examples whose
//! models the disciplines deliberately tightened.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{subj, ts};
use morpholog_core::{Outcome, Program, State, ValidationError};
use morpholog_examples::{biometric_identification_oversight as bio, laytime_demurrage as lay};
use morpholog_surface::parse_program;
use morpholog_test_support::{must_accept, must_accept_as, propose_with_test_actor};

fn parsed(source: &str) -> Program {
    let program = parse_program(source).expect("scenario programme should parse");
    program
        .validate()
        .expect("scenario programme should validate");
    program
}

fn validation_errors(source: &str) -> Vec<ValidationError> {
    parse_program(source)
        .expect("programme should parse")
        .validate()
        .expect_err("programme should fail validation")
}

// ============================================================
// The registry: every discipline tier on one small model.
// ============================================================

const REGISTRY: &str = r#"
program registry

predicate Figure(figure_id: Subject, owner: Subject, amount: Decimal)
    unique by (figure_id)
    append only

predicate CurrentFigure(owner: Subject, figure_id: Subject)
    current pointer by (owner)
    superseded via FigureSupersedes

predicate FigureSupersedes(successor: Subject, prior: Subject)

transformation record(figure_id, owner, amount):
    admit Figure(figure_id, owner, amount)
    admit CurrentFigure(owner, figure_id)

transformation correct(owner, new_id, prior_id, amount):
    require CurrentFigure(owner, prior_id)
    admit Figure(new_id, owner, amount)
    retract CurrentFigure(owner, prior_id)
    admit CurrentFigure(owner, new_id)
    admit FigureSupersedes(new_id, prior_id)

transformation fork_history(successor, prior):
    admit FigureSupersedes(successor, prior)
"#;

fn registry_with_figure() -> (Program, State) {
    let p = parsed(REGISTRY);
    let state = must_accept(
        p.transformation("record").unwrap(),
        vec![subj("f1"), subj("acme"), common::dec(100)],
        State::default(),
        &p.invariants,
        &p.definitions,
    );
    (p, state)
}

// `unique by`: a second figure under the same id, carrying different
// content, is refused - and the refusal names the generated invariant,
// the name an audit row would carry.
#[test]
fn a_duplicate_under_unique_by_is_refused_with_the_generated_name() {
    let (p, state) = registry_with_figure();
    let outcome = propose_with_test_actor(
        p.transformation("record").unwrap(),
        vec![subj("f1"), subj("acme"), common::dec(999)],
        &state,
        &p.invariants,
        &p.definitions,
    )
    .expect("kernel must not error");
    match outcome {
        Outcome::Rejected { reason } => assert!(
            reason.to_string().contains("figure_unique_by_figure_id"),
            "the generated invariant is named: {reason}"
        ),
        Outcome::Accepted { .. } => panic!("a duplicate figure id must be refused"),
    }
}

// `current pointer by`: the singleton holds through the pointer's
// whole move - the correction retracts and re-points atomically, and a
// transformation that would leave two pointers is refused.
#[test]
fn the_pointer_moves_atomically_and_its_singleton_holds() {
    let (p, state) = registry_with_figure();
    let state = must_accept(
        p.transformation("correct").unwrap(),
        vec![subj("acme"), subj("f2"), subj("f1"), common::dec(95)],
        state,
        &p.invariants,
        &p.definitions,
    );
    // A second `record` for the same owner would admit a second
    // pointer beside the moved one.
    let outcome = propose_with_test_actor(
        p.transformation("record").unwrap(),
        vec![subj("f3"), subj("acme"), common::dec(80)],
        &state,
        &p.invariants,
        &p.definitions,
    )
    .expect("kernel must not error");
    match outcome {
        Outcome::Rejected { reason } => assert!(
            reason
                .to_string()
                .contains("current_figure_unique_by_owner"),
            "the pointer singleton is named: {reason}"
        ),
        Outcome::Accepted { .. } => panic!("two current pointers must be refused"),
    }
}

// `superseded via`: the lineage can never fork - one prior, at most
// one direct successor - under the boring generated name.
#[test]
fn the_lineage_cannot_fork() {
    let (p, state) = registry_with_figure();
    let state = must_accept(
        p.transformation("correct").unwrap(),
        vec![subj("acme"), subj("f2"), subj("f1"), common::dec(95)],
        state,
        &p.invariants,
        &p.definitions,
    );
    let outcome = propose_with_test_actor(
        p.transformation("fork_history").unwrap(),
        vec![subj("f9"), subj("f1")],
        &state,
        &p.invariants,
        &p.definitions,
    )
    .expect("kernel must not error");
    match outcome {
        Outcome::Rejected { reason } => assert!(
            reason
                .to_string()
                .contains("figure_supersedes_unique_by_prior"),
            "the no-fork rule is named: {reason}"
        ),
        Outcome::Accepted { .. } => panic!("a forked lineage must be refused"),
    }
}

// ============================================================
// `append only` is a static authoring-time ban - including a retract
// buried inside a nested `for`.
// ============================================================

#[test]
fn retracting_an_append_only_predicate_is_a_validation_error() {
    let errors = validation_errors(
        r#"
program undo_attempt

predicate Record(id: Subject)
    append only

transformation undo(id):
    retract Record(id)
"#,
    );
    let rendered: Vec<String> = errors.iter().map(ToString::to_string).collect();
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::RetractsAppendOnly { .. })),
        "got {rendered:?}"
    );
    assert!(
        rendered.iter().any(|m| m.contains("never by retracting")),
        "the message teaches the correction patterns: {rendered:?}"
    );
}

#[test]
fn the_retract_ban_reaches_inside_nested_for_bodies() {
    let errors = validation_errors(
        r#"
program nested_undo

predicate Record(id: Subject)
    append only
predicate Batch(items: Collection)

transformation undo_batch(batch_id):
    bind Batch(items)
    for item in items:
        for again in items:
            retract Record(again)
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::RetractsAppendOnly { .. })),
        "got {errors:?}"
    );
}

// The lineage named by `superseded via` is append only without saying
// so: history is the doctrine's third class.
#[test]
fn retracting_a_lineage_predicate_is_refused_too() {
    let errors = validation_errors(
        r#"
program lineage_undo

predicate Pointer(owner: Subject, target: Subject)
    current pointer by (owner)
    superseded via Chain
predicate Chain(successor: Subject, prior: Subject)

transformation unlink(successor, prior):
    retract Chain(successor, prior)
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::RetractsAppendOnly { .. })),
        "got {errors:?}"
    );
}

// ============================================================
// The authoring-time error set.
// ============================================================

#[test]
fn an_unknown_field_in_a_clause_is_refused() {
    let errors = validation_errors(
        r#"
program unknown_field

predicate Item(id: Subject, label: Subject)
    unique by (serial)
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::DisciplineUnknownField { .. })),
        "got {errors:?}"
    );
}

#[test]
fn keying_every_field_is_refused_as_vacuous() {
    let errors = validation_errors(
        r#"
program vacuous

predicate Item(id: Subject, label: Subject)
    unique by (id, label)
"#,
    );
    let rendered: Vec<String> = errors.iter().map(ToString::to_string).collect();
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::DisciplineVacuousKeys { .. })),
        "got {rendered:?}"
    );
    assert!(
        rendered
            .iter()
            .any(|m| m.contains("two identical claims are already one claim")),
        "the message teaches set semantics: {rendered:?}"
    );
}

#[test]
fn a_duplicate_clause_is_refused() {
    let errors = validation_errors(
        r#"
program twice

predicate Item(id: Subject, label: Subject)
    unique by (id)
    unique by (id)
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::DisciplineDuplicateClause { .. })),
        "got {errors:?}"
    );
}

// The duplicate check compares key SETS: reordering the fields does
// not make a different commitment, and letting both through would
// generate two same-meaning invariants under different names.
#[test]
fn a_reordered_duplicate_clause_is_still_a_duplicate() {
    let errors = validation_errors(
        r#"
program reordered

predicate Span(id: Subject, opens: Date, closes: Date)
    unique by (id, opens)
    unique by (opens, id)
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::DisciplineDuplicateClause { .. })),
        "got {errors:?}"
    );
}

#[test]
fn a_pointer_cannot_be_append_only() {
    let errors = validation_errors(
        r#"
program contradiction

predicate Pointer(owner: Subject, target: Subject)
    current pointer by (owner)
    append only
"#,
    );
    assert!(
        errors.iter().any(|e| matches!(
            e,
            ValidationError::DisciplinePointerCannotBeAppendOnly { .. }
        )),
        "got {errors:?}"
    );
}

#[test]
fn an_unfit_lineage_predicate_is_refused() {
    let errors = validation_errors(
        r#"
program bad_lineage

predicate Pointer(owner: Subject, target: Subject)
    current pointer by (owner)
    superseded via Chain
predicate Chain(successor: Subject, prior: Subject, extra: Subject)
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::DisciplineLineageUnfit { .. })),
        "got {errors:?}"
    );
}

#[test]
fn superseded_via_without_a_pointer_is_refused() {
    let errors = validation_errors(
        r#"
program dangling

predicate Figure(id: Subject, amount: Decimal)
    superseded via Chain
predicate Chain(successor: Subject, prior: Subject)
"#,
    );
    assert!(
        errors.iter().any(|e| matches!(
            e,
            ValidationError::DisciplineSupersededWithoutPointer { .. }
        )),
        "got {errors:?}"
    );
}

// ============================================================
// Legibility: generated invariants are ordinary citizens of the
// guarantees and controls surfaces, under their traceable names.
// ============================================================

#[test]
fn generated_invariants_appear_in_guarantees() {
    let p = parsed(REGISTRY);
    let guarantees = morpholog_core::guarantees(&p);
    let names: Vec<&str> = guarantees.iter().map(|g| g.invariant.as_str()).collect();
    for expected in [
        "figure_unique_by_figure_id",
        "current_figure_unique_by_owner",
        "figure_supersedes_unique_by_prior",
    ] {
        assert!(
            names.contains(&expected),
            "{expected} should be a visible guarantee: {names:?}"
        );
    }
}

// ============================================================
// The strengthening pins, against the real examples: states the old
// hand-written rules tolerated and the declared disciplines refuse.
// ============================================================

// 12: one voyage, one tender event - the voyage now determines the
// WHOLE tender record. The tender gate already refuses a second NOR
// on the ordinary path, so the discipline's extra teeth only show
// against state no gate would admit: the same NOR id recorded at two
// instants. The old hand-written rule (ids agree, timestamp
// wildcarded) PASSED that state; the declared discipline refuses it.
#[test]
fn the_same_nor_at_two_instants_now_violates_the_declared_uniqueness() {
    let p = lay::program();
    let strengthened = p
        .invariants
        .iter()
        .find(|i| i.name == "nor_tendered_unique_by_voyage")
        .expect("the generated invariant exists");
    let two_instants = State::from_claims(vec![
        morpholog_core::ClaimInstance {
            predicate: "NorTendered".into(),
            args: vec![subj("nor1"), subj("v1"), ts("2026-10-24T14:00:00Z")],
        },
        morpholog_core::ClaimInstance {
            predicate: "NorTendered".into(),
            args: vec![subj("nor1"), subj("v1"), ts("2026-10-24T18:00:00Z")],
        },
    ]);
    let holds = morpholog_core::eval_invariant(strengthened, &two_instants, None, &p.definitions)
        .expect("evaluation should not error");
    assert!(
        !holds,
        "two tender instants for one voyage must violate the discipline"
    );
}

// 13: a match determines its whole decision record - the same decision
// id carrying a second outcome is refused by the discipline, not by
// luck of gate ordering.
#[test]
fn a_decision_id_with_a_second_outcome_is_refused() {
    let p = bio::program();
    // Build the admitted path to one decision via the example's own
    // fixtures: deploy, oversee, start, match, verify twice, decide.
    let state = bio_state_with_decision();
    let outcome = morpholog_core::propose(
        p.transformation("decide_on_identification").unwrap(),
        &morpholog_core::Transition {
            transformation_name: "decide_on_identification".into(),
            args: vec![
                subj("decision_1"),
                subj("match_1"),
                subj("rejected_identification"),
                ts("2026-10-12T12:00:00Z"),
            ],
            actor: morpholog_core::Subject::from("anna"),
        },
        &state,
        &p.invariants,
        &p.definitions,
    )
    .expect("kernel must not error");
    assert!(
        matches!(outcome, Outcome::Rejected { .. }),
        "the same decision id with a different outcome must be refused"
    );
}

fn bio_state_with_decision() -> State {
    let p = bio::program();
    let invs = &p.invariants;
    let defs = &p.definitions;
    let mut state = State::default();
    for (t, args, actor) in [
        ("deploy_system", vec![subj("sys"), subj("authority")], "ops"),
        (
            "place_version_in_service",
            vec![
                subj("sys"),
                subj("v1"),
                ts("2026-10-01T00:00:00Z"),
                ts("2026-12-31T00:00:00Z"),
            ],
            "ops",
        ),
        ("assign_oversight", vec![subj("anna"), subj("sys")], "ops"),
        ("assign_oversight", vec![subj("boris"), subj("sys")], "ops"),
        (
            "start_use",
            vec![
                subj("use_1"),
                subj("sys"),
                subj("v1"),
                subj("watchlist"),
                ts("2026-10-12T09:00:00Z"),
            ],
            "ops",
        ),
        (
            "record_match",
            vec![
                subj("match_1"),
                subj("use_1"),
                subj("probe_77"),
                subj("candidate_42"),
                ts("2026-10-12T09:30:00Z"),
            ],
            "sys",
        ),
        (
            "verify_match",
            vec![subj("match_1"), ts("2026-10-12T10:00:00Z")],
            "anna",
        ),
        (
            "verify_match",
            vec![subj("match_1"), ts("2026-10-12T10:30:00Z")],
            "boris",
        ),
        (
            "decide_on_identification",
            vec![
                subj("decision_1"),
                subj("match_1"),
                subj("confirmed_identification"),
                ts("2026-10-12T11:00:00Z"),
            ],
            "anna",
        ),
    ] {
        state = must_accept_as(p.transformation(t).unwrap(), args, actor, state, invs, defs);
    }
    state
}
