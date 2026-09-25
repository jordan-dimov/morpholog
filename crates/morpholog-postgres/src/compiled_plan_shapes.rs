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
//!   planner left alone, every index an invariant's SQL seeks on at a
//!   join or a literal appears in that invariant's case-bound plan, and
//!   each predicate its case is keyed by is reached through one of the
//!   case's own indexes, since the filter constrains every one and the
//!   planner picks. The JIT cost and the ORDER BY choice are
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
use morpholog_test_support::{claim_instance, dec, subj};
use sqlx::Row as _;

use crate::PgPool;
use crate::compiled::{CaseFilter, CompiledInvariantSet, compile_invariants};
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

/// The indexes a plan seeks through: those named with an index
/// condition. A partial index also serves as a cheap scan of its
/// predicate, which would count as "used" without the condition.
async fn indexes_in_plan(pool: &PgPool, sql: &str, planner_left_alone: bool) -> BTreeSet<String> {
    indexes_in_plan_with(pool, sql, &[], planner_left_alone).await
}

/// As [`indexes_in_plan`], for a statement with bound values: the plan
/// is the one PostgreSQL makes for those values.
async fn indexes_in_plan_with(
    pool: &PgPool,
    sql: &str,
    binds: &[serde_json::Value],
    planner_left_alone: bool,
) -> BTreeSet<String> {
    let mut tx = pool.begin().await.unwrap();
    if !planner_left_alone {
        sqlx::raw_sql("SET LOCAL enable_seqscan = off; SET LOCAL enable_bitmapscan = off")
            .execute(&mut *tx)
            .await
            .unwrap();
    }
    let mut query = sqlx::query(sqlx::AssertSqlSafe(format!("EXPLAIN (FORMAT JSON) {sql}")));
    for value in binds {
        query = query.bind(value.clone());
    }
    let plan: serde_json::Value = query.fetch_one(&mut *tx).await.unwrap().get(0);
    tx.rollback().await.unwrap();
    let mut out = BTreeSet::new();
    fn walk(node: &serde_json::Value, out: &mut BTreeSet<String>) {
        match node {
            serde_json::Value::Object(map) => {
                if let Some(name) = map.get("Index Name").and_then(|v| v.as_str())
                    && map.contains_key("Index Cond")
                {
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
/// plan of the check production runs: the one bounded to the cases a
/// posting touches. The whole-state check may lawfully merge or hash
/// its joins instead, since it reads every row anyway.
async fn assert_required_indexes_used(pool: &PgPool, program: &Program, planner_left_alone: bool) {
    let set = compiled_set(program);
    let asserted = vec![
        claim_instance("JournalEntry", &[subj("e_probe"), subj("d1"), subj("p1")]),
        claim_instance(
            "JournalLine",
            &[subj("e_probe"), subj("cash"), dec(100), dec(0)],
        ),
        claim_instance("Supersedes", &[subj("e_probe"), subj("e_prior")]),
    ];
    for inv in &set.invariants {
        let filter = match inv.case_filter(&asserted, &[]) {
            CaseFilter::Bounded(filter) => Some(filter),
            CaseFilter::Unbounded => None,
            CaseFilter::Untouched => {
                panic!("{}: the probe delta touches every ledger rule", inv.name)
            }
        };
        let used = indexes_in_plan(
            pool,
            &inv.violation_sql(filter.as_deref()),
            planner_left_alone,
        )
        .await;
        for spec in &inv.required_indexes {
            let name = spec.index_name();
            assert!(
                used.contains(&name),
                "{}::{}: the case-bound plan does not use {name} ({}[{}] {}); it uses {used:?}",
                program.name,
                inv.name,
                spec.predicate,
                spec.position,
                crate::compiled::SEEK_REPRESENTATION
            );
        }
        // A case keyed by several columns of one predicate is served by
        // whichever of their indexes the optimiser judges enough, as a
        // keyed load is; the predicate must be reached through one of
        // them.
        let case_predicates: BTreeSet<_> = inv.case_indexes.iter().map(|s| &s.predicate).collect();
        for predicate in case_predicates {
            let reached = inv
                .case_indexes
                .iter()
                .filter(|s| &s.predicate == predicate)
                .any(|s| used.contains(&s.index_name()));
            assert!(
                reached,
                "{}::{}: the case-bound plan reaches {predicate} through none of its case indexes; it uses {used:?}",
                program.name, inv.name
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
            // A seek spelled as the compiled SQL spells it, against the
            // digest of a literal's key. A specification one position
            // off would build an index this seek cannot use.
            let probe = format!(
                "SELECT 1 FROM morpholog.claims t0 WHERE t0.{} AND ({}) = morpholog.claim_digest(morpholog.value_key_v1('{{\"type\":\"subject\",\"value\":\"probe\"}}'::jsonb))",
                spec.partial_predicate_sql,
                spec.seek_expression("t0.")
            );
            let used = indexes_in_plan(&pool, &probe, false).await;
            let name = spec.index_name();
            assert!(
                used.contains(&name),
                "{}: a seek on {}[{}] {} does not use {name}; it uses {used:?}",
                program.name,
                spec.predicate,
                spec.position,
                crate::compiled::SEEK_REPRESENTATION
            );
        }
    }
}

/// Two hundred distinct rows per indexed predicate, subjects at every
/// position, so a seek is genuinely cheaper rather than a tie on an empty
/// table.
async fn populate_for_probes(pool: &PgPool, specs: &[crate::compiled::IndexSpec]) {
    let mut by_predicate: std::collections::BTreeMap<String, usize> =
        std::collections::BTreeMap::new();
    for spec in specs {
        let arity = by_predicate.entry(spec.predicate.to_string()).or_default();
        *arity = (*arity).max(spec.position + 1);
    }
    for (predicate, arity) in by_predicate {
        let elements: Vec<String> = (0..arity)
            .map(|pos| format!("jsonb_build_object('type','subject','value','p{pos}_' || i)"))
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
pub(crate) async fn populate_ledger(pool: &PgPool, entries: i64) {
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

/// The loader seeks through the provisioned indexes: on the populated
/// ledger, a posting's keyed load uses at least one managed index per
/// keyed predicate, with an index condition, and spells each predicate as
/// a literal so the partial index's predicate is provable. The optimiser
/// may judge one position's index enough for a pattern with several.
#[tokio::test]
async fn the_loader_seeks_through_the_provisioned_indexes() {
    use crate::propose::{LoadFilter, Reads, compute_load_scope, load_sql};
    let pool = test_pool().await;
    reset(&pool).await;
    let program = morpholog_examples::double_entry_ledger::program();
    let pg = PgProgram::new(CompiledProgram::new(program.clone()).unwrap());
    provision_indexes(&pool, &pg, false).await.unwrap();
    populate_ledger(&pool, 5_000).await;
    let post = program.transformation("post_simple_entry").unwrap();
    let transition = morpholog_core::Transition {
        transformation_name: post.name.clone(),
        args: vec![
            subj("e_probe"),
            subj("d_2026_05_17"),
            subj("p_2026_05"),
            subj("account_cash"),
            subj("account_revenue"),
            dec(1),
        ],
        actor: morpholog_test_support::test_actor(),
    };
    for reads in [Reads::Body, Reads::BodyAndInvariants] {
        let scope = compute_load_scope(
            post,
            Some(&transition),
            &program.invariants,
            &program.definitions,
            reads,
        );
        let (sql, binds) = load_sql(&scope).unwrap();
        let used = indexes_in_plan_with(&pool, &sql, &binds, true).await;
        let required: Vec<crate::compiled::IndexSpec> = pg.required_indexes();
        for (predicate, filter) in &scope.filters {
            assert!(
                sql.contains(&format!("predicate_name = '{predicate}'")),
                "{reads:?}: {predicate} is spelled as a literal"
            );
            if let LoadFilter::Keyed(patterns) = filter {
                let candidates: Vec<String> = required
                    .iter()
                    .filter(|spec| {
                        &spec.predicate == predicate
                            && patterns
                                .iter()
                                .flatten()
                                .any(|(position, _)| *position == spec.position)
                    })
                    .map(crate::compiled::IndexSpec::index_name)
                    .collect();
                assert!(
                    !candidates.is_empty(),
                    "{reads:?}: {predicate} is keyed but no index is required for its positions"
                );
                assert!(
                    candidates.iter().any(|name| used.contains(name)),
                    "{reads:?}: the load of {predicate} seeks no managed index; candidates {candidates:?}, used {used:?}"
                );
            }
        }
    }
}

/// A rule whose case is keyed by a column no read or join touches: the
/// shape Glasshouse's `delivery_period_is_ordered` has. The case-bound
/// check must seek on that column and leave no relation-level SIRead
/// lock behind, or the bound protects nothing under SERIALIZABLE.
#[tokio::test]
async fn a_case_column_off_the_read_positions_seeks_and_leaves_no_relation_lock() {
    let pool = test_pool().await;
    reset(&pool).await;
    let program = morpholog_surface::parse_program(
        "program bounded
predicate Bounded(item: Subject, amount: Decimal)
invariant non_negative:
    Bounded(item, amount) implies 0 <= amount
transformation record(item, amount):
    admit Bounded(item, amount)
",
    )
    .expect("parses");
    let pg = PgProgram::new(CompiledProgram::new(program.clone()).unwrap());
    let required: BTreeSet<(String, usize)> = pg
        .required_indexes()
        .iter()
        .map(|s| (s.predicate.to_string(), s.position))
        .collect();
    assert!(
        required.contains(&("Bounded".to_string(), 1)),
        "the case column is a required index: {required:?}"
    );
    provision_indexes(&pool, &pg, false).await.unwrap();
    sqlx::raw_sql(
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         SELECT 'Bounded',
                jsonb_build_array(jsonb_build_object('type', 'subject', 'value', 'item_' || i),
                                  jsonb_build_object('type', 'decimal', 'value', i::text)),
                gen_random_uuid()
         FROM generate_series(1, 5000) i;
         ANALYZE morpholog.claims;",
    )
    .execute(&pool)
    .await
    .unwrap();
    let set = compiled_set(&program);
    let inv = &set.invariants[0];
    let CaseFilter::Bounded(filter) =
        inv.case_filter(&[claim_instance("Bounded", &[subj("item_7"), dec(7)])], &[])
    else {
        panic!("the delta bounds the rule to its case");
    };
    let sql = inv.violation_sql(Some(&filter));
    let used = indexes_in_plan(&pool, &sql, true).await;
    assert!(
        used.iter().any(|n| n.starts_with("morpholog_ci_bounded_")),
        "the case-bound plan seeks through a managed index; it uses {used:?}"
    );
    let mut tx = pool.begin().await.unwrap();
    sqlx::raw_sql("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_optional(&mut *tx)
        .await
        .unwrap();
    let relation_locks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks
          WHERE pid = pg_backend_pid() AND mode = 'SIReadLock' AND locktype = 'relation'
            AND relation = 'morpholog.claims'::regclass",
    )
    .fetch_one(&mut *tx)
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(
        relation_locks, 0,
        "the case-bound check took a relation lock"
    );
}
