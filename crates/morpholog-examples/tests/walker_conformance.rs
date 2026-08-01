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

/// `pre(...)` wrapping a defined call, once in a consequent and once
/// in an antecedent: the pre-detector and the footprint walkers must
/// see through the wrapper AND the call - and the two positions cue
/// coverage differently on purpose.
const PRE_AROUND_DEFINED: &str = "\
program pre_around_defined
predicate Sealed(box: Subject)
predicate Opened(box: Subject)
predicate Logged(box: Subject)
define was_sealed(b):
    Sealed(b)
invariant opening_needs_a_prior_seal:
    Opened(b) implies pre(was_sealed(b))
invariant a_prior_seal_is_logged:
    pre(was_sealed(b)) implies Logged(b)
transformation seal(box):
    admit Sealed(box)
    admit Logged(box)
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

/// An invariant whose antecedent is a defined call over a predicate no
/// transformation admits: the unsupplied-antecedent lint fires only if
/// the lint walker descends into the definition body.
const UNSUPPLIED_THROUGH_DEFINED: &str = "\
program unsupplied_through_defined
predicate Ghost(g: Subject)
predicate Real(r: Subject)
define haunted(g):
    Ghost(g)
invariant haunting_is_real:
    haunted(g) implies Real(g)
transformation materialise(r):
    admit Real(r)
";

/// A define body opening with `let` lines: the sugar is substituted
/// away at parse time, so every walker must see the desugared body -
/// and see it identically to the hand-expanded spelling.
const LET_SUGARED_DEFINE: &str = "\
program let_sugared_define
predicate Line(l: Subject, rate: Decimal, volume: Decimal, net: Decimal)
define rounded_net(rate, volume, net):
    let raw = ((rate * volume) / 100)
    let shifted = ((raw) + 0.005)
    net = ((shifted) - ((shifted) % 0.01))
invariant net_is_the_rounded_recompute:
    Line(_, rate, volume, net) implies rounded_net(rate, volume, net)
transformation post(l, rate, volume, net):
    admit Line(l, rate, volume, net)
";

/// `round` nested in arithmetic inside a let-sugared invariant, plus a
/// literal-quantum boundary: the new node meets every walker in the
/// context the billing example actually uses it.
const ROUND_IN_LET_SUGARED_BODY: &str = "\
program round_in_let_sugared_body
predicate Line(l: Subject, rate: Decimal, volume: Decimal, net: Decimal)
invariant net_is_the_rounded_recompute:
    let raw = ((rate * volume) / 100)
    Line(_, rate, volume, net) implies net = round(raw, 0.01)
transformation post(l, rate, volume, net):
    admit Line(l, rate, volume, net)
";

/// A programme-level `const` reaching an invariant, a define, and a
/// transformation statement: substituted away at parse time, so every
/// walker must see the inlined bodies - identically to the
/// hand-inlined spelling.
const CONST_ACROSS_BODY_SORTS: &str = "\
program const_across_body_sorts
const penny = (0.01)
predicate Line(l: Subject, net: Decimal)
define rounded(net):
    net = round(net, penny)
invariant nets_are_rounded:
    Line(_, net) implies rounded(net)
transformation post(l, net):
    require penny <= net
    admit Line(l, net)
";

/// A `span(P3M)` calendar-span literal and a date subtraction nested
/// in a let-sugared invariant body and reached through a defined call:
/// the new literal kind meets every walker in the contexts the
/// covenant example actually uses it.
const SPAN_IN_DATE_ARITHMETIC: &str = "\
program span_in_date_arithmetic
predicate Period(p: Subject, ends_on: Date)
predicate Notice(p: Subject, as_of: Date, days_late: Decimal)
define lateness_exact(as_of, ends_on, days_late):
    days_late = as_of - (ends_on + span(P45D))
invariant lateness_is_the_records_own_count:
    Notice(p, as_of, days_late) and Period(p, ends_on) implies lateness_exact(as_of, ends_on, days_late)
invariant notices_come_after_the_deadline:
    let deadline = (ends_on + span(P45D))
    Notice(p, as_of, _) and Period(p, ends_on) implies deadline before as_of
transformation notice(p, as_of, days_late):
    require Period(p, ends_on) and (ends_on + span(P45D)) before as_of
    admit Notice(p, as_of, days_late)
";

