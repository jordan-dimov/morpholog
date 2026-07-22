//! The walker-conformance corpus: constructs in adversarial contexts,
//! run through every tree-walking pass at once.
//!
//! The recurring bug class this guards is construct-in-context: a pass
//! handles a construct at top level but misses it nested (a chained
//! comparison inside a wider `and`, `pre` inside a comparison operand,
//! a sum target bound through a defined call). Most walkers hand-roll
//! their descent - mutation, polarity, kind collection, and
//! definition expansion each need more than the shared fold offers -
//! so a fix in one walker never propagates to the others. This corpus
//! is the shared gate: every fragment runs the same battery (parse,
//! validate, format round-trip, lints/controls/coverage without panic
//! and deterministically, footprints transitive, parameter kinds
//! total), plus targeted pins where the nesting is the point.
//!
//! The rule: a new construct, or a new walker, adds a fragment or an
//! assertion here first.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{
    CompiledProgram, CoverageTracker, PredicateName, Program, controls, format::format_program,
    lints, predicates_referenced_by_prop, transformation_param_kinds,
};
use morpholog_surface::parse_program;

/// A sum whose target binds inside a defined call that itself calls a
/// second definition: seed resolution and footprints must descend two
/// call frames.
const SUM_THROUGH_DEFINED_CHAIN: &str = "\
program sum_through_defined_chain
predicate Parcel(p: Subject, qty: Decimal[t])
predicate Capacity(cap: Decimal[t])
define inner_parcel(p, q):
    Parcel(p, q)
define outer_parcel(p, q):
    inner_parcel(p, q)
invariant book_within_capacity:
    Capacity(cap) implies sum(q | outer_parcel(_, q)) <= cap
transformation declare(cap):
    admit Capacity(cap)
transformation load(p, qty):
    require Capacity(_)
    admit Parcel(p, qty)
";

/// `pre(...)` wrapping a defined call: the pre-detector and the
/// footprint walkers must both see through the wrapper AND the call.
const PRE_AROUND_DEFINED: &str = "\
program pre_around_defined
predicate Sealed(box: Subject)
predicate Opened(box: Subject)
define was_sealed(b):
    Sealed(b)
invariant opening_needs_a_prior_seal:
    Opened(b) implies pre(was_sealed(b))
transformation seal(box):
    admit Sealed(box)
transformation open(box):
    require Sealed(box)
    retract Sealed(box)
    admit Opened(box)
";

/// A chained comparison inside a `forall` body and inside a defined
/// body: the desugared conjunction must compose in both contexts.
const CHAIN_IN_FORALL_AND_DEFINED: &str = "\
program chain_in_forall_and_defined
predicate Reading(r: Subject, level: Decimal)
predicate Sensor(s: Subject)
define in_band(r, level):
    Reading(r, level) and 0 <= level <= 100
invariant every_reading_in_band:
    forall r in Reading(r, level): 0 <= level <= 100
invariant banded_readings_exist_lawfully:
    Reading(r, level) implies in_band(r, level)
transformation record(r, level):
    admit Reading(r, level)
transformation install(s):
    admit Sensor(s)
";

/// `xor` with claim branches nested as an `implies` consequent: the
/// polarity-aware walkers meet the one construct the property
/// generators never emit.
const XOR_IN_IMPLIES: &str = "\
program xor_in_implies
predicate Case(c: Subject)
predicate Accepted(c: Subject)
predicate Declined(c: Subject)
predicate Decided(c: Subject)
invariant a_decided_case_went_exactly_one_way:
    Decided(c) implies (Accepted(c) xor Declined(c))
transformation open_case(c):
    admit Case(c)
transformation accept(c):
    require Case(c)
    admit Accepted(c)
    admit Decided(c)
transformation decline(c):
    require Case(c)
    admit Declined(c)
    admit Decided(c)
";

/// A quantity-literal chain inside a wider `and`: the chain's typed
/// literals and the flat-conjunction splice, composed.
const QTY_CHAIN_IN_AND: &str = "\
program qty_chain_in_and
predicate Tank(t: Subject, level: Decimal[L], cap: Decimal[L])
predicate Commissioned(t: Subject)
invariant levels_stay_in_band:
    Tank(t, level, cap) implies (Commissioned(t) and 0 L <= level <= cap)
transformation commission(t, cap):
    admit Commissioned(t)
    admit Tank(t, 0 L, cap)
";

/// A `value` lookup whose default is a sum, inside a comparison
/// operand: nested value expressions through the lookup's fallback.
const VALUEOF_SUM_DEFAULT: &str = "\
program valueof_sum_default
predicate Override(cap: Decimal)
predicate Entry(e: Subject, amount: Decimal)
invariant total_within_cap:
    Entry(_, _) implies sum(a | Entry(_, a)) <= (value Override(_) default sum(a | Entry(_, a)))
transformation set_override(cap):
    admit Override(cap)
transformation post(e, amount):
    admit Entry(e, amount)
";

/// A `for` body whose `require` is a defined call: the statement
/// walkers must descend the loop AND expand the call.
const FOR_WITH_DEFINED_REQUIRE: &str = "\
program for_with_defined_require
predicate Approved(item: Subject)
predicate Shipped(item: Subject)
define is_approved(i):
    Approved(i)
transformation approve(item):
    admit Approved(item)
transformation ship_all(items):
    for item in items:
        require is_approved(item)
        admit Shipped(item)
";

