//! Holds the loader to the read plan, and the read plan's point to the
//! database. `DATABASE_URL`-gated.
//!
//! The first check: for every transformation of the corpus, the rows the
//! loader fetches for a transition are exactly the rows the plan's own
//! test admits, over seeded states that include values of another kind
//! than the position declares. The loader's equality is the key's, so a
//! row keyed by a value the kernel would match is fetched whatever kind
//! it is stored as.
//!
//! The second: two proposals keyed on different cases of one predicate
//! past the page threshold read, are held at a barrier, write, and both
//! commit; with whole reads one of them is refused by SERIALIZABLE. The
//! lock table is inspected while both are parked: a keyed read leaves no
//! relation-level SIRead lock on the claims table.

use std::collections::BTreeSet;
use std::sync::Arc;

use morpholog_core::{ClaimInstance, EvalValue, Transition};
use morpholog_test_support::differential::{sample_args, sample_state};
use morpholog_test_support::{claim_instance, dec, subj, test_actor};
use sqlx::PgPool;
use tokio::sync::Barrier;
use uuid::Uuid;

use crate::compiled_differential::test_pool;
use crate::error::PgError;
use crate::propose::{LoadScope, Reads, compute_load_scope, load_state, write_claim_delta};
use crate::scope_differential::corpus_with_hostiles;

async fn reset_db(pool: &PgPool) {
    sqlx::query(crate::testing::RESET_SQL)
        .execute(pool)
        .await
        .expect("failed to truncate test DB");
}

async fn seed(pool: &PgPool, claims: &[ClaimInstance]) {
    for claim in claims {
        sqlx::query(
            "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in) VALUES ($1, $2, $3)
             ON CONFLICT (predicate_name, arguments_hash) DO NOTHING",
        )
        .bind(claim.predicate.as_str())
        .bind(serde_json::to_value(&claim.args).unwrap())
        .bind(Uuid::nil())
        .execute(pool)
        .await
        .expect("seed insert");
    }
}

fn sorted(claims: &[ClaimInstance]) -> BTreeSet<String> {
    claims.iter().map(|c| format!("{c:?}")).collect()
}

#[tokio::test]
async fn the_loader_fetches_exactly_the_rows_the_plan_admits() {
    let pool = test_pool().await;
    let mut cases = 0usize;
    for (name, program) in corpus_with_hostiles() {
        for t in &program.transformations {
            for salt in 0..2u64 {
                let Some(args) = sample_args(&program, t, salt) else {
                    continue;
                };
                let transition = Transition {
                    transformation_name: t.name.clone(),
                    args,
                    actor: test_actor(),
                };
                let full: Vec<ClaimInstance> = sample_state(&program, 2, salt).claims().to_vec();
                reset_db(&pool).await;
                seed(&pool, &full).await;
                for reads in [Reads::Body, Reads::BodyAndInvariants] {
                    let scope = compute_load_scope(
                        t,
                        Some(&transition),
                        &program.invariants,
                        &program.definitions,
                        reads,
                    );
                    let expected: Vec<ClaimInstance> =
                        full.iter().filter(|c| scope.admits(c)).cloned().collect();
                    let mut tx = pool.begin().await.unwrap();
                    let loaded = load_state(&mut tx, &scope).await.unwrap();
                    tx.rollback().await.unwrap();
                    assert_eq!(
                        sorted(&loaded.claims().to_vec()),
                        sorted(&expected),
                        "{name}::{} salt {salt} {reads:?}: the loader and the plan disagree (scope {scope:?})",
                        t.name
                    );
                    cases += 1;
                }
            }
        }
    }
    assert!(cases >= 100, "generator collapse: {cases} cases");
}

/// A row of another kind at a keyed position, keyed by the value the
/// kernel would match: fetched, because the key is the kernel's equality;
/// a row of the same kind with another value: not.
#[tokio::test]
async fn the_loader_keys_by_kernel_equality_over_old_shape_rows() {
    let program = morpholog_surface::parse_program(
        "program old_shape_load
predicate Level(k: Subject, n: Decimal)
predicate Out(k: Subject)
transformation probe(k, n):
    require Level(k, n)
    admit Out(k)
",
    )
    .unwrap();
    let t = &program.transformations[0];
    let pool = test_pool().await;
    reset_db(&pool).await;
    // A subject where a decimal is declared, and two spellings of one
    // decimal.
    let rows = [
        claim_instance("Level", &[subj("a"), subj("legacy")]),
        claim_instance(
            "Level",
            &[subj("a"), morpholog_test_support::dec_str("1.0")],
        ),
        claim_instance(
            "Level",
            &[subj("b"), morpholog_test_support::dec_str("1.00")],
        ),
    ];
    seed(&pool, &rows).await;
    let load = |args: Vec<EvalValue>| {
        compute_load_scope(
            t,
            Some(&Transition {
                transformation_name: t.name.clone(),
                args,
                actor: test_actor(),
            }),
            &program.invariants,
            &program.definitions,
            Reads::Body,
        )
    };
    let mut tx = pool.begin().await.unwrap();
    let legacy = load_state(&mut tx, &load(vec![subj("a"), subj("legacy")]))
        .await
        .unwrap();
    assert_eq!(legacy.claims().to_vec(), vec![rows[0].clone()]);
    let scaled = load_state(&mut tx, &load(vec![subj("b"), dec(1)]))
        .await
        .unwrap();
    assert_eq!(
        scaled.claims().to_vec(),
        vec![rows[2].clone()],
        "1 keys 1.00"
    );
    let none = load_state(&mut tx, &load(vec![subj("a"), dec(2)]))
        .await
        .unwrap();
    assert!(none.claims().is_empty());
    tx.rollback().await.unwrap();
}

