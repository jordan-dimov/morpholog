//! The plan-shape gate on the compiled checks: the indexes the compiler
//! says its SQL can seek on are the ones PostgreSQL actually seeks on.
//! Asserted structurally, not by pinning a plan tree, because the planner
//! may choose any reasonable plan:
//!
//! - **Eligibility.** With unordered scans disabled, a seek on each
//!   required index's extractor uses that index, for every
//!   whole-in-fragment gallery programme. One probe per index, because a
//!   scan uses one index and a lookup filtered on two positions may use
//!   either. This catches an extractor drifting from its index expression.
//! - **Planner regression.** On a populated, ANALYZEd ledger with the
//!   planner left alone, every index required for an invariant appears in
//!   that invariant's plan. The JIT cost and the ORDER BY choice are
//!   guarded where they bite (`jit = off` in the check transaction, ORDER
//!   BY over the extractor expressions) and would show here as a missing
//!   index.
//!
//! There is deliberately no test that every full violation query uses its
//! own indexes: a full uniqueness check is a self-join over the whole
//! predicate, which the planner rightly serves as a hash join of two full
//! scans. The indexes pay in correlated lookups and the case-bound check,
//! which the ledger regression covers.
//!
//! Compiled SQL has no bind parameters, so there is no generic plan to
//! force. `DATABASE_URL`-gated.

use std::collections::BTreeSet;

use morpholog_core::{CompiledProgram, Program};
use sqlx::Row as _;

use crate::PgPool;
use crate::compiled::{CompiledInvariantSet, compile_invariants};
use crate::compiled_differential::{test_pool, whole_in_fragment};
use crate::indexes::provision_indexes;
use crate::program::PgProgram;

async fn reset(pool: &PgPool) {
    sqlx::raw_sql(crate::testing::RESET_SQL)
        .execute(pool)
        .await
        .expect("truncate");
    let ours: Vec<String> = sqlx::query_scalar(
        "SELECT c.relname FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
         WHERE i.indrelid = 'morpholog.claims'::regclass AND c.relname LIKE 'morpholog_ci_%'",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    for name in ours {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP INDEX IF EXISTS morpholog.\"{name}\""
        )))
        .execute(pool)
        .await
        .unwrap();
    }
}

/// The index names used anywhere in a plan.
async fn indexes_in_plan(pool: &PgPool, sql: &str, planner_left_alone: bool) -> BTreeSet<String> {
    let mut tx = pool.begin().await.unwrap();
    if !planner_left_alone {
        sqlx::raw_sql("SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = off")
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    let plan: serde_json::Value =
        sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN (FORMAT JSON) {sql}")))
            .fetch_one(&mut *tx)
            .await
            .unwrap()
            .get(0);
    tx.rollback().await.unwrap();
    let mut out = BTreeSet::new();
    fn walk(node: &serde_json::Value, out: &mut BTreeSet<String>) {
        match node {
            serde_json::Value::Object(map) => {
                if let Some(name) = map.get("Index Name").and_then(|v| v.as_str()) {
                    out.insert(name.to_string());
                }
                for v in map.values() {
                    walk(v, out);
                }
            }
            serde_json::Value::Array(items) => items.iter().for_each(|v| walk(v, out)),
            _ => {}
        }
    }
    walk(&plan, &mut out);
    out
}

fn compiled_set(program: &Program) -> CompiledInvariantSet {
    compile_invariants(program.validated().expect("gallery programme validates"))
        .expect("whole-in-fragment programme compiles")
}

/// Every index the compiler required for an invariant is used in the
/// invariant's plan.
async fn assert_required_indexes_used(pool: &PgPool, program: &Program, planner_left_alone: bool) {
    let set = compiled_set(program);
    for inv in &set.invariants {
        let used = indexes_in_plan(pool, &inv.violation_sql(None), planner_left_alone).await;
        for spec in &inv.required_indexes {
            let name = spec.index_name();
            assert!(
                used.contains(&name),
                "{}::{}: the plan does not use {name} ({}[{}] {}); it uses {used:?}",
                program.name,
                inv.name,
                spec.predicate,
                spec.position,
                spec.representation.as_str()
            );
        }
    }
}

#[tokio::test]
async fn every_required_index_is_eligible_for_every_whole_in_fragment_programme() {
    let pool = test_pool().await;
    let programmes = whole_in_fragment();
    assert!(!programmes.is_empty());
    for program in programmes {
        reset(&pool).await;
        let pg = PgProgram::new(CompiledProgram::new(program.clone()).unwrap());
        populate_for_probes(&pool, &pg.required_indexes()).await;
        provision_indexes(&pool, &pg, false).await.unwrap();
        for spec in pg.required_indexes() {
            // A seek spelled as the compiled SQL spells it, with a literal
            // of the representation's type. A specification one position
            // off would build an index this seek cannot use.
            let literal = match spec.representation {
                crate::compiled::Representation::Text => "'probe'",
                crate::compiled::Representation::Numeric => "0",
                crate::compiled::Representation::Jsonb => "'{}'::jsonb",
                crate::compiled::Representation::QuantityAmount => "0",
            };
            let probe = format!(
                "SELECT 1 FROM morpholog.claims t0 WHERE t0.{} AND ({}) = {literal}",
                spec.partial_predicate_sql,
                spec.representation.extractor("t0.", spec.position)
            );
            let used = indexes_in_plan(&pool, &probe, false).await;
            let name = spec.index_name();
            assert!(
                used.contains(&name),
                "{}: a seek on {}[{}] {} does not use {name}; it uses {used:?}",
                program.name,
                spec.predicate,
                spec.position,
                spec.representation.as_str()
            );
        }
    }
}

/// Two hundred distinct rows per indexed predicate, each position holding
/// a value of the kind its representation reads (a subject elsewhere), so
/// a seek is genuinely cheaper rather than a tie on an empty table.
async fn populate_for_probes(pool: &PgPool, specs: &[crate::compiled::IndexSpec]) {
    use crate::compiled::Representation;
    let mut by_predicate: std::collections::BTreeMap<String, Vec<(usize, Representation)>> =
        std::collections::BTreeMap::new();
    for spec in specs {
        by_predicate
            .entry(spec.predicate.to_string())
            .or_default()
            .push((spec.position, spec.representation));
    }
    for (predicate, positions) in by_predicate {
        let arity = positions.iter().map(|(p, _)| p + 1).max().unwrap_or(1);
        let elements: Vec<String> = (0..arity)
            .map(
                |pos| match positions.iter().find(|(p, _)| *p == pos).map(|(_, r)| *r) {
                    Some(Representation::Numeric) => {
                        "jsonb_build_object('type','decimal','value',i::text)".to_string()
                    }
                    Some(Representation::Jsonb) => {
                        "jsonb_build_object('type','bool','value',(i % 2 = 0))".to_string()
                    }
                    Some(Representation::QuantityAmount) => {
                        "jsonb_build_object('type','quantity','value',jsonb_build_object('amount',i::text,'unit','MW'))".to_string()
                    }
                    _ => format!("jsonb_build_object('type','subject','value','p{pos}_' || i)"),
                },
            )
            .collect();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
             SELECT '{}', jsonb_build_array({}), $1 FROM generate_series(1, 200) AS i",
            predicate.replace('\'', "''"),
            elements.join(", ")
        )))
        .bind(uuid::Uuid::nil())
        .execute(pool)
        .await
        .unwrap();
    }
    sqlx::raw_sql("ANALYZE morpholog.claims")
        .execute(pool)
        .await
        .unwrap();
}

