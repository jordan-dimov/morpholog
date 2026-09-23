//! Body-level `let` in `define` and `invariant` bodies: a source using lets and its hand-inlined
//! twin give the same `Program`. Refusals carry spans; nothing of a `let` reaches the IR.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::format::canonical_hash;
use morpholog_surface::parse_program;

fn header(rest: &str) -> String {
    format!(
        "program lets_test\n\n\
         predicate Committed(inv: Subject, amount: Decimal)\n\
         predicate Reading(meter: Subject, kwh: Decimal)\n\n{rest}"
    )
}

fn assert_equivalent(sugared: &str, desugared: &str) {
    let p1 = parse_program(&header(sugared)).expect("sugared source parses");
    let p2 = parse_program(&header(desugared)).expect("desugared source parses");
    assert_eq!(
        p1, p2,
        "sugared and desugared programmes must be identical IR"
    );
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
// Equivalence: the substitution IS the semantics.
// ------------------------------------------------------------

#[test]
fn single_let_in_define_body_desugars_by_substitution() {
    assert_equivalent(
        "define shifted_ok(a, committed):\n    \
             let shifted = ((a) + 0.005)\n    \
             committed = (shifted)",
        "define shifted_ok(a, committed):\n    \
             committed = ((a) + 0.005)",
    );
}

#[test]
fn later_let_may_use_an_earlier_let() {
    assert_equivalent(
        "define rounded_ok(a, b, divisor, committed):\n    \
             let raw = ((a * b) / divisor)\n    \
             let shifted = ((raw) + 0.005)\n    \
             committed = ((shifted) - ((shifted) % 0.01))",
        "define rounded_ok(a, b, divisor, committed):\n    \
             committed = ((((a * b) / divisor) + 0.005) - \
                          ((((a * b) / divisor) + 0.005) % 0.01))",
    );
}

#[test]
fn let_in_invariant_body_desugars_identically() {
    assert_equivalent(
        "invariant cap:\n    \
             let ceiling = (100 + 20)\n    \
             forall r in Reading(m, kwh):\n        \
             kwh <= (ceiling)",
        "invariant cap:\n    \
             forall r in Reading(m, kwh):\n        \
             kwh <= (100 + 20)",
    );
}

#[test]
fn term_valued_let_flows_into_a_claim_argument_position() {
    // A let bound to a plain term may stand where only terms stand.
    assert_equivalent(
        "invariant pinned:\n    \
             let who = (m)\n    \
             Reading(who, kwh) implies kwh <= 100",
        "invariant pinned:\n    \
             Reading(m, kwh) implies kwh <= 100",
    );
}

#[test]
fn canonical_hash_is_identical_for_sugared_and_desugared() {
    // The hash is over the inlined form, so naming a value cannot change the rules.
    let sugared = parse_program(&header(
        "define rounded_ok(a, b, divisor, committed):\n    \
             let raw = ((a * b) / divisor)\n    \
             committed = ((raw) + 0.005)",
    ))
    .expect("parses");
    let desugared = parse_program(&header(
        "define rounded_ok(a, b, divisor, committed):\n    \
             committed = (((a * b) / divisor) + 0.005)",
    ))
    .expect("parses");
    assert_eq!(canonical_hash(&sugared), canonical_hash(&desugared));
}

#[test]
fn substitution_is_algebraic_not_hygienic() {
    // A let value mentioning `m`, used under `forall ... Reading(m, _)`, reads the quantified
    // `m`: a let is inlined text, not a closure. Deliberate.
    assert_equivalent(
        "invariant scaled:\n    \
             let doubled = (kwh * 2)\n    \
             forall r in Reading(m, kwh):\n        \
             (doubled) <= 100",
        "invariant scaled:\n    \
             forall r in Reading(m, kwh):\n        \
             (kwh * 2) <= 100",
    );
}

#[test]
fn inline_body_takes_no_lets() {
    // Lets belong to the indented form only.
    let errs = parse_program(&header("invariant cap: let c = (1)\n"))
        .expect_err("inline let must be refused");
    assert!(!errs.is_empty());
}

// ------------------------------------------------------------
// Refusals. Each names the let and says what to do instead.
// ------------------------------------------------------------

#[test]
fn duplicate_let_name_is_refused() {
    refusal_containing(
        "define f(a, committed):\n    \
             let raw = ((a) + 1)\n    \
             let raw = ((a) + 2)\n    \
             committed = (raw)",
        "duplicate let `raw`",
    );
}

#[test]
fn let_colliding_with_a_parameter_is_refused() {
    refusal_containing(
        "define f(a, committed):\n    \
             let a = ((committed) + 1)\n    \
             committed = (a)",
        "collides with a parameter",
    );
}

#[test]
fn let_colliding_with_a_quantifier_binder_is_refused() {
    // Shadowing a `forall` binder is refused. Claim-bound variables do not count (see above).
    refusal_containing(
        "invariant cap:\n    \
             let r = (100)\n    \
             forall r in Reading(m, kwh):\n        \
             kwh <= (r)",
        "collides with a quantifier binding",
    );
}

#[test]
fn term_valued_let_flows_into_a_sum_target() {
    // A sum target is not a binder, so a term-valued let substitutes into it.
    assert_equivalent(
        "define f(total):\n    \
             let selected = (kwh)\n    \
             total = (sum(selected | Reading(m, kwh)))",
        "define f(total):\n    \
             total = (sum(kwh | Reading(m, kwh)))",
    );
}

#[test]
fn self_referential_let_is_refused() {
    // Even `let a = (a)`, which would otherwise vanish silently.
    refusal_containing(
        "define f(a):\n    \
             let raw = (raw)\n    \
             Reading(m, raw)",
        "let `raw` references itself",
    );
}

#[test]
fn forward_reference_is_refused() {
    // Refused whether or not the body also uses the later let directly.
    for body in ["committed = ((first) + (later))", "committed = (first)"] {
        refusal_containing(
            &format!(
                "define f(x, committed):\n    \
                     let first = ((later) + 1)\n    \
                     let later = ((x) + 1)\n    \
                     {body}"
            ),
            "let `first` references `later`, which is declared later",
        );
    }
}

#[test]
fn actor_cannot_name_a_let() {
    // `actor` always means the actor, so a let by that name could never be used.
    refusal_containing(
        "define f(a, committed):\n    \
             let actor = ((a) + 1)\n    \
             committed = (a)",
        "`actor` cannot name a let value",
    );
}

#[test]
fn unused_let_is_refused() {
    refusal_containing(
        "define f(a, committed):\n    \
             let unused = ((a) + 1)\n    \
             committed = (a)",
        "let `unused` is never used",
    );
}

#[test]
fn transitively_dead_let_chain_is_refused_whole() {
    // `head` is used only by the unused `tail`, so both are refused.
    let errs = parse_program(&header(
        "define f(a, committed):\n    \
             let head = ((a) + 1)\n    \
             let tail = ((head) * 2)\n    \
             committed = (a)",
    ))
    .expect_err("dead chain must be refused");
    for name in ["head", "tail"] {
        assert!(
            errs.iter()
                .any(|e| e.message.contains(&format!("let `{name}` is never used"))),
            "expected `{name}` reported dead, got: {:?}",
            errs.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
    }
}

// Computed lets in term-only positions. Each body puts `let net = (a + 1)` in a different slot
// that takes terms only.
#[test]
fn computed_let_is_refused_in_every_term_only_position() {
    let cases: &[(&str, &str)] = &[
        (
            "claim argument",
            "define f(a):\n    \
                 let net = ((a) + 1)\n    \
                 Reading(m, net)",
        ),
        (
            "defined-call argument",
            "define g(x):\n    Reading(x, 1)\n\n\
             define f(a):\n    \
                 let net = ((a) + 1)\n    \
                 g(net)",
        ),
        (
            "membership operand",
            "define f(a, xs):\n    \
                 let net = ((a) + 1)\n    \
                 net in xs",
        ),
        (
            "value-lookup argument",
            "define f(a, committed):\n    \
                 let net = ((a) + 1)\n    \
                 committed = (value Reading(net, _))",
        ),
    ];
    for (position, source) in cases {
        let errs =
            parse_program(&header(source)).expect_err("computed let in term slot must refuse");
        assert!(
            errs.iter()
                .any(|e| e.message.contains("computed let `net`")
                    && e.message.contains("plain terms")),
            "{position}: expected the computed-let refusal, got: {:?}",
            errs.iter().map(|e| &e.message).collect::<Vec<_>>()
        );
    }
}

#[test]
fn computed_let_substitutes_into_a_sum_target() {
    // A sum target takes any value, so a computed let is fine there.
    assert_equivalent(
        "define f(a, total):\n    \
             let net = ((a) + 1)\n    \
             total = (sum(net | Reading(m, kwh)))",
        "define f(a, total):\n    \
             total = (sum((a) + 1 | Reading(m, kwh)))",
    );
}

#[test]
fn term_slot_refusal_inside_a_later_let_names_the_referenced_let() {
    // The diagnostic must blame `net`, whose value lands in the slot, not the let using it.
    let errs = parse_program(&header(
        "define f(a, total):\n    \
             let net = ((a) + 1)\n    \
             let row = (sum(1 | Reading(m, net)))\n    \
             total = (row)",
    ))
    .expect_err("computed let in a later let's term slot must refuse");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("computed let `net`")),
        "expected the refusal to name `net`, got: {:?}",
        errs.iter().map(|e| &e.message).collect::<Vec<_>>()
    );
    assert!(
        !errs
            .iter()
            .any(|e| e.message.contains("computed let `row`")),
        "the refusal must not blame the let being expanded into"
    );
}

