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
SELECT (t0.arguments -> 0)::text AS "w_entry",
       (NOT (min_scale(l2.s) <= 28 AND abs(l2.s) * power(10::numeric, min_scale(l2.s)) < 79228162514264337593543950336::numeric) OR NOT (min_scale(l4.s) <= 28 AND abs(l4.s) * power(10::numeric, min_scale(l4.s)) < 79228162514264337593543950336::numeric)) AS "range_error"
FROM morpholog.claims t0, LATERAL (SELECT COALESCE(sum((t1.arguments -> 2 ->> 'value')::numeric), 0::numeric) AS s FROM morpholog.claims t1 WHERE t1.predicate_name = 'JournalLine' AND (t0.arguments -> 0 ->> 'value') = (t1.arguments -> 0 ->> 'value')) l2, LATERAL (SELECT COALESCE(sum((t3.arguments -> 3 ->> 'value')::numeric), 0::numeric) AS s FROM morpholog.claims t3 WHERE t3.predicate_name = 'JournalLine' AND (t0.arguments -> 0 ->> 'value') = (t3.arguments -> 0 ->> 'value')) l4
WHERE t0.predicate_name = 'JournalEntry'
  AND ((NOT (min_scale(l2.s) <= 28 AND abs(l2.s) * power(10::numeric, min_scale(l2.s)) < 79228162514264337593543950336::numeric) OR NOT (min_scale(l4.s) <= 28 AND abs(l4.s) * power(10::numeric, min_scale(l4.s)) < 79228162514264337593543950336::numeric)) OR NOT (l2.s) = (l4.s))
ORDER BY (t0.arguments -> 0 ->> 'value')::text
LIMIT 1"#
    );
    // Asked only once the query above returned a violation: any entry
    // in scope whose total no decimal can hold.
    assert_eq!(
        set.invariants[1].range_sql(),
        Some(
            r#"SELECT 1
FROM morpholog.claims t0, LATERAL (SELECT COALESCE(sum((t1.arguments -> 2 ->> 'value')::numeric), 0::numeric) AS s FROM morpholog.claims t1 WHERE t1.predicate_name = 'JournalLine' AND (t0.arguments -> 0 ->> 'value') = (t1.arguments -> 0 ->> 'value')) l2, LATERAL (SELECT COALESCE(sum((t3.arguments -> 3 ->> 'value')::numeric), 0::numeric) AS s FROM morpholog.claims t3 WHERE t3.predicate_name = 'JournalLine' AND (t0.arguments -> 0 ->> 'value') = (t3.arguments -> 0 ->> 'value')) l4
WHERE (t0.predicate_name = 'JournalEntry' AND ((NOT (min_scale(l2.s) <= 28 AND abs(l2.s) * power(10::numeric, min_scale(l2.s)) < 79228162514264337593543950336::numeric) OR NOT (min_scale(l4.s) <= 28 AND abs(l4.s) * power(10::numeric, min_scale(l4.s)) < 79228162514264337593543950336::numeric))))
LIMIT 1"#
        )
    );
    assert_eq!(set.invariants[0].range_sql(), None);
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
       (t0.arguments -> 1)::text AS "w_prior_entry_id",
       false AS "range_error"
FROM morpholog.claims t0, morpholog.claims t1
WHERE t0.predicate_name = 'Supersedes'
  AND t1.predicate_name = 'Supersedes'
  AND (t0.arguments -> 1 ->> 'value') = (t1.arguments -> 1 ->> 'value')
  AND NOT ((t0.arguments -> 0 ->> 'value') = (t1.arguments -> 0 ->> 'value'))
ORDER BY (t0.arguments -> 0 ->> 'value')::text, (t1.arguments -> 0 ->> 'value')::text, (t0.arguments -> 1 ->> 'value')::text
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
SELECT (t0.arguments -> 0)::text AS "w_entry",
       false AS "range_error"
FROM morpholog.claims t0
WHERE t0.predicate_name = 'JournalEntry'
  AND NOT EXISTS (SELECT 1 FROM morpholog.claims t1 WHERE t1.predicate_name = 'JournalLine' AND (t0.arguments -> 0 ->> 'value') = (t1.arguments -> 0 ->> 'value'))