/// Ledger entries laid out as the bench lays them out, plus restatements.
async fn populate_ledger(pool: &PgPool, entries: i64) {
    let nil = uuid::Uuid::nil();
    sqlx::query(
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         SELECT 'JournalEntry',
                jsonb_build_array(
                    jsonb_build_object('type','subject','value','entry_' || i),
                    jsonb_build_object('type','subject','value','d_2026_05_17'),
                    jsonb_build_object('type','subject','value','p_gate')),
                $1
         FROM generate_series(1, $2) AS i",
    )
    .bind(nil)
    .bind(entries)
    .execute(pool)
    .await
    .unwrap();
    for (debit, credit, account) in [
        ("100", "0", "account_cash"),
        ("0", "100", "account_revenue"),
    ] {
        sqlx::query(
            "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
             SELECT 'JournalLine',
                    jsonb_build_array(
                        jsonb_build_object('type','subject','value','entry_' || i),
                        jsonb_build_object('type','subject','value',$3::text),
                        jsonb_build_object('type','decimal','value',$4::text),
                        jsonb_build_object('type','decimal','value',$5::text)),
                    $1
             FROM generate_series(1, $2) AS i",
        )
        .bind(nil)
        .bind(entries)
        .bind(account)
        .bind(debit)
        .bind(credit)
        .execute(pool)
        .await
        .unwrap();
    }
    // A tenth of the entries restated, so the fork invariant's
    // self-join has rows to seek and the planner has statistics for it.
    sqlx::query(
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         SELECT 'Supersedes',
                jsonb_build_array(
                    jsonb_build_object('type','subject','value','entry_' || (i + $2)),
                    jsonb_build_object('type','subject','value','entry_' || i)),
                $1
         FROM generate_series(1, $2 / 10) AS i",
    )
    .bind(nil)
    .bind(entries)
    .execute(pool)
    .await
    .unwrap();
    sqlx::raw_sql("ANALYZE morpholog.claims")
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn the_planner_chooses_the_provisioned_indexes_on_a_populated_ledger() {
    let pool = test_pool().await;
    reset(&pool).await;
    let program = morpholog_examples::double_entry_ledger::program();
    let pg = PgProgram::new(CompiledProgram::new(program.clone()).unwrap());
    provision_indexes(&pool, &pg, false).await.unwrap();
    populate_ledger(&pool, 5_000).await;
    assert_required_indexes_used(&pool, &program, true).await;
}