/// `if(...)` in a let-sugared invariant with a defined call inside
/// the condition and a `sum` inside a branch: the new node meets
/// every walker in the contexts the scoped-charges example uses it,
/// with each of the three children carrying a predicate the others
/// do not (so a walker that skips one child reddens the targeted
/// footprint assertion below, not just the generic sweep).
const COND_ACROSS_CHILDREN: &str = "\
program cond_across_children
predicate OnlyWhen(w: Subject)
predicate OnlyThen(t: Subject, amount: Decimal)
predicate OnlyOtherwise(o: Subject, fallback: Decimal)
predicate Out(x: Subject, v: Decimal)
define armed(w):
    OnlyWhen(w)
invariant picked_is_lawful:
    let fallback_total = (sum(f | OnlyOtherwise(_, f)))
    Out(x, v) implies v = if(armed(x), sum(a | OnlyThen(_, a)), fallback_total)
transformation record(x, v):
    admit Out(x, v)
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
        ("unsupplied_through_defined", UNSUPPLIED_THROUGH_DEFINED),
        ("let_sugared_define", LET_SUGARED_DEFINE),
        ("round_in_let_sugared_body", ROUND_IN_LET_SUGARED_BODY),
        ("const_across_body_sorts", CONST_ACROSS_BODY_SORTS),
        ("span_in_date_arithmetic", SPAN_IN_DATE_ARITHMETIC),
        ("cond_across_children", COND_ACROSS_CHILDREN),
    ]
}

/// The conditional's three children each carry a predicate the others
/// do not; a broken walker that visits the condition but skips a
/// branch (or vice versa) fails HERE, where the generic sweep's
/// non-empty footprint check would still pass.
#[test]
fn the_conditional_footprint_carries_all_three_children() {
    let program = parsed("cond_across_children", COND_ACROSS_CHILDREN);
    let invariant = program
        .invariants
        .iter()
        .find(|i| i.name.as_str() == "picked_is_lawful")
        .expect("the fragment's invariant");
    let mut refs = std::collections::BTreeSet::new();
    morpholog_core::predicates_referenced_by_prop(&invariant.body, &program.definitions, &mut refs);
    for expected in ["OnlyWhen", "OnlyThen", "OnlyOtherwise"] {
        assert!(
            refs.iter().any(|p| p.as_str() == expected),
            "`{expected}` must be in the footprint (child-specific); got {refs:?}"
        );
    }
}

/// A sum nested in a conditional branch still receives its typed seed
/// from `lower_sum_seeds` - the lowering descends both branches.
#[test]
fn a_sum_inside_a_branch_receives_its_seed() {
    use morpholog_core::{Prop, SumSeed, ValueExpr};
    let program = parsed("cond_across_children", COND_ACROSS_CHILDREN);
    let invariant = program
        .invariants
        .iter()
        .find(|i| i.name.as_str() == "picked_is_lawful")
        .expect("the fragment's invariant");
    // Walk to the conditional's `then` branch by the fragment's known
    // shape (let-substituted: Implies { Out(..), v = if(..) }): the
    // sum over a Decimal-declared position keeps the decimal seed -
    // the point is that lowering REACHED it (an unlowered sum in a
    // quantity position elsewhere would keep a wrong default
    // silently).
    let Prop::Implies { right, .. } = &invariant.body else {
        panic!("fragment shape: implies; got {:?}", invariant.body);
    };
    let Prop::Eq(_, rhs) = right.as_ref() else {
        panic!("fragment shape: v = if(..); got {right:?}");
    };
    let ValueExpr::Cond { then, .. } = rhs.as_ref() else {
        panic!("fragment shape: a conditional; got {rhs:?}");
    };
    let ValueExpr::Sum { seed, .. } = then.as_ref() else {
        panic!("the then branch is a sum; got {then:?}");
    };
    assert_eq!(*seed, SumSeed::Decimal);
}

fn parsed(name: &str, source: &str) -> Program {
    let program =
        parse_program(source).unwrap_or_else(|e| panic!("corpus `{name}` must parse: {e:?}"));
    program
        .validate()
        .unwrap_or_else(|e| panic!("corpus `{name}` must validate: {e:?}"));
    program
}

