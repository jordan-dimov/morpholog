//! Behavioural tests for defined propositions (`define`): the
//! relational-substitution semantics (generator projection, ground-arg
//! filtering, hygiene-by-projection, multiplicity dedup), composition
//! with `bind` / `pre(...)` / `sum`, the authoring-time checks the
//! construct adds, and the legibility surfaces seeing through calls.
//!
//! Scenario programmes are inline `.morph`, per the testing doctrine:
//! the business story is the spec. The deliberately-adversarial shapes
//! a parser can never produce (hygiene capture attempts, unresolved
//! calls) live in `morpholog-core`'s own test layer instead.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{dec, subj};
use morpholog_core::{EvalError, Outcome, Program, State, ValidationError};
use morpholog_examples::clinical_trial_enrolment;
use morpholog_surface::parse_program;
use morpholog_test_support::{has_claim, must_accept, propose_with_test_actor};

fn parsed(source: &str) -> Program {
    let program = parse_program(source).expect("scenario programme should parse");
    program
        .validate()
        .expect("scenario programme should validate");
    program
}

fn validation_errors(source: &str) -> Vec<ValidationError> {
    let program = parse_program(source).expect("programme should parse");
    program
        .validate()
        .expect_err("programme should fail validation")
}

// ============================================================
// The club: nested definitions, bind through a call (generator
// projection), ground-argument filtering, lawful rejection.
// ============================================================

const CLUB: &str = r#"
program club

predicate Member(person: Subject, club: Subject)
predicate Suspended(person: Subject)
predicate Sponsor(person: Subject, newcomer: Subject)
predicate Admitted(newcomer: Subject, sponsor: Subject)

define member_in_good_standing(person, club):
    Member(person, club)
    and not Suspended(person)

define sponsored_by_member(newcomer, club, sponsor):
    Sponsor(sponsor, newcomer)
    and member_in_good_standing(sponsor, club)

invariant admissions_are_sponsored:
    Admitted(newcomer, sponsor) implies Sponsor(sponsor, newcomer)

transformation join(person, club):
    admit Member(person, club)

transformation suspend(person):
    admit Suspended(person)

transformation sponsor(person, newcomer):
    admit Sponsor(person, newcomer)

transformation enrol(newcomer, club):
    bind sponsored_by_member(newcomer, club, sponsor)
    admit Admitted(newcomer, sponsor)
"#;

fn club_with_sponsor() -> (Program, State) {
    let p = parsed(CLUB);
    let state = must_accept(
        p.transformation("join").unwrap(),
        vec![subj("alice"), subj("chess_club")],
        State::default(),
        &p.invariants,
        &p.definitions,
    );
    let state = must_accept(
        p.transformation("sponsor").unwrap(),
        vec![subj("alice"), subj("nina")],
        state,
        &p.invariants,
        &p.definitions,
    );
    (p, state)
}

// `bind` through a call: the body finds the sponsor, the projection
// hands the binding out through the `sponsor` argument, and the
// admitted claim carries it. The caller never sees the body's
// internals - only the projected argument.
#[test]
fn bind_through_a_call_projects_the_generator_argument_out() {
    let (p, state) = club_with_sponsor();
    let state = must_accept(
        p.transformation("enrol").unwrap(),
        vec![subj("nina"), subj("chess_club")],
        state,
        &p.invariants,
        &p.definitions,
    );
    assert!(has_claim(
        &state,
        "Admitted",
        &[subj("nina"), subj("alice")]
    ));
}

