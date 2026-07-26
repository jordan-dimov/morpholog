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
use morpholog_test_support::{must_accept, must_accept_as, must_reject};

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

/// Build a CompiledProgram for the analysis entry points, which now
/// take `&CompiledProgram`.
fn compiled(p: &morpholog_core::Program) -> morpholog_core::CompiledProgram {
    morpholog_core::CompiledProgram::new(p.clone()).expect("fixture is valid")
}

#[test]
fn a_duplicate_under_unique_by_is_refused_with_the_generated_name() {
    let (p, state) = registry_with_figure();
    let reason = must_reject(
        p.transformation("record").unwrap(),
        vec![subj("f1"), subj("acme"), common::dec(999)],
        &state,
        &p.invariants,
        &p.definitions,
    );
    assert!(
        reason.to_string().contains("figure_unique_by_figure_id"),
        "the generated invariant is named: {reason}"
    );
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
    let reason = must_reject(
        p.transformation("record").unwrap(),
        vec![subj("f3"), subj("acme"), common::dec(80)],
        &state,
        &p.invariants,
        &p.definitions,
    );
    assert!(
        reason
            .to_string()
            .contains("current_figure_unique_by_owner"),
        "the pointer singleton is named: {reason}"
    );
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
    let reason = must_reject(
        p.transformation("fork_history").unwrap(),
        vec![subj("f9"), subj("f1")],
        &state,
        &p.invariants,
        &p.definitions,
    );
    assert!(
        reason
            .to_string()
            .contains("figure_supersedes_unique_by_prior"),
        "the no-fork rule is named: {reason}"
    );
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
    let guarantees = morpholog_core::guarantees(&compiled(&p));
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

#[test]
fn the_generated_unique_invariant_is_pinned_exactly() {
    // The lowered invariant's whole IR, against an independently
    // hand-built expectation - the one check the generator cannot
    // satisfy by being self-consistently wrong (shape drift like an
    // And-wrapped single agreement, or a silently missing invariant,
    // reddens here and nowhere else).
    use morpholog_core::{InvariantOrigin, Prop, Term, ValueExpr, Var};
    let program = parsed(
        r#"
program pin

predicate Account(account_id: Subject, balance: Decimal)
    unique by (account_id)

transformation open(account_id, balance):
    admit Account(account_id, balance)
"#,
    );
    let generated = program
        .invariants
        .iter()
        .find(|i| i.name.as_str() == "account_unique_by_account_id")
        .expect("the clause lowers to its invariant");
    let shared = || Term::Var(Var::from("account_id"));
    let expected_body = Prop::Implies {
        left: Box::new(Prop::And(vec![
            Prop::Claim {
                predicate: "Account".into(),
                args: vec![shared(), Term::Var(Var::from("balance_a"))],
            },
            Prop::Claim {
                predicate: "Account".into(),
                args: vec![shared(), Term::Var(Var::from("balance_b"))],
            },
        ])),
        right: Box::new(Prop::Eq(
            Box::new(ValueExpr::Term(Term::Var(Var::from("balance_a")))),
            Box::new(ValueExpr::Term(Term::Var(Var::from("balance_b")))),
        )),
    };
    assert_eq!(generated.body, expected_body);
    assert_eq!(generated.version, 1);
    assert_eq!(generated.origin, InvariantOrigin::Discipline);
}

#[test]
fn a_vacuous_clause_lowers_no_invariant_at_all() {
    // Validation refuses the all-keys clause separately; this pins the
    // LOWERING side - the vacuous clause must not quietly manufacture
    // an invariant the refusal then hides.
    let program = parse_program(
        r#"
program vacuous_lowering

predicate Item(id: Subject, label: Subject)
    unique by (id, label)
"#,
    )
    .expect("parses; validation refuses separately");
    assert!(
        program.invariants.is_empty(),
        "an all-keys clause generated an invariant: {:?}",
        program
            .invariants
            .iter()
            .map(|i| &i.name)
            .collect::<Vec<_>>()
    );
}

#[test]
fn lineage_provenance_travels_to_the_coverage_report() {
    // The no-fork invariant generated from `superseded via` carries
    // its `from:` provenance into every legibility surface; coverage's
    // report is the pinnable one.
    let program = parsed(
        r#"
program lineage_provenance

predicate Current(owner: Subject, figure_id: Subject)
    current pointer by (owner)
    superseded via Supersedes
predicate Supersedes(successor: Subject, prior: Subject)

transformation point(owner, figure_id):
    admit Current(owner, figure_id)

transformation restate(owner, successor, prior):
    bind Current(owner, prior)
    retract Current(owner, prior)
    admit Current(owner, successor)
    admit Supersedes(successor, prior)
"#,
    );
    let report = morpholog_core::CoverageTracker::new(&program).into_report();
    let lineage_entry = report
        .invariants
        .iter()
        .find(|i| i.invariant == "supersedes_unique_by_prior")
        .expect("the no-fork invariant is tracked");
    let from = lineage_entry.from.as_deref().expect("carries provenance");
    assert!(
        from.contains("Supersedes"),
        "provenance names the lineage predicate: {from}"
    );
}

#[test]
fn an_unlowered_lineage_is_caught_and_an_unfit_one_is_not_expected() {
    // Hand-built IR that skips lowering must fail validation for the
    // superseded-via clause's missing no-fork invariant - and only
    // when the lineage predicate actually has the two-argument shape
    // the convention requires.
    use morpholog_core::ir_builder::{predicate, program};
    use morpholog_core::{Discipline, ValidationError};
    let two_arg = {
        let mut p = program("unlowered")
            .predicates(vec![
                {
                    let mut d = predicate("Figure")
                        .subject("figure_id")
                        .decimal("amount")
                        .build();
                    d.disciplines = vec![
                        Discipline::UniqueBy {
                            fields: vec!["figure_id".to_string()],
                        },
                        Discipline::CurrentPointerBy {
                            fields: vec!["figure_id".to_string()],
                        },
                    ];
                    d
                },
                {
                    let mut d = predicate("Supersedes")
                        .subject("successor")
                        .subject("prior")
                        .build();
                    d.disciplines = vec![Discipline::SupersededVia {
                        lineage: "Supersedes".into(),
                    }];
                    d
                },
            ])
            .build();
        p.invariants.clear();
        p
    };
    let errors = two_arg.validate().expect_err("unlowered must fail");
    assert!(
        errors.iter().any(
            |e| matches!(e, ValidationError::DisciplineNotLowered { invariant, .. }
                if invariant.contains("supersedes_unique_by_prior"))
        ),
        "the missing no-fork invariant is named: {errors:?}"
    );
}

// ============================================================
// effective by: the in-force-on-a-date selector, generated
// ============================================================

const EFFECTIVE_DATED: &str = r#"program eff

predicate ChargeRate(charge: Subject, effective_from: Date, amount: Decimal)
    effective by (charge) on (effective_from)
predicate Priced(charge: Subject, amount: Decimal)

invariant priced_at_the_rate_in_force:
    Priced(charge, amount)
    implies charge_rate_in_force_on(charge, @2026-06-01, amount)

transformation add_rate(charge, effective_from, amount):
    admit ChargeRate(charge, effective_from, amount)

transformation price(charge, amount):
    admit Priced(charge, amount)
"#;

/// The generated selector picks the latest version not after the date -
/// so a superseded rate and a future rate are both refused, and only the
/// one in force is admitted.
///
/// Three rates and three verdicts, because the two wrong answers fail
/// differently: the 2025 rate is real but superseded, and the 2026-09
/// rate is real but not yet in force. A selector that got only the
/// `on_or_before` half right would admit the first; one that got only
/// the `no later version` half right would admit the second.
#[test]
fn the_generated_selector_admits_only_the_rate_in_force() {
    let p = parsed(EFFECTIVE_DATED);
    let add = p.transformation("add_rate").expect("add_rate").clone();
    let price = p.transformation("price").expect("price").clone();
    let mut state = State::default();
    for (from, amount) in [
        ("2025-01-01", "10.00"),
        ("2026-01-01", "12.00"),
        ("2026-09-01", "99.00"),
    ] {
        state = must_accept(
            &add,
            vec![subj("c1"), common::date(from), common::dec_str(amount)],
            state,
            &p.invariants,
            &p.definitions,
        );
    }

    // In force on 2026-06-01.
    let ok = must_accept(
        &price,
        vec![subj("c1"), common::dec_str("12.00")],
        state.clone(),
        &p.invariants,
        &p.definitions,
    );
    assert!(common::has_claim(
        &ok,
        "Priced",
        &[subj("c1"), common::dec_str("12.00")]
    ));

    for wrong in ["10.00", "99.00"] {
        must_reject(
            &price,
            vec![subj("c1"), common::dec_str(wrong)],
            &state,
            &p.invariants,
            &p.definitions,
        );
    }
}

/// Lowering twice must not generate the selector twice - a programme can
/// be re-parsed, and a duplicate definition name would make the call
/// ambiguous.
#[test]
fn generating_the_selector_is_idempotent() {
    let mut p = parse_program(EFFECTIVE_DATED).expect("parses");
    let before = p.definitions.len();
    morpholog_core::lower_discipline_definitions(&mut p);
    morpholog_core::lower_discipline_definitions(&mut p);
    assert_eq!(
        p.definitions.len(),
        before,
        "already-lowered programme must not gain another selector"
    );
}

/// An authored definition of the generated name is refused rather than
/// silently winning. Caught in the surface, the only place that still
/// knows which definitions the author wrote - after lowering appends to
/// the same list, nothing can tell them apart.
#[test]
fn an_authored_definition_may_not_shadow_the_generated_selector() {
    let source = r#"program clash

predicate ChargeRate(charge: Subject, effective_from: Date, amount: Decimal)
    effective by (charge) on (effective_from)

define charge_rate_in_force_on(charge, as_of, effective_from, amount):
    ChargeRate(charge, effective_from, amount)

transformation add_rate(charge, effective_from, amount):
    admit ChargeRate(charge, effective_from, amount)
"#;
    let err = parse_program(source).expect_err("a shadowing definition must be refused");
    let text = format!("{err:?}");
    assert!(
        text.contains("already defines it"),
        "must name the collision: {text}"
    );
}

/// The dating field has to be a moment, and it cannot also be a key -
/// grouping versions by the date that orders them would leave nothing
/// able to supersede anything.
#[test]
fn an_unusable_effective_clause_is_refused_by_name() {
    let not_a_time = r#"program nt
predicate R(charge: Subject, tag: Subject, amount: Decimal)
    effective by (charge) on (tag)
transformation t(charge, tag, amount):
    admit R(charge, tag, amount)
"#;
    let date_is_key = r#"program dk
predicate R(charge: Subject, effective_from: Date, amount: Decimal)
    effective by (charge, effective_from) on (effective_from)
transformation t(charge, effective_from, amount):
    admit R(charge, effective_from, amount)
"#;
    // Matched on the VARIANT, not the rendered message: the text is for a
    // human and may be reworded, which is exactly the instability that
    // makes an unnamed `require` hard to test (#261).
    let program = parse_program(not_a_time).expect("parses; validation refuses the clause");
    let errs = program
        .validate()
        .expect_err("a subject cannot order versions");
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::EffectiveDateNotATime { .. })),
        "got {errs:?}"
    );

    let program = parse_program(date_is_key).expect("parses; validation refuses the clause");
    let errs = program
        .validate()
        .expect_err("the date cannot also be a key");
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::EffectiveDateIsAKey { .. })),
        "got {errs:?}"
    );
}

