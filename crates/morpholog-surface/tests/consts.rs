//! Programme-level `const`: parse-time substitution across every body
//! sort, so the named and hand-inlined spellings yield the SAME
//! `Program` and the same canonical hash. Refusals are parser-side
//! with spans; nothing about a const reaches the IR.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::format::canonical_hash;
use morpholog_surface::parse_program;

fn header(rest: &str) -> String {
    format!(
        "program consts_test\n\n\
         predicate Line(l: Subject, net: Decimal)\n\
         predicate Cap(c: Subject, limit: Decimal)\n\n{rest}"
    )
}

fn assert_equivalent(sugared: &str, inlined: &str) {
    let p1 = parse_program(&header(sugared)).expect("const source parses");
    let p2 = parse_program(&header(inlined)).expect("inlined source parses");
    assert_eq!(p1, p2, "const and inlined programmes must be identical IR");
    assert_eq!(canonical_hash(&p1), canonical_hash(&p2));
}

fn refusal_containing(source: &str, needle: &str) {
    let errs = parse_program(&header(source)).expect_err("source must be refused");
    assert!(
        errs.iter().any(|e| e.message.contains(needle)),
        "expected a diagnostic containing {needle:?}, got: {:?}",
        errs.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
}

// ------------------------------------------------------------
// Equivalence: one figure, every body sort.
// ------------------------------------------------------------

#[test]
fn a_const_reaches_invariant_and_define_bodies() {
    assert_equivalent(
        "const penny = (0.01)\n\n\
         define rounded(net):\n    net = round(net, penny)\n\n\
         invariant nets_are_rounded:\n    Line(_, net) implies rounded(net)",
        "define rounded(net):\n    net = round(net, 0.01)\n\n\
         invariant nets_are_rounded:\n    Line(_, net) implies rounded(net)",
    );
}

#[test]
fn a_const_reaches_transformation_statements() {
    // require (a value position), admit (a term slot), and let.
    assert_equivalent(
        "const floor = (5)\n\n\
         transformation post(l, net):\n    \
             require floor <= net\n    \
             let spare = net - floor\n    \
             admit Line(l, spare)",
        "transformation post(l, net):\n    \
             require 5 <= net\n    \
             let spare = net - 5\n    \
             admit Line(l, spare)",
    );
}

#[test]
fn a_const_reaches_derived_clauses() {
    assert_equivalent(
        "const penny = (0.01)\n\n\
         predicate RoundedNet(l: Subject, r: Decimal)\n\n\
         derived RoundedNet(l):\n    \
             over Line(l, net)\n    \
             value r = round(net, penny)",
        "predicate RoundedNet(l: Subject, r: Decimal)\n\n\
         derived RoundedNet(l):\n    \
             over Line(l, net)\n    \
             value r = round(net, 0.01)",
    );
}

#[test]
fn a_const_may_use_earlier_consts() {
    assert_equivalent(
        "const penny = (0.01)\n\
         const half_penny = ((penny) / 2)\n\n\
         invariant nets_clear_half_a_penny:\n    Line(_, net) implies half_penny <= net",
        "invariant nets_clear_half_a_penny:\n    Line(_, net) implies (0.01 / 2) <= net",
    );
}

#[test]
fn a_const_in_a_claim_pattern_is_refused_not_filtered() {
    // The action-at-a-distance trap: substituting here would turn the
    // pattern variable into a literal filter, silently shrinking the
    // rule's universe. Refused - match a variable and compare.
    refusal_containing(
        "const house_cap = (250)\n\n\
         invariant the_house_cap_exists:\n    Cap(c, _) implies Cap(c, house_cap)",
        "stands in the `Cap` claim pattern",
    );
}

#[test]
fn the_vacuous_filter_shape_is_refused() {
    // A distant const sharing a plausible variable name must not
    // quietly rewrite `ChargeLine(_, net)` into a filter on 0.01.
    refusal_containing(
        "const net = (0.01)\n\n\
         invariant nets_are_non_negative:\n    Line(_, net) implies 0 <= net",
        "stands in the `Line` claim pattern",
    );
}

#[test]
fn a_const_in_a_bind_pattern_or_defined_call_is_refused() {
    refusal_containing(
        "const cap = (250)\n\n\
         transformation post(l, net):\n    \
             bind Cap(cap, limit)\n    \
             admit Line(l, net)",
        "stands in the `Cap` claim pattern",
    );
    // A defined call is claim-shaped until resolution, which runs
    // AFTER the const pass - so the refusal names it as a pattern.
    refusal_containing(
        "const cap = (250)\n\n\
         define within(limit):\n    Cap(_, limit)\n\n\
         invariant r:\n    Line(_, net) implies within(cap)",
        "stands in the `within` claim pattern",
    );
}

#[test]
fn constructive_and_resolved_slots_still_take_consts() {
    // admit builds a claim, retract resolves its arguments, emit
    // constructs an intent - none bind, so a const is an ordinary use.
    assert_equivalent(
        "const default_net = (10)\n\n\
         intent Posted(l: Subject, net: Decimal)\n\n\
         transformation post(l):\n    \
             admit Line(l, default_net)\n    \
             emit Posted(l, default_net)\n\n\
         transformation unpost(l):\n    \
             retract Line(l, default_net)",
        "intent Posted(l: Subject, net: Decimal)\n\n\
         transformation post(l):\n    \
             admit Line(l, 10)\n    \
             emit Posted(l, 10)\n\n\
         transformation unpost(l):\n    \
             retract Line(l, 10)",
    );
}

#[test]
fn a_const_reaches_a_nested_for_body() {
    assert_equivalent(
        "const floor = (5)\n\n\
         transformation post_all(ls):\n    \
             for l in ls:\n        \
                 require floor <= 10\n        \
                 admit Line(l, floor)",
        "transformation post_all(ls):\n    \
             for l in ls:\n        \
                 require 5 <= 10\n        \
                 admit Line(l, 5)",
    );
}

#[test]
fn a_const_composes_with_body_lets() {
    assert_equivalent(
        "const divisor = (100)\n\n\
         define net_of(rate, volume, net):\n    \
             let raw = ((rate * volume) / divisor)\n    \
             net = round(raw, 0.01)",
        "define net_of(rate, volume, net):\n    \
             net = round((rate * volume) / 100, 0.01)",
    );
}

// ------------------------------------------------------------
// Refusals.
// ------------------------------------------------------------

#[test]
fn duplicate_consts_are_refused() {
    refusal_containing(
        "const penny = (0.01)\nconst penny = (0.05)\n\n\
         invariant r:\n    Line(_, net) implies net = penny",
        "duplicate const `penny`",
    );
}

#[test]
fn actor_cannot_name_a_const() {
    refusal_containing(
        "const actor = (1)\n\n\
         invariant r:\n    Line(_, net) implies net = 1",
        "`actor` cannot name a const",
    );
}

#[test]
fn self_and_forward_references_are_refused() {
    refusal_containing(
        "const a = (a)\n\ninvariant r:\n    Line(_, net) implies net = a",
        "const `a` references itself",
    );
    refusal_containing(
        "const first = ((later) + 1)\nconst later = (1)\n\n\
         invariant r:\n    Line(_, net) implies net = first + later",
        "declared later - a const may use earlier consts only",
    );
}

#[test]
fn an_unused_const_is_refused() {
    refusal_containing(
        "const unused = (7)\n\n\
         invariant r:\n    Line(_, net) implies net = 1",
        "const `unused` is never used",
    );
}

#[test]
fn parameter_collisions_are_refused() {
    refusal_containing(
        "const net = (1)\n\n\
         transformation post(l, net):\n    admit Line(l, net)",
        "const `net` collides with a parameter",
    );
}

#[test]
fn quantifier_binder_collisions_are_refused() {
    refusal_containing(
        "const x = (1)\n\n\
         invariant r:\n    (exists x: Line(x, _)) implies Line(_, x)",
        "const `x` collides with a quantifier binding",
    );
}

#[test]
fn statement_binding_collisions_are_refused() {
    refusal_containing(
        "const spare = (1)\n\n\
         transformation post(l, net):\n    \
             let spare = net - 1\n    \
             admit Line(l, spare)",
        "const `spare` collides with a statement binding",
    );
}

#[test]
fn derived_key_collisions_are_refused() {
    refusal_containing(
        "const l = (1)\n\n\
         predicate RoundedNet(l: Subject, r: Decimal)\n\n\
         derived RoundedNet(l):\n    over Line(l, net)\n    value r = net + l",
        "const `l` collides with a derived key",
    );
}

#[test]
fn body_let_collisions_are_refused_not_shadowed() {
    // The body let would otherwise win silently (it substitutes
    // first); the collision is refused at the let, naming the const.
    refusal_containing(
        "const penny = (0.01)\n\n\
         define rounded(net):\n    \
             let penny = (0.05)\n    \
             net = round(net, penny)\n\n\
         invariant r:\n    Line(_, net) implies net = penny",
        "collides with the programme-level const",
    );
}

#[test]
fn a_computed_const_in_a_term_slot_is_refused() {
    // Patterns refuse consts outright, so the computed-value refusal
    // is now reachable only through the constructive slots.
    refusal_containing(
        "const cap = (100 + 50)\n\n\
         transformation post(l, net):\n    admit Line(l, cap)",
        "computed const `cap`",
    );
}

#[test]
fn an_open_initialiser_is_refused() {
    // A free variable would capture whichever local exists at each
    // use site - an unhygienic macro, not a constant.
    refusal_containing(
        "const uplifted = (rate + 0.01)\n\n\
         transformation post(l, rate):\n    \
             let result = uplifted\n    \
             admit Line(l, result)",
        "references `rate`, which is not a const",
    );
}

#[test]
fn actor_and_wildcards_cannot_be_const_values() {
    refusal_containing(
        "const proposer = (actor)\n\n\
         transformation post(l, net):\n    \
             require proposer = proposer\n    \
             admit Line(l, net)",
        "references `actor`, which varies with every proposal",
    );
    refusal_containing(
        "const anything = (_)\n\n\
         invariant r:\n    Line(_, net) implies net = anything",
        "contains a wildcard",
    );
}

#[test]
fn a_state_reading_initialiser_is_refused() {
    // sum/value read the ledger; a figure that changes with state is
    // a rule's job, not a const's.
    refusal_containing(
        "const total = (sum(n | Line(_, n)))\n\n\
         invariant r:\n    Line(_, net) implies net <= total",
        "reads state",
    );
    refusal_containing(
        "const cap = (value Cap(_, _))\n\n\
         invariant r:\n    Line(_, net) implies net <= cap",
        "reads state",
    );
}
