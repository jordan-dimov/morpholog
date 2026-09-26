use super::*;
use morpholog_core::ir_builder as b;
use morpholog_test_support::{claim_instance, dec, subj};

fn ledger_program() -> morpholog_core::Program {
    morpholog_examples::double_entry_ledger::program()
}

fn compiled(program: &morpholog_core::Program) -> CompiledInvariantSet {
    compile_invariants(program.validated().expect("gallery programme validates"))
        .expect("every invariant is in the fragment")
}

fn refusals(program: &morpholog_core::Program) -> Vec<CompileRefusal> {
    compile_invariants(program.validated().expect("programme validates"))
        .expect_err("expected at least one refusal")
}

#[test]
fn ledger_compiles_fully_in_programme_order() {
    let program = ledger_program();
    let set = compiled(&program);
    let names: Vec<&str> = set.invariants.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "supersedes_unique_by_prior_entry_id",
            "balanced_posted_entry",
            "journal_entry_has_lines"
        ],
        "generated discipline invariant first, then authored order"
    );
}

#[test]
fn balanced_posted_entry_sql_is_pinned() {
    let program = ledger_program();
    let set = compiled(&program);
    let sql = set.invariants[1].violation_sql(None);
    assert_eq!(
        sql,
        r#"/* morpholog compiled invariant balanced_posted_entry v1 stage1 */
SELECT (t0.arguments -> 0)::text AS "w_entry"
FROM morpholog.claims t0, LATERAL (SELECT COALESCE(sum((CASE WHEN (t1.arguments -> 2 ->> 'type') = 'decimal' THEN (t1.arguments -> 2 ->> 'value')::numeric END)), 0::numeric) AS s, COALESCE(bool_or((t1.arguments -> 2 ->> 'type') IS DISTINCT FROM 'decimal'), false) AS f FROM morpholog.claims t1 WHERE t1.predicate_name = 'JournalLine' AND (morpholog.claim_digest(morpholog.value_key_v1(t0.arguments -> 0))) = (morpholog.claim_digest(morpholog.value_key_v1(t1.arguments -> 0)))) l2, LATERAL (SELECT COALESCE(sum((CASE WHEN (t3.arguments -> 3 ->> 'type') = 'decimal' THEN (t3.arguments -> 3 ->> 'value')::numeric END)), 0::numeric) AS s, COALESCE(bool_or((t3.arguments -> 3 ->> 'type') IS DISTINCT FROM 'decimal'), false) AS f FROM morpholog.claims t3 WHERE t3.predicate_name = 'JournalLine' AND (morpholog.claim_digest(morpholog.value_key_v1(t0.arguments -> 0))) = (morpholog.claim_digest(morpholog.value_key_v1(t3.arguments -> 0)))) l4
WHERE t0.predicate_name = 'JournalEntry'
  AND ((l2.f OR NOT (min_scale(l2.s) <= 28 AND abs(l2.s) * power(10::numeric, min_scale(l2.s)) < 79228162514264337593543950336::numeric) OR l4.f OR NOT (min_scale(l4.s) <= 28 AND abs(l4.s) * power(10::numeric, min_scale(l4.s)) < 79228162514264337593543950336::numeric)) OR NOT (l2.s) = (l4.s))
ORDER BY (morpholog.value_key_v1(t0.arguments -> 0))::text
LIMIT 1"#
    );
    // Asked only once the query above returned a violation: the first
    // entry in scope, in the kernel's order, whose lines hold a value a
    // sum cannot take, or whose total no decimal can hold.
    assert_eq!(
        set.invariants[1].error_sqls(None, &[]).unwrap(),
        [
            r#"SELECT CASE WHEN l2.f THEN 'sum_kind' WHEN NOT (min_scale(l2.s) <= 28 AND abs(l2.s) * power(10::numeric, min_scale(l2.s)) < 79228162514264337593543950336::numeric) THEN 'range' WHEN l4.f THEN 'sum_kind' WHEN NOT (min_scale(l4.s) <= 28 AND abs(l4.s) * power(10::numeric, min_scale(l4.s)) < 79228162514264337593543950336::numeric) THEN 'range' END AS "kind",
       NULL::text AS "domain",
       CASE WHEN l2.f THEN (SELECT (t1.arguments -> 2)::text FROM morpholog.claims t1 WHERE t1.predicate_name = 'JournalLine' AND (morpholog.claim_digest(morpholog.value_key_v1(t0.arguments -> 0))) = (morpholog.claim_digest(morpholog.value_key_v1(t1.arguments -> 0))) AND (t1.arguments -> 2 ->> 'type') IS DISTINCT FROM 'decimal' ORDER BY t1.arguments_hash LIMIT 1) WHEN l4.f THEN (SELECT (t3.arguments -> 3)::text FROM morpholog.claims t3 WHERE t3.predicate_name = 'JournalLine' AND (morpholog.claim_digest(morpholog.value_key_v1(t0.arguments -> 0))) = (morpholog.claim_digest(morpholog.value_key_v1(t3.arguments -> 0))) AND (t3.arguments -> 3 ->> 'type') IS DISTINCT FROM 'decimal' ORDER BY t3.arguments_hash LIMIT 1) END AS "left",
       NULL::text AS "right",
       CASE WHEN l2.f THEN (SELECT ((t1.arguments -> 2 ->> 'type') IS DISTINCT FROM 'decimal') FROM morpholog.claims t1 WHERE t1.predicate_name = 'JournalLine' AND (morpholog.claim_digest(morpholog.value_key_v1(t0.arguments -> 0))) = (morpholog.claim_digest(morpholog.value_key_v1(t1.arguments -> 0))) ORDER BY t1.arguments_hash LIMIT 1) WHEN l4.f THEN (SELECT ((t3.arguments -> 3 ->> 'type') IS DISTINCT FROM 'decimal') FROM morpholog.claims t3 WHERE t3.predicate_name = 'JournalLine' AND (morpholog.claim_digest(morpholog.value_key_v1(t0.arguments -> 0))) = (morpholog.claim_digest(morpholog.value_key_v1(t3.arguments -> 0))) ORDER BY t3.arguments_hash LIMIT 1) END AS "first"
FROM morpholog.claims t0, LATERAL (SELECT COALESCE(sum((CASE WHEN (t1.arguments -> 2 ->> 'type') = 'decimal' THEN (t1.arguments -> 2 ->> 'value')::numeric END)), 0::numeric) AS s, COALESCE(bool_or((t1.arguments -> 2 ->> 'type') IS DISTINCT FROM 'decimal'), false) AS f FROM morpholog.claims t1 WHERE t1.predicate_name = 'JournalLine' AND (morpholog.claim_digest(morpholog.value_key_v1(t0.arguments -> 0))) = (morpholog.claim_digest(morpholog.value_key_v1(t1.arguments -> 0)))) l2, LATERAL (SELECT COALESCE(sum((CASE WHEN (t3.arguments -> 3 ->> 'type') = 'decimal' THEN (t3.arguments -> 3 ->> 'value')::numeric END)), 0::numeric) AS s, COALESCE(bool_or((t3.arguments -> 3 ->> 'type') IS DISTINCT FROM 'decimal'), false) AS f FROM morpholog.claims t3 WHERE t3.predicate_name = 'JournalLine' AND (morpholog.claim_digest(morpholog.value_key_v1(t0.arguments -> 0))) = (morpholog.claim_digest(morpholog.value_key_v1(t3.arguments -> 0)))) l4
WHERE (t0.predicate_name = 'JournalEntry' AND (l2.f OR NOT (min_scale(l2.s) <= 28 AND abs(l2.s) * power(10::numeric, min_scale(l2.s)) < 79228162514264337593543950336::numeric) OR l4.f OR NOT (min_scale(l4.s) <= 28 AND abs(l4.s) * power(10::numeric, min_scale(l4.s)) < 79228162514264337593543950336::numeric)))
ORDER BY t0.arguments_hash
LIMIT 1"#
        ]
    );
    assert!(set.invariants[0].error_sqls(None, &[]).unwrap().is_empty());
    // Bounded to the obligation, like the violation query.
    let bounded = set.invariants[1]
        .error_sqls(Some("((t0.arguments -> 0 ->> 'value') = 'e42')"), &[])
        .unwrap()
        .pop()
        .unwrap();
    assert!(bounded.ends_with(
        "\n  AND (((t0.arguments -> 0 ->> 'value') = 'e42'))\nORDER BY t0.arguments_hash\nLIMIT 1"
    ));
}