/// The generated selector's own variable names must not collide with the
/// predicate's fields.
///
/// A payload field called `as_of` put that name in the parameter list
/// twice, and the resulting error named `definition r_in_force_on` - a
/// definition the author never wrote, so unactionable. Every field name
/// here is one the generator wants for itself.
#[test]
fn the_generated_selector_avoids_the_predicates_own_field_names() {
    let source = r#"program collide

predicate R(charge: Subject, effective_from: Date, as_of: Decimal, later_effective_from: Decimal)
    effective by (charge) on (effective_from)
predicate Seen(charge: Subject)

invariant seen_has_a_version_in_force:
    Seen(charge)
    implies r_in_force_on(charge, @2026-06-01, _, _)

transformation add(charge, effective_from, as_of, later_effective_from):
    admit R(charge, effective_from, as_of, later_effective_from)

transformation see(charge):
    admit Seen(charge)
"#;
    // Validation is the assertion: a collision surfaced as
    // DuplicateParameter plus a kind conflict, both naming the generated
    // definition rather than anything the author could edit.
    let p = parsed(source);
    let add = p.transformation("add").expect("add").clone();
    let see = p.transformation("see").expect("see").clone();
    let state = must_accept(
        &add,
        vec![
            subj("c1"),
            common::date("2026-01-01"),
            common::dec_str("1"),
            common::dec_str("2"),
        ],
        State::default(),
        &p.invariants,
        &p.definitions,
    );
    // And the selector still selects: a version is in force, so this is
    // admitted rather than refused by the invariant above.
    must_accept(&see, vec![subj("c1")], state, &p.invariants, &p.definitions);
}