fn corpus() -> Vec<(&'static str, &'static str)> {
    vec![
        ("sum_through_defined_chain", SUM_THROUGH_DEFINED_CHAIN),
        ("pre_around_defined", PRE_AROUND_DEFINED),
        ("chain_in_forall_and_defined", CHAIN_IN_FORALL_AND_DEFINED),
        ("xor_in_implies", XOR_IN_IMPLIES),
        ("qty_chain_in_and", QTY_CHAIN_IN_AND),
        ("valueof_sum_default", VALUEOF_SUM_DEFAULT),
        ("for_with_defined_require", FOR_WITH_DEFINED_REQUIRE),
    ]
}

fn parsed(name: &str, source: &str) -> Program {
    parse_program(source).unwrap_or_else(|e| panic!("corpus `{name}` must parse: {e:?}"))
}

#[test]
fn every_fragment_survives_every_walker_deterministically() {
    for (name, source) in corpus() {
        let program = parsed(name, source);
        program
            .validate()
            .unwrap_or_else(|e| panic!("corpus `{name}` must validate: {e:?}"));

        // Format round-trip: the formatter's walk and the parser's
        // lowering passes agree on the whole tree, nesting included.
        let reparsed = parse_program(&format_program(&program))
            .unwrap_or_else(|e| panic!("corpus `{name}` must re-parse its rendering: {e:?}"));
        assert_eq!(program, reparsed, "`{name}`: format round-trip drifted");

        // The analysis-grade walkers run, and run the same way twice.
        let compiled = CompiledProgram::new(program.clone())
            .unwrap_or_else(|e| panic!("corpus `{name}` must compile: {e:?}"));
        assert_eq!(lints(&compiled), lints(&compiled), "`{name}`: lints drift");
        assert_eq!(
            controls(&compiled),
            controls(&compiled),
            "`{name}`: controls drift"
        );
        // CoverageReport is a pinned envelope without PartialEq; its
        // serialization is the comparable form.
        assert_eq!(
            serde_json::to_value(CoverageTracker::new(&program).into_report()).unwrap(),
            serde_json::to_value(CoverageTracker::new(&program).into_report()).unwrap(),
            "`{name}`: coverage shapes drift"
        );

        // Footprints are transitive: every invariant reaches at least
        // one predicate, however deeply the reference is nested.
        for inv in &program.invariants {
            let mut footprint = std::collections::BTreeSet::new();
            predicates_referenced_by_prop(&inv.body, &program.definitions, &mut footprint);
            assert!(
                !footprint.is_empty(),
                "`{name}`: invariant `{}` has an empty footprint - a walker \
                 stopped short of a nested reference",
                inv.name
            );
        }

        // Parameter-kind inference is total over the corpus.
        let validated = program.validated().expect("validated above");
        for t in &program.transformations {
            transformation_param_kinds(&validated, &t.name).unwrap_or_else(|e| {
                panic!("`{name}`: param kinds for `{}` must resolve: {e:?}", t.name)
            });
        }
    }
}

#[test]
fn nested_references_reach_the_footprint() {
    // The transitivity bites: predicates reachable ONLY through
    // defined calls (and through `pre`) are in the footprint.
    let program = parsed("sum_through_defined_chain", SUM_THROUGH_DEFINED_CHAIN);
    let inv = &program.invariants[0];
    let mut footprint = std::collections::BTreeSet::new();
    predicates_referenced_by_prop(&inv.body, &program.definitions, &mut footprint);
    assert!(
        footprint.contains(&PredicateName::from("Parcel")),
        "the sum's predicate is two defined calls deep: {footprint:?}"
    );

    let program = parsed("pre_around_defined", PRE_AROUND_DEFINED);
    let inv = &program.invariants[0];
    let mut footprint = std::collections::BTreeSet::new();
    predicates_referenced_by_prop(&inv.body, &program.definitions, &mut footprint);
    assert!(
        footprint.contains(&PredicateName::from("Sealed")),
        "the predicate behind pre(defined_call) is in the footprint: {footprint:?}"
    );
}

#[test]
fn the_sum_seed_resolves_through_the_defined_chain() {
    use morpholog_core::{Prop, SumSeed, ValueExpr};
    let program = parsed("sum_through_defined_chain", SUM_THROUGH_DEFINED_CHAIN);
    let Prop::Implies { right, .. } = &program.invariants[0].body else {
        panic!("implication expected");
    };
    let Prop::Compare { left, .. } = right.as_ref() else {
        panic!("comparison expected");
    };
    let ValueExpr::Sum { seed, .. } = left.as_ref() else {
        panic!("sum expected");
    };
    assert_eq!(
        *seed,
        SumSeed::Quantity("t".into()),
        "the summed variable's kind is two call frames away"
    );
}

#[test]
fn the_chained_comparison_desugars_identically_in_both_contexts() {
    // The chain inside the forall body and the chain inside the
    // defined body lower to the same flat conjunction shape.
    use morpholog_core::Prop;
    let program = parsed("chain_in_forall_and_defined", CHAIN_IN_FORALL_AND_DEFINED);
    let Prop::Forall { body, .. } = &program.invariants[0].body else {
        panic!("forall expected");
    };
    let Prop::And(in_forall) = body.as_ref() else {
        panic!("the chain desugars to a flat And inside the forall: {body:?}");
    };
    let Prop::And(in_defined) = &program.definitions[0].body else {
        panic!(
            "the chain composes flat inside the defined body's conjunction: {:?}",
            program.definitions[0].body
        );
    };
    assert_eq!(in_forall.len(), 2);
    // The defined body is `Reading(...) and <chain>` - one claim plus
    // the chain's two links, spliced flat.
    assert_eq!(in_defined.len(), 3);
}