ORDER BY (t0.arguments -> 0 ->> 'value')::text
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
        balanced.case_filter(&asserted, &[]),
        CaseFilter::Bounded("((t0.arguments -> 0 ->> 'value') = 'e42')".to_string())
    );
    // Supersedes is untouched by this delta entirely.
    assert_eq!(
        set.invariants[0].case_filter(&asserted, &[]),
        CaseFilter::Untouched
    );
}

#[test]
fn close_period_delta_touches_no_ledger_invariant() {
    let program = ledger_program();
    let set = compiled(&program);
    let asserted = vec![claim_instance("PeriodClosed", &[subj("p1")])];
    for inv in &set.invariants {
        assert_eq!(
            inv.case_filter(&asserted, &[]),
            CaseFilter::Untouched,
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
        set.invariants[1].case_filter(&[], &retracted),
        CaseFilter::Bounded("((t0.arguments -> 0 ->> 'value') = 'e7')".to_string())
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
            // Membership needs a bound collection, and binding a
            // Collection-kinded position refuses first - the kind
            // tier owns this family; the `Prop::In` arm is defence.
            "in",
            "invariant r:\n    Cx(x, c) implies x in c\n",
            CompileReason::ArgumentKind {
                kind: PredicateArgKind::Collection,
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
            "quantity kind",
            "invariant r:\n    Qty(x, q) implies Qty2(x, q)\n",
            CompileReason::ArgumentKind {
                kind: PredicateArgKind::Quantity(morpholog_core::Unit::from("t".to_string())),
            },
        ),
        (
            "collection kind",
            "invariant r:\n    Cx(x, c) implies Cx2(x, c)\n",
            CompileReason::ArgumentKind {
                kind: PredicateArgKind::Collection,
            },
        ),
        (
            "date literal",
            "invariant r:\n    Dated(x, @2026-01-01, _) implies A(x)\n",
            CompileReason::Literal { kind: "date" },
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
predicate Qty2(x: Subject, q: Decimal[t])
predicate Cx(x: Subject, c: Collection)
predicate Cx2(x: Subject, c: Collection)

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
        sql.contains("(t0.arguments -> 1) = (t1.arguments -> 1)"),
        "date join compares tagged jsonb, got:\n{sql}"
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
    // The whole comment head: exactly one opener, exactly one
    // closer, nothing nested. PostgreSQL block comments NEST, so
    // an embedded `/*` left alone would swallow the statement -
    // the first version of this test only inspected the text
    // before the first closer and missed exactly that.
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
        filter.contains(r#"(t0.arguments -> 1) = '{"type":"date","value":"2026-01-31"}'::jsonb"#),
        "the tagged constant equality, got: {filter}"
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

/// The indexes the ledger's SQL can seek on are emitted by the compiler
/// beside the SQL, one per (predicate, position) it filters or joins
/// on, in the representation the SQL reads that position with. A
/// witness-only position (the fork invariant's successor ids) is not a
/// seek and is not indexed; the spike indexed every variable position.
#[test]
fn ledger_required_indexes_are_pinned() {
    let specs = compiled(&ledger_program()).required_indexes();
    let shape: Vec<(String, usize, &str)> = specs
        .iter()
        .map(|s| {
            (
                s.predicate.to_string(),
                s.position,
                s.representation.as_str(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        [
            ("JournalEntry".to_string(), 0, "text"),
            ("JournalLine".to_string(), 0, "text"),
            ("Supersedes".to_string(), 1, "text"),
        ]
    );
    let first = &specs[0];
    assert_eq!(first.expression_sql, "arguments -> 0 ->> 'value'");
    assert_eq!(
        first.partial_predicate_sql,
        "predicate_name = 'JournalEntry'"
    );
    assert_eq!(
        first.index_name(),
        format!("morpholog_ci_journalentry_0_text_{}", &first.digest()[..12])
    );
    assert_eq!(
        first.create_sql(),
        format!(
            "CREATE INDEX CONCURRENTLY \"{}\" ON morpholog.claims USING btree ((arguments -> 0 ->> 'value')) WHERE predicate_name = 'JournalEntry'",
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
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = sqlx::PgPool::connect(&crate::with_default_user(&url))
        .await
        .expect("connect");
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