// Ground arguments filter: a suspended sponsor fails the nested
// `member_in_good_standing` call, so the outer call has no match and
// the bind rejects lawfully - a business refusal, not a kernel error.
#[test]
fn a_suspended_sponsor_fails_the_nested_condition() {
    let (p, state) = club_with_sponsor();
    let state = must_accept(
        p.transformation("suspend").unwrap(),
        vec![subj("alice")],
        state,
        &p.invariants,
        &p.definitions,
    );
    let outcome = propose_with_test_actor(
        p.transformation("enrol").unwrap(),
        vec![subj("nina"), subj("chess_club")],
        &state,
        &p.invariants,
        &p.definitions,
    )
    .expect("kernel must not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

// Two sponsors in good standing: the call yields two distinct
// projections, and `bind` demands exactly one - the same multi-match
// kernel error an ambiguous claim lookup raises, surfaced through the
// call unchanged.
#[test]
fn two_distinct_projections_make_bind_a_multi_match_error() {
    let (p, state) = club_with_sponsor();
    let state = must_accept(
        p.transformation("join").unwrap(),
        vec![subj("bob"), subj("chess_club")],
        state,
        &p.invariants,
        &p.definitions,
    );
    let state = must_accept(
        p.transformation("sponsor").unwrap(),
        vec![subj("bob"), subj("nina")],
        state,
        &p.invariants,
        &p.definitions,
    );
    let err = propose_with_test_actor(
        p.transformation("enrol").unwrap(),
        vec![subj("nina"), subj("chess_club")],
        &state,
        &p.invariants,
        &p.definitions,
    )
    .expect_err("two projections must surface as a kernel error at bind");
    assert!(matches!(err, EvalError::TypeMismatch(_)), "got {err:?}");
}

// ============================================================
// Multiplicity: a call yields each distinct argument-binding witness
// once. The same body inlined counts internal witnesses.
// ============================================================

const VOUCHES: &str = r#"
program vouches

predicate Witness(subject: Subject, voucher: Subject)
predicate VouchedCount(subject: Subject, n: Decimal)
predicate RawCount(subject: Subject, n: Decimal)

define vouched(subject):
    Witness(subject, _)

transformation observe(subject, voucher):
    admit Witness(subject, voucher)

transformation count_through_call(subject):
    let n = sum(1 | vouched(subject))
    admit VouchedCount(subject, n)

transformation count_inline(subject):
    let m = sum(1 | Witness(subject, _))
    admit RawCount(subject, m)
"#;

#[test]
fn a_call_counts_distinct_projections_while_the_inline_body_counts_witnesses() {
    let p = parsed(VOUCHES);
    let state = must_accept(
        p.transformation("observe").unwrap(),
        vec![subj("deal"), subj("v1")],
        State::default(),
        &p.invariants,
        &p.definitions,
    );
    let state = must_accept(
        p.transformation("observe").unwrap(),
        vec![subj("deal"), subj("v2")],
        state,
        &p.invariants,
        &p.definitions,
    );
    // Through the call: "is the deal vouched?" has one distinct answer
    // for the projected arguments, however many internal witnesses
    // exist - internal multiplicity is not observable.
    let state = must_accept(
        p.transformation("count_through_call").unwrap(),
        vec![subj("deal")],
        state,
        &p.invariants,
        &p.definitions,
    );
    assert!(has_claim(&state, "VouchedCount", &[subj("deal"), dec(1)]));
    // Inlined, the same proposition counts its witnesses - two claims,
    // two matches. The contrast is the dedup contract, pinned.
    let state = must_accept(
        p.transformation("count_inline").unwrap(),
        vec![subj("deal")],
        state,
        &p.invariants,
        &p.definitions,
    );
    assert!(has_claim(&state, "RawCount", &[subj("deal"), dec(2)]));
}

// ============================================================
// pre(...) around a call: the context swap applies to the body.
// ============================================================

const TALLY: &str = r#"
program tally

predicate Tally(n: Decimal)

define tally_at(n):
    Tally(n)

invariant tally_never_decreases:
    Tally(n) and pre(tally_at(m)) implies m <= n

transformation set_first_tally(n):
    admit Tally(n)

transformation move_tally(from, to):
    require tally_at(from)
    retract Tally(from)
    admit Tally(to)
"#;

#[test]
fn a_call_wrapped_in_pre_reads_the_pre_state() {
    let p = parsed(TALLY);
    let state = must_accept(
        p.transformation("set_first_tally").unwrap(),
        vec![dec(5)],
        State::default(),
        &p.invariants,
        &p.definitions,
    );
    // Raising the tally satisfies the transition invariant: the
    // pre-state call sees 5, the candidate sees 10.
    let state = must_accept(
        p.transformation("move_tally").unwrap(),
        vec![dec(5), dec(10)],
        state,
        &p.invariants,
        &p.definitions,
    );
    // Lowering it is refused by the same invariant.
    let outcome = propose_with_test_actor(
        p.transformation("move_tally").unwrap(),
        vec![dec(10), dec(3)],
        &state,
        &p.invariants,
        &p.definitions,
    )
    .expect("kernel must not error");
    assert!(matches!(outcome, Outcome::Rejected { .. }));
}

// ============================================================
// Authoring-time checks: the rules `define` adds, each refused with
// the teaching-quality message the contract promises.
// ============================================================

#[test]
fn definitions_referencing_each_other_in_a_cycle_are_refused() {
    let errors = validation_errors(
        r#"
program cyclic

predicate Thing(x: Subject)

define even_step(x):
    odd_step(x)

define odd_step(x):
    even_step(x)
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::DefinitionCycle { .. })),
        "got {errors:?}"
    );
}

// The cycle diagnostic names the cycle's members only - an entry
// definition that merely *reaches* the cycle is not blamed for it.
#[test]
fn the_cycle_diagnostic_excludes_a_caller_that_only_reaches_the_cycle() {
    let errors = validation_errors(
        r#"
program reaches_a_cycle

predicate Thing(x: Subject)

define entry(x):
    ping(x)

define ping(x):
    pong(x)

define pong(x):
    ping(x)
"#,
    );
    let cycle = errors
        .iter()
        .find_map(|e| match e {
            ValidationError::DefinitionCycle { names } => Some(names.clone()),
            _ => None,
        })
        .expect("a cycle is reported");
    assert_eq!(
        cycle,
        vec!["ping".to_string(), "pong".to_string()],
        "only the cycle's members are named, not `entry`"
    );
}