/// The clause claims one version per key per date, so it owes the
/// invariant that makes that true.
///
/// Without it two rows tie for "latest" and the selector returns BOTH -
/// so two contradictory prices each satisfied "priced at the rate in
/// force", which is precisely what the discipline exists to prevent. The
/// adversarial shape is two different payloads at the same effective
/// date; the second must be refused.
#[test]
fn two_versions_at_one_effective_date_cannot_both_stand() {
    let p = parsed(EFFECTIVE_DATED);
    let add = p.transformation("add_rate").expect("add_rate").clone();
    let state = must_accept(
        &add,
        vec![
            subj("c1"),
            common::date("2026-01-01"),
            common::dec_str("10.00"),
        ],
        State::default(),
        &p.invariants,
        &p.definitions,
    );
    must_reject(
        &add,
        vec![
            subj("c1"),
            common::date("2026-01-01"),
            common::dec_str("12.00"),
        ],
        &state,
        &p.invariants,
        &p.definitions,
    );
}

/// One selector per predicate, so one clause.
///
/// Both clauses generate the same name, so the second was silently
/// skipped by the lowering and the governing doctrine ended up decided by
/// declaration order - a programme that validates and means something
/// other than it says.
#[test]
fn a_predicate_may_carry_only_one_effective_clause() {
    let source = r#"program two
predicate Rate(account: Subject, market: Subject, valid_from: Date, published_at: Timestamp, amount: Decimal)
    effective by (account) on (valid_from)
    effective by (market) on (published_at)
transformation t(account, market, valid_from, published_at, amount):
    admit Rate(account, market, valid_from, published_at, amount)
"#;
    let program = parse_program(source).expect("parses; validation refuses the pair");
    let errs = program.validate().expect_err("two clauses cannot coexist");
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::MultipleEffectiveClauses { .. })),
        "got {errs:?}"
    );
}

