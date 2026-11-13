//! Behavioural tests for `define`: how a call binds, filters and
//! deduplicates, how it composes with `bind` / `pre(...)` / `sum`, the
//! authoring-time checks, and the read-side reports seeing through calls.
//!
//! Scenarios are inline `.morph`. Shapes a parser can never produce
//! (capture attempts, unresolved calls) are tested in `morpholog-core`.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

mod common;

use common::{dec, must_reject, subj};
use morpholog_core::{EvalError, Program, State, ValidationError};
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
        &p,
    );
    let state = must_accept(
        p.transformation("sponsor").unwrap(),
        vec![subj("alice"), subj("nina")],
        state,
        &p,
    );
    (p, state)
}

// `bind` through a call: the body finds the sponsor and hands it out
// through the `sponsor` argument. The caller sees only that argument,
// never the body's internals.

/// Build a PreparedProgram for the analysis entry points.
fn prepared(p: &morpholog_core::Program) -> morpholog_core::PreparedProgram {
    morpholog_core::PreparedProgram::new(p.clone()).expect("fixture is valid")
}

#[test]
fn bind_through_a_call_projects_the_generator_argument_out() {
    let (p, state) = club_with_sponsor();
    let state = must_accept(
        p.transformation("enrol").unwrap(),
        vec![subj("nina"), subj("chess_club")],
        state,
        &p,
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
        &p,
    );
    must_reject(
        p.transformation("enrol").unwrap(),
        vec![subj("nina"), subj("chess_club")],
        &state,
        &p,
    );
}

// Two sponsors in good standing: the call yields two results and `bind`
// demands exactly one, so it raises the same error as an ambiguous
// claim lookup.
#[test]
fn two_distinct_projections_make_bind_a_multi_match_error() {
    let (p, state) = club_with_sponsor();
    let state = must_accept(
        p.transformation("join").unwrap(),
        vec![subj("bob"), subj("chess_club")],
        state,
        &p,
    );
    let state = must_accept(
        p.transformation("sponsor").unwrap(),
        vec![subj("bob"), subj("nina")],
        state,
        &p,
    );
    let err = propose_with_test_actor(
        p.transformation("enrol").unwrap(),
        vec![subj("nina"), subj("chess_club")],
        &state,
        &p,
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
        &p,
    );
    let state = must_accept(
        p.transformation("observe").unwrap(),
        vec![subj("deal"), subj("v2")],
        state,
        &p,
    );
    // Through the call, "is the deal vouched?" has one answer, however
    // many matches the body has inside.
    let state = must_accept(
        p.transformation("count_through_call").unwrap(),
        vec![subj("deal")],
        state,
        &p,
    );
    assert!(has_claim(&state, "VouchedCount", &[subj("deal"), dec(1)]));
    // Inlined, the same proposition counts both matching claims.
    let state = must_accept(
        p.transformation("count_inline").unwrap(),
        vec![subj("deal")],
        state,
        &p,
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
        &p,
    );
    // Raising the tally satisfies the transition invariant: the
    // pre-state call sees 5, the candidate sees 10.
    let state = must_accept(
        p.transformation("move_tally").unwrap(),
        vec![dec(5), dec(10)],
        state,
        &p,
    );
    // Lowering it is refused by the same invariant.
    must_reject(
        p.transformation("move_tally").unwrap(),
        vec![dec(10), dec(3)],
        &state,
        &p,
    );
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

// `admit` needs a predicate. Naming a definition there gets its own
// guidance, not a misleading undeclared-predicate report.
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

// A parameter the body reads but never binds must arrive bound. Passing
// a free variable there is refused as an unbound name.
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
    let matrix = morpholog_core::controls(&prepared(&clinical_trial_enrolment::program()));
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

// ============================================================
// A definition that names a figure: one value, read by a gate, a rule
// and a derived read; and what is refused.
// ============================================================

const STOCK: &str = r#"
program stock

predicate StockItem(item: Subject, quantity: Decimal)
    unique by (item)
predicate Allocation(allocation: Subject, item: Subject, quantity: Decimal)
    unique by (allocation)
predicate Allocated(item: Subject, quantity: Decimal)

define allocated(item) = (sum(q | Allocation(item: item, quantity: q, ..)))

define headroom(item, on_hand) = (on_hand - allocated(item))

invariant allocations_fit_the_stock:
    StockItem(item, on_hand) implies allocated(item) <= on_hand

transformation receive(item, quantity):
    admit StockItem(item, quantity)

transformation allocate(allocation, item, quantity):
    bind StockItem(item, on_hand)
    require within_stock: quantity <= headroom(item, on_hand)
    admit Allocation(allocation, item, quantity)

transformation force(allocation, item, quantity):
    admit Allocation(allocation, item, quantity)

derived Allocated(item):
    over StockItem(item: item, ..)
    value quantity = allocated(item)
"#;

fn stocked() -> (Program, State) {
    let p = parsed(STOCK);
    let state = must_accept(
        p.transformation("receive").unwrap(),
        vec![subj("pallet"), dec(10)],
        State::default(),
        &p,
    );
    let state = must_accept(
        p.transformation("allocate").unwrap(),
        vec![subj("a1"), subj("pallet"), dec(4)],
        state,
        &p,
    );
    (p, state)
}

/// The same figure decides the gate, the rule and the read: with four
/// of ten reserved, a gate for six admits and one for seven refuses,
/// the rule refuses an act that skips the gate, and the read shows the
/// figure.
#[test]
fn a_figure_is_read_by_a_gate_a_rule_and_a_derived_read() {
    let (p, state) = stocked();
    let allocate = p.transformation("allocate").unwrap();
    let within = must_accept(
        allocate,
        vec![subj("a2"), subj("pallet"), dec(6)],
        state.clone(),
        &p,
    );
    assert!(has_claim(
        &within,
        "Allocation",
        &[subj("a2"), subj("pallet"), dec(6)]
    ));
    assert!(matches!(
        must_reject(allocate, vec![subj("a3"), subj("pallet"), dec(7)], &state, &p),
        morpholog_core::RejectionReason::Require { name: Some(name), .. } if name.as_str() == "within_stock"
    ));
    assert!(matches!(
        must_reject(
            p.transformation("force").unwrap(),
            vec![subj("a3"), subj("pallet"), dec(7)],
            &state,
            &p
        ),
        morpholog_core::RejectionReason::Invariant { name, .. } if name.as_str() == "allocations_fit_the_stock"
    ));
    let derived = p
        .derived_claims
        .iter()
        .find(|d| d.predicate.as_str() == "Allocated")
        .unwrap();
    let rows = morpholog_test_support::enumerate_derived(&p, derived, &within).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].args, vec![subj("pallet"), dec(10)]);
}