#[test]
fn a_definition_sharing_a_predicate_name_is_refused() {
    let errors = validation_errors(
        r#"
program colliding

predicate Approved(doc: Subject)

define Approved(doc):
    Approved(doc)
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::DefinitionNameCollision { .. })),
        "got {errors:?}"
    );
}

#[test]
fn actor_inside_a_definition_body_is_refused() {
    let errors = validation_errors(
        r#"
program actor_in_body

predicate MayAct(person: Subject)

define proposer_may_act(ignored):
    MayAct(actor) and MayAct(ignored)
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::ActorNotAvailable { .. })),
        "got {errors:?}"
    );
}

#[test]
fn pre_inside_a_definition_body_is_refused() {
    let errors = validation_errors(
        r#"
program pre_in_body

predicate Tally(n: Decimal)

define previous_tally(n):
    pre(Tally(n))
"#,
    );
    let rendered: Vec<String> = errors.iter().map(ToString::to_string).collect();
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::PreNotAvailable { .. })),
        "got {rendered:?}"
    );
    assert!(
        rendered.iter().any(|m| m.contains("wrap the call")),
        "the message teaches the fix: {rendered:?}"
    );
}

#[test]
fn a_parameter_the_body_never_references_is_refused_with_guidance() {
    let errors = validation_errors(
        r#"
program dead_param

predicate Item(x: Subject)

define item_exists(x, unused_day):
    Item(x)
"#,
    );
    let rendered: Vec<String> = errors.iter().map(ToString::to_string).collect();
    assert!(
        rendered
            .iter()
            .any(|m| m.contains("`unused_day`") && m.contains("not referenced")),
        "got {rendered:?}"
    );
}

#[test]
fn a_duplicate_parameter_is_refused() {
    let errors = validation_errors(
        r#"
program dup_param

predicate Pair(x: Subject, y: Subject)

define paired(x, x):
    Pair(x, x)
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::DuplicateParameter { .. })),
        "got {errors:?}"
    );
}

// Definitions are proposition-valued only: `admit` (like `retract` and
// `value`) needs a predicate, and naming a definition there is a
// category error with its own guidance, not a misleading
// undeclared-predicate report.
#[test]
fn admitting_a_definition_is_a_category_error_with_guidance() {
    let errors = validation_errors(
        r#"
program admit_a_condition

predicate Item(x: Subject)

define item_exists(x):
    Item(x)

transformation record(x):
    admit item_exists(x)
"#,
    );
    let rendered: Vec<String> = errors.iter().map(ToString::to_string).collect();
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::UnresolvedDefinitionCall { .. })),
        "got {rendered:?}"
    );
    assert!(
        rendered
            .iter()
            .any(|m| m.contains("where a predicate is required")),
        "the message names the category error: {rendered:?}"
    );
}

#[test]
fn a_call_with_the_wrong_arity_is_refused() {
    let errors = validation_errors(
        r#"
program wrong_arity

predicate Item(x: Subject)
predicate Tagged(x: Subject)

define item_exists(x):
    Item(x)

invariant tagged_items_exist:
    Tagged(x) implies item_exists(x, x)
"#,
    );
    assert!(
        errors.iter().any(|e| matches!(
            e,
            ValidationError::ArityMismatch {
                vocabulary: morpholog_core::VocabularyKind::Definition,
                ..
            }
        )),
        "got {errors:?}"
    );
}

// A use-only parameter (one the body consults but never binds) must
// arrive bound: an invariant calling with a free variable in that
// position is the same unbound-name error the runtime would raise.
#[test]
fn an_unbound_argument_for_a_use_only_parameter_is_refused() {
    let errors = validation_errors(
        r#"
program unbound_use_only

predicate Item(x: Subject, recorded_on: Date)
predicate Flagged(x: Subject)

define recorded_by(x, deadline):
    Item(x, recorded_on)
    and recorded_on on_or_before deadline

invariant flagged_items_recorded:
    Flagged(x) implies recorded_by(x, deadline)
"#,
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::UnboundVariable { .. })),
        "got {errors:?}"
    );
}

// ============================================================
// Legibility surfaces see through calls: the control matrix lists the
// predicates a gate consults via its named conditions.
// ============================================================

#[test]
fn inspect_controls_lists_predicates_consulted_through_definitions() {
    let matrix = morpholog_core::controls(&clinical_trial_enrolment::program());
    let randomise = matrix
        .transformations
        .iter()
        .find(|t| t.transformation == "randomise_participant")
        .expect("randomise_participant is in the matrix");
    let consulted: Vec<&str> = randomise
        .gates
        .iter()
        .flat_map(|g| g.consults.iter().map(String::as_str))
        .collect();
    for expected in [
        "ProtocolVersion",
        "ProtocolApprovedBy",
        "ConsentFormVersion",
        "InformedConsentObtained",
        "DelegatedInvestigator",
        "EligibilityAssessment",
    ] {
        assert!(
            consulted.contains(&expected),
            "{expected} should be consulted through the named conditions: {consulted:?}"
        );
    }
}