// ------------------------------------------------------------
// Growth guards.
// ------------------------------------------------------------

#[test]
fn a_long_term_valued_chain_expands_linearly() {
    // Small enough for both guards; this checks that a long chain expands in linear time. A
    // quadratic expansion would grind on it.
    let mut body = String::from("    let a0 = (x)\n");
    for i in 1..10_000 {
        body.push_str(&format!("    let a{i} = (a{})\n", i - 1));
    }
    body.push_str("    committed = (a9999)");
    let sugared = format!("define f(x, committed):\n{body}");
    let p1 = parse_program(&header(&sugared)).expect("chain parses");
    let p2 = parse_program(&header("define f(x, committed):\n    committed = (x)"))
        .expect("desugared parses");
    assert_eq!(p1, p2, "the chain must collapse to its root");
}

#[test]
fn term_slot_refusal_wins_over_the_budget_projection() {
    // Term-slot uses never grow the tree, so a large body must still report the term-slot
    // refusal, not a budget error.
    let conjuncts = vec!["Reading(m, net)"; 4000].join(" and ");
    let source = format!("define f(a):\n    let net = ((a) + 1)\n    {conjuncts}");
    let errs = parse_program(&header(&source)).expect_err("computed let in term slots refuses");
    assert!(
        errs.iter()
            .any(|e| e.message.contains("computed let `net`")),
        "expected the term-slot refusal"
    );
    assert!(
        !errs.iter().any(|e| e.message.contains("expression budget")),
        "the budget projection must not count term-slot occurrences"
    );
}