#[test]
fn supersedes_uniqueness_sql_is_pinned() {
    let program = ledger_program();
    let set = compiled(&program);
    let sql = set.invariants[0].violation_sql(None);
    assert_eq!(
        sql,
        r#"/* morpholog compiled invariant supersedes_unique_by_prior_entry_id v1 stage1 */
SELECT (t0.arguments -> 0)::text AS "w_new_entry_id_a",
       (t1.arguments -> 0)::text AS "w_new_entry_id_b",
       (t0.arguments -> 1)::text AS "w_prior_entry_id"
FROM morpholog.claims t0, morpholog.claims t1
WHERE t0.predicate_name = 'Supersedes'
  AND t1.predicate_name = 'Supersedes'
  AND (morpholog.claim_digest(morpholog.value_key_v1(t0.arguments -> 1))) = (morpholog.claim_digest(morpholog.value_key_v1(t1.arguments -> 1)))
  AND NOT ((morpholog.value_key_v1(t0.arguments -> 0)) = (morpholog.value_key_v1(t1.arguments -> 0)))
ORDER BY (morpholog.value_key_v1(t0.arguments -> 0))::text, (morpholog.value_key_v1(t1.arguments -> 0))::text, (morpholog.value_key_v1(t0.arguments -> 1))::text
LIMIT 1"#
    );
}