/// The rule over the figure is judged per item: the plan follows the
/// call into the sum, which keys on the item the case names.
#[test]
fn a_rule_over_a_figure_stays_bound_to_its_case() {
    use morpholog_core::{Bounding, ImpactPlan};
    let p = parsed(STOCK);
    let inv = p
        .invariants
        .iter()
        .find(|i| i.name.as_str() == "allocations_fit_the_stock")
        .unwrap();
    assert!(
        matches!(
            ImpactPlan::with_definitions(inv, &p.definitions).bounding(),
            Bounding::Bound
        ),
        "the call is followed into the sum"
    );
}

/// An argument that is an expression carries no case into the body,
/// so the rule is checked whole, and `check -v` says which call.
#[test]
fn a_figure_called_with_an_expression_widens_the_rule() {
    use morpholog_core::{Bounding, ImpactPlan, Widening};
    let p = parsed(&STOCK.replace(
        "StockItem(item, on_hand) implies allocated(item) <= on_hand",
        "StockItem(item, on_hand) implies headroom(item, on_hand + 0) >= 0",
    ));
    let inv = p
        .invariants
        .iter()
        .find(|i| i.name.as_str() == "allocations_fit_the_stock")
        .unwrap();
    assert!(matches!(
        ImpactPlan::with_definitions(inv, &p.definitions).bounding(),
        Bounding::Whole(Widening::UnfollowedCall(name)) if name.as_str() == "headroom"
    ));
}

/// A figure is not a condition and a condition is not a figure: each
/// called as the other is refused by name, with the context.
#[test]
fn a_figure_and_a_condition_called_as_each_other_are_refused() {
    let as_condition = validation_errors(&STOCK.replace(
        "    require within_stock: quantity <= headroom(item, on_hand)\n",
        "    require within_stock: allocated(item)\n",
    ));
    assert!(
        as_condition
            .iter()
            .any(|e| matches!(e, ValidationError::DefinitionYieldsValue { name, .. } if name == "allocated")),
        "{as_condition:?}"
    );
    let as_figure = validation_errors(
        &CLUB.replace("    bind sponsored_by_member(newcomer, club, sponsor)\n    admit Admitted(newcomer, sponsor)\n",
        "    let sponsor = member_in_good_standing(newcomer, club)\n    admit Admitted(newcomer, sponsor)\n"),
    );
    assert!(
        as_figure
            .iter()
            .any(|e| matches!(e, ValidationError::DefinitionYieldsNoValue { name, .. } if name == "member_in_good_standing")),
        "{as_figure:?}"
    );
}