// ============================================================
// total over: declaring the companion an effective-dated
// predicate needs
// ============================================================

const TOTALITY: &str = r#"program tot

predicate ChargeRate(charge: Subject, effective_from: Date, amount: Decimal)
    effective by (charge) on (effective_from)
predicate Priced(charge: Subject, priced_on: Date, amount: Decimal)

invariant priced_at_the_rate_in_force:
    Priced(charge, d, amount)
    implies charge_rate_in_force_on(charge, d, amount)

invariant priced_charges_have_a_rate total over ChargeRate:
    Priced(charge, d, _)
    implies (exists ef: ChargeRate(charge, ef, _) and ef on_or_before d)

transformation add_rate(charge, effective_from, amount):
    admit ChargeRate(charge, effective_from, amount)

transformation price(charge, priced_on, amount):
    admit Priced(charge, priced_on, amount)
"#;

/// An effective-dated predicate with nothing declaring its totality earns
/// a hint - and `--strict` turns that into a refusal.
///
/// The tier matters: a partial effective-dated predicate can be a correct
/// model (a rule that genuinely should not apply before the first version
/// exists), so this cannot be a hard error. An author who wants the
/// pairing guaranteed rather than remembered runs `--strict`, and then the
/// omission is unwritable.
#[test]
fn an_effective_predicate_without_a_declared_companion_is_flagged() {
    let undeclared = TOTALITY.replace(" total over ChargeRate", "");
    let program = parse_program(&undeclared).expect("parses either way");
    let compiled = morpholog_core::CompiledProgram::new(program).expect("valid");
    let hints = morpholog_core::lints(&compiled);
    assert!(
        hints.iter().any(|l| matches!(
            l,
            morpholog_core::Lint::EffectiveWithoutDeclaredTotality { predicate } if predicate == "ChargeRate"
        )),
        "got {hints:?}"
    );

    // Declared: no finding.
    let declared = parse_program(TOTALITY).expect("parses");
    let compiled = morpholog_core::CompiledProgram::new(declared).expect("valid");
    let hints = morpholog_core::lints(&compiled);
    assert!(
        !hints.iter().any(|l| matches!(
            l,
            morpholog_core::Lint::EffectiveWithoutDeclaredTotality { .. }
        )),
        "a declared companion must satisfy it; got {hints:?}"
    );
}

/// A declared companion settles the governing-selection lint too, which
/// previously had to recognise the backstop by shape.
///
/// This is the "smell to checked pairing" half: the author says which rule
/// backstops the predicate, so an unusual-but-intended backstop counts and
/// a shape matching by accident does not.
#[test]
fn a_declared_companion_settles_the_governing_selection_lint() {
    let declared = parse_program(TOTALITY).expect("parses");
    let compiled = morpholog_core::CompiledProgram::new(declared).expect("valid");
    let hints = morpholog_core::lints(&compiled);
    assert!(
        !hints.iter().any(|l| matches!(
            l,
            morpholog_core::Lint::GoverningSelectionWithoutTotality { .. }
        )),
        "the declaration is the backstop; got {hints:?}"
    );
}

/// The clause round-trips: formatting must not silently downgrade a
/// checked pairing back to a shape guess.
#[test]
fn the_totality_clause_survives_a_format_and_reparse() {
    let program = parse_program(TOTALITY).expect("parses");
    let reparsed =
        parse_program(&morpholog_core::format::format_program(&program)).expect("reparses");
    let declared: Vec<_> = reparsed
        .invariants
        .iter()
        .filter_map(|i| i.totality_for.as_ref().map(ToString::to_string))
        .collect();
    assert_eq!(declared, vec!["ChargeRate".to_string()]);
}