#[test]
fn journal_entry_has_lines_sql_is_pinned() {
    let program = ledger_program();
    let set = compiled(&program);
    let sql = set.invariants[2].violation_sql(None);
    assert_eq!(
        sql,
        r#"/* morpholog compiled invariant journal_entry_has_lines v1 stage1 */
SELECT (t0.arguments -> 0)::text AS "w_entry"
FROM morpholog.claims t0
WHERE t0.predicate_name = 'JournalEntry'
  AND NOT EXISTS (SELECT 1 FROM morpholog.claims t1 WHERE t1.predicate_name = 'JournalLine' AND (morpholog.claim_digest(morpholog.value_key_v1(t0.arguments -> 0))) = (morpholog.claim_digest(morpholog.value_key_v1(t1.arguments -> 0))))
ORDER BY (morpholog.value_key_v1(t0.arguments -> 0))::text
LIMIT 1"#
    );
}

#[test]
fn post_simple_entry_delta_bounds_every_ledger_invariant_to_the_entry() {
    let program = ledger_program();
    let set = compiled(&program);
    let asserted = vec![
        claim_instance("JournalEntry", &[subj("e42"), subj("d1"), subj("p1")]),
        claim_instance(
            "JournalLine",
            &[subj("e42"), subj("cash"), dec(100), dec(0)],
        ),
        claim_instance("JournalLine", &[subj("e42"), subj("rev"), dec(0), dec(100)]),
    ];
    let balanced = &set.invariants[1];
    assert_eq!(
        bounded(balanced.case_filter(&asserted, &[])),
        "((morpholog.claim_digest(morpholog.value_key_v1(t0.arguments -> 0))) = morpholog.claim_digest(morpholog.value_key_v1('{\"type\":\"subject\",\"value\":\"e42\"}'::jsonb)))"
    );
    // Supersedes is untouched by this delta entirely.
    assert!(matches!(
        set.invariants[0].case_filter(&asserted, &[]),
        CaseFilter::Untouched
    ));
}

/// The bound a case filter rendered, or a panic naming what it did instead.
fn bounded(filter: CaseFilter) -> String {
    match filter {
        CaseFilter::Bounded(sql) => sql,
        other => panic!("expected a bounded filter, got {other:?}"),
    }
}

#[test]
fn close_period_delta_touches_no_ledger_invariant() {
    let program = ledger_program();
    let set = compiled(&program);
    let asserted = vec![claim_instance("PeriodClosed", &[subj("p1")])];
    for inv in &set.invariants {
        assert!(
            matches!(inv.case_filter(&asserted, &[]), CaseFilter::Untouched),
            "{} should be skipped for close_period",
            inv.name
        );
    }
}