/// Relation-level SIRead locks on the claims table, seen from outside.
async fn relation_sireads(pool: &PgPool) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_locks
          WHERE mode = 'SIReadLock' AND locktype = 'relation'
            AND relation = 'morpholog.claims'::regclass",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// The shape #396 describes: a gate that reads the predicate the body
/// then admits, keyed on the entry.
fn entries_program() -> morpholog_core::Program {
    morpholog_surface::parse_program(
        "program entries
predicate Entry(id: Subject, n: Decimal)
transformation post(id, n):
    require not Entry(id, _)
    admit Entry(id, n)
",
    )
    .unwrap()
}

fn posting_scope(id: &str, reads: Reads) -> LoadScope {
    let program = entries_program();
    let t = program.transformation("post").unwrap();
    let transition = Transition {
        transformation_name: t.name.clone(),
        args: vec![subj(id), dec(1)],
        actor: test_actor(),
    };
    compute_load_scope(
        t,
        Some(&transition),
        &program.invariants,
        &program.definitions,
        reads,
    )
}

/// Five thousand entries: well past the page count at which PostgreSQL
/// holds a whole read as one lock on the relation.
async fn populate_entries(pool: &PgPool) {
    sqlx::raw_sql(
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         SELECT 'Entry', jsonb_build_array(jsonb_build_object('type','subject','value','e' || i), jsonb_build_object('type','decimal','value','1')), '00000000-0000-0000-0000-000000000000'
         FROM generate_series(1, 5000) AS i;
         ANALYZE morpholog.claims",
    )
    .execute(pool)
    .await
    .unwrap();
}

/// One writer: a SERIALIZABLE transaction that loads `scope`, waits at
/// the barrier twice (after its read, and after its write), admits its
/// own entry, and commits.
async fn writer(
    pool: PgPool,
    scope: LoadScope,
    id: &'static str,
    barrier: Arc<Barrier>,
) -> Result<(), PgError> {
    let mut tx = pool.begin().await.map_err(crate::error::classify)?;
    sqlx::raw_sql("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await
        .map_err(crate::error::classify)?;
    let _ = load_state(&mut tx, &scope).await?;
    barrier.wait().await;
    let delta = vec![claim_instance("Entry", &[subj(id), dec(1)])];
    write_claim_delta(&mut tx, Uuid::now_v7(), &delta, &[]).await?;
    barrier.wait().await;
    tx.commit().await.map_err(crate::error::classify)
}

/// Both writers' outcomes, with the relation locks observed while both
/// were parked after their reads.
async fn race(
    scope_for: impl Fn(&str) -> LoadScope,
) -> (Result<(), PgError>, Result<(), PgError>, i64) {
    let pool = test_pool().await;
    reset_db(&pool).await;
    populate_entries(&pool).await;
    let program = crate::program::PgProgram::new(
        morpholog_core::CompiledProgram::new(entries_program()).unwrap(),
    );
    crate::indexes::provision_indexes(&pool, &program, false)
        .await
        .unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let a = tokio::spawn(writer(
        pool.clone(),
        scope_for("e_new_a"),
        "e_new_a",
        barrier.clone(),
    ));
    let b = tokio::spawn(writer(
        pool.clone(),
        scope_for("e_new_b"),
        "e_new_b",
        barrier.clone(),
    ));
    // Both have read; look before either writes.
    barrier.wait().await;
    let relation_locks = relation_sireads(&pool).await;
    barrier.wait().await;
    (a.await.unwrap(), b.await.unwrap(), relation_locks)
}

#[tokio::test]
async fn two_postings_on_disjoint_entries_commit_together_under_keyed_reads() {
    let (a, b, relation_locks) = race(|id| posting_scope(id, Reads::Body)).await;
    assert!(a.is_ok() && b.is_ok(), "keyed reads: {a:?} / {b:?}");
    assert_eq!(
        relation_locks, 0,
        "a keyed read must leave no relation-level SIRead lock"
    );
}

/// The control: the same two postings reading the predicate whole
/// conflict, and the lock table says why.
#[tokio::test]
async fn the_same_postings_conflict_under_whole_reads() {
    let whole = |id: &str| {
        let mut scope = posting_scope(id, Reads::Body);
        for filter in scope.filters.values_mut() {
            *filter = crate::propose::LoadFilter::Whole;
        }
        scope
    };
    let (a, b, relation_locks) = race(whole).await;
    assert!(
        matches!(a, Err(PgError::SerializationFailure))
            || matches!(b, Err(PgError::SerializationFailure)),
        "whole reads must conflict: {a:?} / {b:?}"
    );
    assert!(
        relation_locks > 0,
        "a whole read past the page threshold holds a relation-level SIRead lock"
    );
}