#[test]
fn shallow_exponential_let_chain_is_refused_by_the_node_budget() {
    // Each let doubles the size while depth grows by one, which MAX_EXPR_DEPTH cannot see.
    // The budget refuses before the blowup is allocated.
    let mut body = String::from("    let a0 = ((x + x) + (x + x))\n");
    for i in 1..24 {
        body.push_str(&format!(
            "    let a{i} = ((a{} + a{}) + (a{} + a{}))\n",
            i - 1,
            i - 1,
            i - 1,
            i - 1
        ));
    }
    body.push_str("    committed = (a23)");
    let source = format!("define f(x, committed):\n{body}");
    refusal_containing(&source, "past the expression budget");
}

#[test]
fn depth_from_substitution_is_caught_by_validation() {
    // Within the size budget but nested past MAX_EXPR_DEPTH: validation still catches it.
    let mut body = String::from("    let a0 = ((x) + 1)\n");
    for i in 1..300 {
        body.push_str(&format!("    let a{i} = ((a{}) + 1)\n", i - 1));
    }
    body.push_str("    committed = (a299)");
    let source = format!("define f(x, committed):\n{body}");
    let program = parse_program(&header(&source)).expect("parses within the node budget");
    let errs = program.validate().expect_err("must trip the depth guard");
    assert!(
        errs.iter().any(|e| e.to_string().contains("maximum depth")),
        "expected NestingTooDeep, got: {errs:?}"
    );
}

// ------------------------------------------------------------
// Grammar edges.
// ------------------------------------------------------------

#[test]
fn unparenthesised_let_value_is_refused_loudly() {
    // Parens are required so `let cap = 100` followed by `amount <= cap` cannot read
    // `100 amount` as a quantity.
    let errs = parse_program(&header(
        "invariant cap:\n    \
             let ceiling = 100\n    \
             forall r in Reading(m, kwh):\n        \
             kwh <= (ceiling)",
    ))
    .expect_err("unparenthesised value must be refused");
    assert!(!errs.is_empty());
}

#[test]
fn let_value_spans_lines_freely_inside_its_parens() {
    // Inside the parens, a long value can break across lines.
    assert_equivalent(
        "define f(a, b, committed):\n    \
             let raw = ((a * b)\n\
                        / 100)\n    \
             committed = (raw)",
        "define f(a, b, committed):\n    \
             committed = ((a * b) / 100)",
    );
}