#[test]
fn retraction_also_touches_cases() {
    let program = ledger_program();
    let set = compiled(&program);
    let retracted = vec![claim_instance(
        "JournalLine",
        &[subj("e7"), subj("cash"), dec(5), dec(0)],
    )];
    assert_eq!(
        bounded(set.invariants[1].case_filter(&[], &retracted)),
        "((morpholog.claim_digest(morpholog.value_key_v1(t0.arguments -> 0))) = morpholog.claim_digest(morpholog.value_key_v1('{\"type\":\"subject\",\"value\":\"e7\"}'::jsonb)))"
    );
}

#[test]
fn out_of_fragment_constructs_refuse_by_name_and_variant() {
    let source = "\
program compiled_refusals

predicate A(x: Subject)
predicate B(x: Subject)

invariant uses_or:
    A(x) implies (B(x) or A(x))

invariant uses_pre:
    pre(A(x)) implies A(x)

invariant in_fragment:
    A(x) implies B(x)
";
    let program = morpholog_surface::parse_program(source).expect("parses");
    let refused = refusals(&program);
    let names: Vec<&str> = refused.iter().map(|r| r.invariant.as_str()).collect();
    assert_eq!(
        names,
        ["uses_or", "uses_pre"],
        "whole-run refusal names each offender, in-fragment invariant not blamed"
    );
    assert_eq!(
        refused[0].reason,
        CompileReason::Construct { construct: "or" }
    );
    assert_eq!(
        refused[1].reason,
        CompileReason::Construct { construct: "pre" }
    );
}

/// One refusing programme per out-of-fragment family the current IR
/// can spell, each pinned to its typed reason - so the fragment
/// boundary is enforced by variant, never by message text.
#[test]
fn every_out_of_fragment_family_refuses_with_its_typed_reason() {
    let cases: Vec<(&str, &str, CompileReason)> = vec![
        (
            "xor",
            "invariant r:\n    A(x) implies (B(x) xor A(x))\n",
            CompileReason::Construct { construct: "xor" },
        ),
        (
            "in",
            "invariant r:\n    Cx(x, c) implies x in c\n",
            CompileReason::Construct { construct: "in" },
        ),
        (
            // The checker admits `Any` under every comparator; the SQL
            // cannot tell how to order it at compile time.
            "ordering over an untyped position",
            "invariant r:\n    Anything(x, v) and Anything(_, w) implies v <= w\n",
            CompileReason::ArgumentKind {
                kind: PredicateArgKind::Any,
            },
        ),
        (
            "timestamp ordering under a nested scope",
            "invariant r:\n    A(x) implies (exists s: exists e: Timed(x, s, e) and s strictly_before e)\n",
            CompileReason::ComparisonShape {
                detail: "a quantity or timestamp ordering under a nested scope",
            },
        ),
        (
            "defined",
            "define d(x):\n    B(x)\n\ninvariant r:\n    A(x) implies d(x)\n",
            CompileReason::Construct {
                construct: "defined call",
            },
        ),
        (
            "temporal comparison",
            "invariant r:\n    Dated(x, d1, d2) implies d1 on_or_before d2\n",
            CompileReason::ComparisonDomain {
                domain: OrderedDomain::Date,
            },
        ),
        (
            "arithmetic",
            "invariant r:\n    Amount(x, n) implies 0 <= n + 1\n",
            CompileReason::Construct {
                construct: "arithmetic",
            },
        ),
        (
            "extremum",
            "invariant r:\n    Amount(x, n) implies max(m | Amount(_, m)) <= 100\n",
            CompileReason::Construct {
                construct: "extremum",
            },
        ),
        (
            "value lookup",
            "invariant r:\n    Amount(x, n) implies n <= value Cap(_)\n",
            CompileReason::Construct {
                construct: "value lookup",
            },
        ),
        (
            "conditional value",
            "invariant r:\n    Amount(x, n) implies n <= if(A(x), 1, 2)\n",
            CompileReason::Construct {
                construct: "conditional value",
            },
        ),
        (
            "builtin call",
            "invariant r:\n    Amount(x, n) implies abs(n) <= 100\n",
            CompileReason::Construct {
                construct: "builtin call",
            },
        ),
        (
            "duration comparison",
            "invariant r:\n    Lasting(x, d) implies d shorter_than duration(PT1H)\n",
            CompileReason::ComparisonDomain {
                domain: OrderedDomain::Duration,
            },
        ),
        (
            "quantity ordering not last",
            "invariant r:\n    Qty(x, q) and q > 0 t and A(x) implies B(x)\n",
            CompileReason::ComparisonShape {
                detail: "a quantity or timestamp ordering must be the last conjunct of its scope",
            },
        ),
        (
            "quantity ordering under a nested scope",
            "invariant r:\n    A(x) implies (exists q: Qty(x, q) and q > 0 t)\n",
            CompileReason::ComparisonShape {
                detail: "a quantity or timestamp ordering under a nested scope",
            },
        ),
        (
            "computed sum target",
            "invariant r:\n    Cap(cap) implies sum(n * 2 | Amount(_, n)) <= cap\n",
            CompileReason::SumShape {
                detail: "computed target",
            },
        ),
    ];
    let decls = "\
program family_refusals

predicate A(x: Subject)
predicate B(x: Subject)
predicate Amount(x: Subject, n: Decimal)
predicate Cap(cap: Decimal)
predicate Dated(x: Subject, opened: Date, closed: Date)
predicate Qty(x: Subject, q: Decimal[t])
predicate Timed(x: Subject, s: Timestamp, e: Timestamp)
predicate Lasting(x: Subject, d: Duration)
predicate Cx(x: Subject, c: Collection)
predicate Cx2(x: Subject, c: Collection)
predicate Anything(x: Subject, v: Any)

";
    for (family, invariant_src, expected) in cases {
        let source = format!("{decls}{invariant_src}");
        let program = morpholog_surface::parse_program(&source)
            .unwrap_or_else(|e| panic!("{family}: parse failed: {e:?}"));
        let refused = refusals(&program);
        assert_eq!(
            refused.len(),
            1,
            "{family}: expected exactly the one refusal, got {refused:?}"
        );
        assert_eq!(refused[0].reason, expected, "family: {family}");
    }
}