/// What a figure's body may not do: read the actor, read `pre`, leave a
/// parameter unused, call itself round a cycle, take a builtin's name;
/// and what a call may not do: pass the wrong number or kind of
/// arguments, or name a figure nothing defines.
#[test]
fn what_a_figure_refuses_is_refused_by_name() {
    let refused = |body: &str, call: &str, wanted: &dyn Fn(&ValidationError) -> bool| {
        let source = STOCK
            .replace(
                "define headroom(item, on_hand) = (on_hand - allocated(item))",
                body,
            )
            .replace("quantity <= headroom(item, on_hand)", call);
        let errors = validation_errors(&source);
        assert!(errors.iter().any(wanted), "{source}\n{errors:?}");
    };
    refused(
        "define headroom(item, on_hand) = (on_hand - allocated(actor))",
        "quantity <= headroom(item, on_hand)",
        &|e| matches!(e, ValidationError::ActorNotAvailable { .. }),
    );
    refused(
        "define headroom(item, on_hand) = (on_hand - sum(q | pre(Allocation(_, item, q))))",
        "quantity <= headroom(item, on_hand)",
        &|e| matches!(e, ValidationError::PreNotAvailable { .. }),
    );
    refused(
        "define headroom(item, on_hand) = (allocated(item))",
        "quantity <= headroom(item, on_hand)",
        &|e| matches!(e, ValidationError::ParameterNotReferenced { parameter, .. } if parameter == "on_hand"),
    );
    refused(
        "define headroom(item, on_hand) = (on_hand - headroom(item, on_hand))",
        "quantity <= headroom(item, on_hand)",
        &|e| matches!(e, ValidationError::DefinitionCycle { names } if names == &["headroom"]),
    );
    refused(
        "define periods(item, on_hand) = (on_hand - allocated(item))",
        "quantity <= on_hand",
        &|e| matches!(e, ValidationError::DefinitionNameIsBuiltin { name } if name == "periods"),
    );
    refused(
        "define headroom(item, on_hand) = (if(StockItem(item, _), on_hand, on_hand))",
        "quantity <= headroom(item, on_hand)",
        &|e| matches!(e, ValidationError::DefinitionKindUnknown { definition } if definition == "headroom"),
    );
    refused(
        "define headroom(item, on_hand) = (on_hand - allocated(item))",
        "quantity <= headroom(item)",
        &|e| matches!(e, ValidationError::ArityMismatch { name, expected: 2, actual: 1, .. } if name == "headroom"),
    );
    refused(
        "define headroom(item, on_hand) = (on_hand - allocated(item))",
        "quantity <= headroom(item, item)",
        &|e| {
            matches!(
                e,
                ValidationError::VariableKindConflict { .. }
                    | ValidationError::OperandKindMismatch { .. }
                    | ValidationError::ArgKindMismatch { .. }
            )
        },
    );
    refused(
        "define headroom(item, on_hand) = (on_hand - allocated(item))",
        "quantity <= room(item, on_hand)",
        &|e| matches!(e, ValidationError::Undeclared { name, .. } if name == "room"),
    );
}

// ============================================================
// The pricing scope an embedder wanted written once (#299): a condition
// that binds its scope by equality exports nothing, so the scope is a
// figure, read by the act with `let`.
// ============================================================

const PRICING: &str = r#"
program pricing

predicate Charge(charge: Subject, rate_unit: Subject, vat_policy: Subject, price_scope: Subject)
    unique by (charge)
predicate Priced(charge: Subject, scope: Subject)
    unique by (charge)

define price_scope_of(charge, meter) = (if(Charge(charge: charge, price_scope: #book, ..), #book, meter))

transformation declare(charge, rate_unit, vat_policy, price_scope):
    admit Charge(charge, rate_unit, vat_policy, price_scope)

transformation price(charge, meter):
    bind Charge(charge: charge, ..)
    let scope = price_scope_of(charge, meter)
    admit Priced(charge, scope)
"#;

#[test]
fn the_pricing_scope_is_one_figure_read_by_the_act() {
    let p = parsed(PRICING);
    let declare = p.transformation("declare").unwrap();
    let price = p.transformation("price").unwrap();
    let mut state = State::default();
    for (charge, scope) in [("c_book", "book"), ("c_meter", "meter")] {
        state = must_accept(
            declare,
            vec![subj(charge), subj("kwh"), subj("standard"), subj(scope)],
            state,
            &p,
        );
    }
    let state = must_accept(price, vec![subj("c_book"), subj("m7")], state, &p);
    assert!(has_claim(&state, "Priced", &[subj("c_book"), subj("book")]));
    let state = must_accept(price, vec![subj("c_meter"), subj("m7")], state, &p);
    assert!(has_claim(&state, "Priced", &[subj("c_meter"), subj("m7")]));
}

/// The issue's own condition, bound: `scope = #book` binds nothing, so
/// the `bind` cannot export `scope` and the checker says so. A `bind`
/// exports what a body binds through a claim pattern, as the club's
/// sponsor shows above.
#[test]
fn a_condition_that_fixes_its_scope_by_equality_exports_nothing() {
    let errors = validation_errors(
        &PRICING
            .replace(
                "define price_scope_of(charge, meter) = (if(Charge(charge: charge, price_scope: #book, ..), #book, meter))",
                "define price_scope_of(charge, meter, scope):\n    (Charge(charge: charge, price_scope: #book, ..) and scope = #book)\n    or (Charge(charge: charge, price_scope: #meter, ..) and scope = meter)",
            )
            .replace(
                "    let scope = price_scope_of(charge, meter)\n",
                "    bind the_scope: price_scope_of(charge, meter, scope)\n",
            ),
    );
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::UnboundVariable { variable, .. } if variable == "scope")),
        "{errors:?}"
    );
}