#[test]
fn every_fragment_survives_every_walker_deterministically() {
    for (name, source) in corpus() {
        let program = parsed(name, source);

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

#[test]
fn the_pre_detector_sees_through_the_wrapping() {
    // needs_pre_state cues coverage to carry the previous state on
    // every replay step, and it is ANTECEDENT-only by design: replay
    // evaluates antecedents to decide firing and never evaluates
    // consequents, so `pre` in a consequent must not cue it while
    // `pre(defined_call(...))` in an antecedent must - through both
    // the wrapper and the call. The whole-body scan behind the
    // scorer's pre-gate sees both. The xor fragment is the negative
    // control: no pre anywhere, no cue.
    let program = parsed("pre_around_defined", PRE_AROUND_DEFINED);
    assert!(CoverageTracker::new(&program).needs_pre_state());
    assert_eq!(
        morpholog_core::invariants_using_pre(&program),
        vec![
            "opening_needs_a_prior_seal".to_string(),
            "a_prior_seal_is_logged".to_string(),
        ]
    );
    let no_pre = parsed("xor_in_implies", XOR_IN_IMPLIES);
    assert!(!CoverageTracker::new(&no_pre).needs_pre_state());
}

#[test]
fn the_statement_read_footprint_descends_the_loop_and_the_call() {
    // The predicate consulted by `require is_approved(item)` inside the
    // `for` body is read through both a statement nesting AND a defined
    // call - scoped loading that misses it evaluates against claims
    // that were never loaded.
    let program = parsed("for_with_defined_require", FOR_WITH_DEFINED_REQUIRE);
    let ship_all = program
        .transformations
        .iter()
        .find(|t| t.name.as_str() == "ship_all")
        .unwrap();
    let mut read = std::collections::BTreeSet::new();
    for stmt in &ship_all.body {
        morpholog_core::predicates_read_by_stmt(stmt, &program.definitions, &mut read);
    }
    assert!(
        read.contains(&PredicateName::from("Approved")),
        "the read footprint stops short of the nested require: {read:?}"
    );
}

#[test]
fn parameter_kinds_resolve_exactly_through_the_chain() {
    // Not merely Ok: the flow into a quantity-kinded claim position
    // must surface the unit. An inference that answered Subject for
    // everything would still "resolve".
    use morpholog_core::ParamKind;
    let program = parsed("qty_chain_in_and", QTY_CHAIN_IN_AND);
    let validated = program.validated().expect("validated in parsed()");
    let kinds = transformation_param_kinds(&validated, &"commission".into()).unwrap();
    let rendered: Vec<(String, ParamKind)> =
        kinds.into_iter().map(|(v, k)| (v.to_string(), k)).collect();
    assert_eq!(
        rendered,
        vec![
            (
                "t".to_string(),
                ParamKind::Concrete(morpholog_core::PredicateArgKind::Subject)
            ),
            (
                "cap".to_string(),
                ParamKind::Concrete(morpholog_core::PredicateArgKind::Quantity("L".into()))
            ),
        ]
    );
}

#[test]
fn the_implication_shape_is_recognised_through_the_xor_consequent() {
    // With nothing observed, an implication-shaped invariant reports
    // never-fired; only a walker that failed to see the implication
    // through its xor consequent would classify it always-on.
    use morpholog_core::CoverageVerdict;
    let program = parsed("xor_in_implies", XOR_IN_IMPLIES);
    let report = CoverageTracker::new(&program).into_report();
    let inv = report
        .invariants
        .iter()
        .find(|i| i.invariant == "a_decided_case_went_exactly_one_way")
        .unwrap();
    assert!(
        matches!(inv.verdict, CoverageVerdict::NeverFired),
        "implication through xor mis-classified: {:?}",
        inv.verdict
    );
}

#[test]
fn the_unsupplied_antecedent_lint_descends_the_definition() {
    // The antecedent's dependence on the never-admitted predicate is
    // visible only inside the definition body: an empty lint report
    // here means the lint walker stopped at the call.
    use morpholog_core::Lint;
    let program = parsed("unsupplied_through_defined", UNSUPPLIED_THROUGH_DEFINED);
    let compiled = CompiledProgram::new(program).unwrap();
    let findings = lints(&compiled);
    assert!(
        findings.iter().any(|l| matches!(
            l,
            Lint::UnsuppliedAntecedent { invariant, missing }
                if invariant == "haunting_is_real"
                    && missing.contains(&"Ghost".to_string())
        )),
        "the lint never descended into the definition: {findings:?}"
    );
}