/// The two orderings a power-trading ledger forced: a quantity against a
/// literal of its unit, and one timestamp before another. Each can raise
/// in the kernel, so each closes its scope and carries the raise as data
/// the error query reports.
fn orderings_program() -> morpholog_core::Program {
    morpholog_surface::parse_program(
        "\
program orderings

predicate Terms(trade: Subject, quantity: Decimal[MW], delivery_start: Timestamp, delivery_end: Timestamp)

invariant quantity_is_positive:
    Terms(quantity: qty, ..) implies qty > 0 MW

invariant delivery_period_is_ordered:
    Terms(delivery_start: s, delivery_end: e, ..) implies s strictly_before e
",
    )
    .expect("parses")
}

#[test]
fn a_quantity_ordering_against_a_literal_is_pinned() {
    let set = compiled(&orderings_program());
    let inv = &set.invariants[0];
    assert_eq!(inv.name.as_str(), "quantity_is_positive");
    assert_eq!(
        inv.violation_sql(None),
        r#"/* morpholog compiled invariant quantity_is_positive v1 stage1 */
SELECT (t0.arguments -> 1)::text AS "w_qty"
FROM morpholog.claims t0
WHERE t0.predicate_name = 'Terms'
  AND (NOT (COALESCE((t0.arguments -> 1 ->> 'type') = 'quantity', false) AND (t0.arguments -> 1 -> 'value' ->> 'unit') = ('MW')) OR NOT ((CASE WHEN (t0.arguments -> 1 ->> 'type') = 'quantity' THEN (t0.arguments -> 1 -> 'value' ->> 'amount')::numeric END)) > ('0'::numeric))
ORDER BY (morpholog.value_key_v1(t0.arguments -> 1))::text
LIMIT 1"#
    );
    // Reports the stored operand whole, so the runner hands the kernel
    // what it would have compared.
    assert_eq!(
        inv.error_sqls(None, &[]).unwrap(),
        [
            r#"SELECT CASE WHEN NOT (COALESCE((t0.arguments -> 1 ->> 'type') = 'quantity', false) AND (t0.arguments -> 1 -> 'value' ->> 'unit') = ('MW')) THEN 'compare' END AS "kind",
       CASE WHEN NOT (COALESCE((t0.arguments -> 1 ->> 'type') = 'quantity', false) AND (t0.arguments -> 1 -> 'value' ->> 'unit') = ('MW')) THEN 'decimal' END AS "domain",
       CASE WHEN NOT (COALESCE((t0.arguments -> 1 ->> 'type') = 'quantity', false) AND (t0.arguments -> 1 -> 'value' ->> 'unit') = ('MW')) THEN (t0.arguments -> 1)::text END AS "left",
       CASE WHEN NOT (COALESCE((t0.arguments -> 1 ->> 'type') = 'quantity', false) AND (t0.arguments -> 1 -> 'value' ->> 'unit') = ('MW')) THEN ('{"type":"quantity","value":{"amount":"0","unit":"MW"}}'::jsonb)::text END AS "right",
       NULL::boolean AS "first"
FROM morpholog.claims t0
WHERE (t0.predicate_name = 'Terms' AND NOT (COALESCE((t0.arguments -> 1 ->> 'type') = 'quantity', false) AND (t0.arguments -> 1 -> 'value' ->> 'unit') = ('MW')))
ORDER BY t0.arguments_hash
LIMIT 1"#
        ]
    );
    // No ordering seeks an index; the antecedent binds without filtering.
    // The case does: a delta bounds the check to its quantity.
    assert!(inv.required_indexes.is_empty());
    let case: Vec<(String, usize)> = inv
        .case_indexes
        .iter()
        .map(|s| (s.predicate.to_string(), s.position))
        .collect();
    assert_eq!(case, vec![("Terms".to_string(), 1)]);
}

#[test]
fn a_timestamp_ordering_is_pinned() {
    let set = compiled(&orderings_program());
    let inv = &set.invariants[1];
    assert_eq!(inv.name.as_str(), "delivery_period_is_ordered");
    assert_eq!(
        inv.violation_sql(None),
        r#"/* morpholog compiled invariant delivery_period_is_ordered v1 stage1 */
SELECT (t0.arguments -> 3)::text AS "w_e",
       (t0.arguments -> 2)::text AS "w_s"
FROM morpholog.claims t0
WHERE t0.predicate_name = 'Terms'
  AND (NOT (COALESCE((t0.arguments -> 2 ->> 'type') = 'timestamp', false) AND COALESCE((t0.arguments -> 3 ->> 'type') = 'timestamp', false)) OR NOT (morpholog.timestamp_nanos(t0.arguments -> 2)) < (morpholog.timestamp_nanos(t0.arguments -> 3)))
ORDER BY (morpholog.value_key_v1(t0.arguments -> 3))::text, (morpholog.value_key_v1(t0.arguments -> 2))::text
LIMIT 1"#
    );
}

/// The error query orders rows as the kernel's candidate state holds
/// them: everything loaded first, in the load order, then each step's
/// admissions in statement order.
#[test]
fn the_error_query_follows_the_kernels_order() {
    let set = compiled(&orderings_program());
    let inv = &set.invariants[0];
    let first = uuid::Uuid::parse_str("0192b3a4-0000-7000-8000-000000000001").unwrap();
    let second = uuid::Uuid::parse_str("0192b3a4-0000-7000-8000-000000000002").unwrap();
    let steps = [
        DeltaStep {
            transition_id: first,
            asserted: vec![claim_instance(
                "Terms",
                &[
                    subj("t1"),
                    morpholog_test_support::qty("5", "MW"),
                    morpholog_test_support::ts("2026-01-01T00:00:00Z"),
                    morpholog_test_support::ts("2026-01-02T00:00:00Z"),
                ],
            )],
        },
        DeltaStep {
            transition_id: second,
            asserted: Vec::new(),
        },
    ];
    let sql = inv.error_sqls(None, &steps).unwrap().pop().unwrap();
    assert!(sql.ends_with(
        r#"
ORDER BY CASE t0.asserted_in WHEN '0192b3a4-0000-7000-8000-000000000001' THEN 1 WHEN '0192b3a4-0000-7000-8000-000000000002' THEN 2 ELSE 0 END, CASE t0.asserted_in WHEN '0192b3a4-0000-7000-8000-000000000001' THEN COALESCE(array_position(ARRAY['[{"type":"subject","value":"t1"},{"type":"quantity","value":{"amount":"5","unit":"MW"}},{"type":"timestamp","value":"2026-01-01T00:00:00Z"},{"type":"timestamp","value":"2026-01-02T00:00:00Z"}]'::jsonb], t0.arguments), 0) ELSE 0 END, t0.arguments_hash
LIMIT 1"#
    ), "{sql}");
}

#[test]
fn a_date_position_joins_as_the_tagged_value() {
    // Date/Timestamp/Bool/Duration positions are lawful join keys:
    // equality runs over the whole tagged value, whose canonical
    // serialisation makes structural equality semantic equality.
    let source = "\
program tagged_join

predicate Opened(x: Subject, on: Date)
predicate Closed(x: Subject, on: Date)

invariant closed_on_the_open_date:
    Closed(x, d) implies Opened(x, d)
";
    let program = morpholog_surface::parse_program(source).expect("parses");
    let set = compiled(&program);
    let sql = set.invariants[0].violation_sql(None);
    assert!(
        sql.contains("(morpholog.claim_digest(morpholog.value_key_v1(t0.arguments -> 1))) = (morpholog.claim_digest(morpholog.value_key_v1(t1.arguments -> 1)))"),
        "date join compares the keys' digests, got:\n{sql}"
    );
}

#[test]
fn the_provenance_comment_neutralises_a_hostile_invariant_name() {
    let program = b::program("hostile")
        .predicates(vec![b::predicate("A").subject("x").build()])
        .invariants(vec![b::invariant(
            "evil */ DROP TABLE morpholog.claims; /*\nline",
            b::implies(
                b::claim("A", vec![b::var("x")]),
                b::claim("A", vec![b::var("x")]),
            ),
        )])
        .build();
    let set = compile_invariants(program.validated().expect("validates"))
        .expect("in-fragment body compiles");
    let sql = set.invariants[0].violation_sql(None);
    // The whole comment head: exactly one opener, exactly one closer.
    // PostgreSQL block comments NEST, so an embedded `/*` would swallow
    // the statement.
    let head = sql.lines().next().expect("the comment head line");
    assert_eq!(head.matches("/*").count(), 1, "one opener, got:\n{sql}");
    assert_eq!(head.matches("*/").count(), 1, "one closer, got:\n{sql}");
    assert!(
        head.ends_with("*/"),
        "the closer ends the head line, got:\n{sql}"
    );
    assert!(
        head.contains("* /") && head.contains("/ *"),
        "both delimiters in the name are neutralised, got:\n{sql}"
    );
}

#[test]
fn a_date_keyed_delta_bounds_the_case_instead_of_widening() {
    let source = "\
program tagged_case

predicate Opened(x: Subject, on: Date)
predicate Closed(x: Subject, on: Date)

invariant closed_on_the_open_date:
    Closed(x, d) implies Opened(x, d)
";
    let program = morpholog_surface::parse_program(source).expect("parses");
    let set = compiled(&program);
    let asserted = vec![morpholog_core::ClaimInstance {
        predicate: "Closed".into(),
        args: vec![
            morpholog_test_support::subj("s1"),
            morpholog_test_support::date("2026-01-31"),
        ],
    }];
    let CaseFilter::Bounded(filter) = set.invariants[0].case_filter(&asserted, &[]) else {
        panic!("a date-keyed delta must bound, not widen");
    };
    assert!(
        filter.contains(r#"(morpholog.claim_digest(morpholog.value_key_v1(t0.arguments -> 1))) = morpholog.claim_digest(morpholog.value_key_v1('{"type":"date","value":"2026-01-31"}'::jsonb))"#),
        "the digest of the constant's key, got: {filter}"
    );
}

#[test]
fn comment_safe_leaves_no_delimiter_standing() {
    let hostile = "a*/b/*c\r\nd*/*e/*/f";
    let safe = comment_safe(hostile);
    assert!(!safe.contains("*/"), "closer survived: {safe}");
    assert!(!safe.contains("/*"), "opener survived: {safe}");
    assert!(!safe.contains('\n') && !safe.contains('\r'));
}

/// The compiler emits one index per (predicate, position) the ledger's
/// SQL filters or joins on, in the representation the SQL reads it with.
/// A witness-only position (the fork invariant's successor ids) is not a
/// seek and is not indexed.
#[test]
fn ledger_required_indexes_are_pinned() {
    let specs = compiled(&ledger_program()).required_indexes();
    let shape: Vec<(String, usize)> = specs
        .iter()
        .map(|s| (s.predicate.to_string(), s.position))
        .collect();
    assert_eq!(
        shape,
        [
            ("JournalEntry".to_string(), 0),
            ("JournalLine".to_string(), 0),
            ("Supersedes".to_string(), 0),
            ("Supersedes".to_string(), 1),
        ]
    );
    let first = &specs[0];
    assert_eq!(
        first.expression_sql,
        "morpholog.claim_digest(morpholog.value_key_v1(arguments -> 0))"
    );
    assert_eq!(
        first.partial_predicate_sql,
        "predicate_name = 'JournalEntry'"
    );
    assert_eq!(
        first.index_name(),
        format!("morpholog_ci_journalentry_0_vk1_{}", &first.digest()[..12])
    );
    assert_eq!(
        first.create_sql(),
        format!(
            "CREATE INDEX CONCURRENTLY \"{}\" ON morpholog.claims USING btree ((morpholog.claim_digest(morpholog.value_key_v1(arguments -> 0)))) WHERE predicate_name = 'JournalEntry'",
            first.index_name()
        )
    );
    // The digest covers the whole specification, so the same expression
    // over another predicate is another requirement.
    assert_ne!(specs[0].digest(), specs[1].digest());
}

/// The representability test the compiled sums carry, on the decimal
/// domain's edges: the kernel's rule is a normalised scale of at most
/// 28 and a normalised coefficient under 2^96, so the largest decimal
/// passes and one more refuses, at scale 0 and at scale 28 alike; a
/// coefficient of 1 at scale 29 refuses; trailing zeros never count.
#[tokio::test]
async fn the_range_test_matches_the_decimal_domain_at_its_edges() {
    let pool = crate::compiled_differential::test_pool().await;
    let vectors: &[(&str, bool)] = &[
        ("0", false),
        ("79228162514264337593543950335", false),
        ("-79228162514264337593543950335", false),
        ("79228162514264337593543950336", true),
        ("-79228162514264337593543950336", true),
        ("7.9228162514264337593543950335", false),
        ("7.9228162514264337593543950336", true),
        ("0.0000000000000000000000000001", false),
        ("0.00000000000000000000000000001", true),
        ("1.500", false),
        ("79228162514264337593543950335.0", false),
        ("79228162514264337593543950335.5", true),
    ];
    for (value, out_of_range) in vectors {
        let sql = format!(
            "SELECT {} FROM (VALUES ({value}::numeric)) AS t(v)",
            super::range_error_sql("v")
        );
        let got: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .fetch_one(&pool)
            .await
            .expect("evaluates");
        assert_eq!(got, *out_of_range, "{value}");
    }
}

/// A check bounded to a case seeks on the case's columns, so they are
/// required indexes even where no join or literal touches them.
#[test]
fn a_case_column_is_a_required_index() {
    let program = b::program("bounded")
        .predicates(vec![
            b::predicate("Bounded")
                .subject("item")
                .decimal("amount")
                .build(),
        ])
        .invariants(vec![b::invariant(
            "non_negative",
            b::implies(
                b::claim("Bounded", vec![b::var("item"), b::var("amount")]),
                b::le(b::term(b::dec("0")), b::term(b::var("amount"))),
            ),
        )])
        .build();
    let set = compiled(&program);
    let required: Vec<(String, usize)> = set
        .required_indexes()
        .iter()
        .map(|s| (s.predicate.to_string(), s.position))
        .collect();
    assert_eq!(
        required,
        vec![("Bounded".to_string(), 0), ("Bounded".to_string(), 1)]
    );
}
