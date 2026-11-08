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
//! relation-level SIRead lock on the claims table. The same pair is run
//! for a read keyed by a value a `bind` found, where the loader's key is
//! a subquery: the planner, not the SQL, decides what the read locks.

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
use crate::scope_differential::{chain_state, corpus_with_hostiles, hostile_program};

async fn reset_db(pool: &PgPool) {
    crate::testing::reset_database(pool)
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

/// As a multiset, so a row fetched twice is a disagreement.
fn sorted(claims: &[ClaimInstance]) -> Vec<String> {
    let mut rows: Vec<String> = claims.iter().map(|c| format!("{c:?}")).collect();
    rows.sort();
    rows
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
                    decided_at: None,
                };
                // A generated state can repeat a claim; the table holds one
                // row per claim, so the expectation must too.
                let mut full: Vec<ClaimInstance> =
                    sample_state(&program, 2, salt).claims().to_vec();
                full.sort_by_key(|c| format!("{c:?}"));
                full.dedup();
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
                    let whole = morpholog_core::State::from_claims(full.clone());
                    let expected: Vec<ClaimInstance> = full
                        .iter()
                        .filter(|c| scope.admits(c, &whole))
                        .cloned()
                        .collect();
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
                decided_at: None,
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

/// Over the chain fixture, where every link matches: the loader fetches
/// what the projection admits, and that is one stock row of three.
#[tokio::test]
async fn the_loader_follows_a_chain_of_binds_to_the_rows_the_links_name() {
    let program = hostile_program("bind_chain");
    let t = &program.transformations[0];
    let pool = test_pool().await;
    reset_db(&pool).await;
    let full = chain_state();
    seed(&pool, &full).await;
    let transition = Transition {
        transformation_name: t.name.clone(),
        args: vec![subj("a1")],
        actor: test_actor(),
        decided_at: None,
    };
    let scope = compute_load_scope(
        t,
        Some(&transition),
        &program.invariants,
        &program.definitions,
        Reads::Body,
    );
    let whole = morpholog_core::State::from_claims(full.clone());
    let expected: Vec<ClaimInstance> = full
        .iter()
        .filter(|c| scope.admits(c, &whole))
        .cloned()
        .collect();
    let mut tx = pool.begin().await.unwrap();
    let loaded = load_state(&mut tx, &scope).await.unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(sorted(&loaded.claims().to_vec()), sorted(&expected));
    assert_eq!(loaded.claims_for("Stock").count(), 1, "{loaded:?}");
    assert_eq!(loaded.claims_for("Good").count(), 1, "{loaded:?}");
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
        decided_at: None,
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

/// One act in the race: what it loads, then what it writes.
struct Contender {
    scope: LoadScope,
    asserted: Vec<ClaimInstance>,
    retracted: Vec<ClaimInstance>,
}

fn posting(id: &str, reads: Reads) -> Contender {
    Contender {
        scope: posting_scope(id, reads),
        asserted: vec![claim_instance("Entry", &[subj(id), dec(1)])],
        retracted: vec![],
    }
}

/// One writer: a SERIALIZABLE transaction that loads its scope, waits at
/// the barrier twice (after its read, and after its write), writes its
/// delta, and commits.
async fn writer(pool: PgPool, act: Contender, barrier: Arc<Barrier>) -> Result<(), PgError> {
    let mut tx = pool.begin().await.map_err(crate::error::classify)?;
    sqlx::raw_sql("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await
        .map_err(crate::error::classify)?;
    let _ = load_state(&mut tx, &act.scope).await?;
    barrier.wait().await;
    write_claim_delta(&mut tx, Uuid::now_v7(), &act.asserted, &act.retracted).await?;
    barrier.wait().await;
    tx.commit().await.map_err(crate::error::classify)
}

/// Both writers' outcomes on a populated, provisioned database, with the
/// relation locks observed while both were parked after their reads.
async fn race(
    pool: &PgPool,
    a: Contender,
    b: Contender,
) -> (Result<(), PgError>, Result<(), PgError>, i64) {
    let barrier = Arc::new(Barrier::new(3));
    let a = tokio::spawn(writer(pool.clone(), a, barrier.clone()));
    let b = tokio::spawn(writer(pool.clone(), b, barrier.clone()));
    // Both have read; look before either writes.
    barrier.wait().await;
    let relation_locks = relation_sireads(pool).await;
    barrier.wait().await;
    (a.await.unwrap(), b.await.unwrap(), relation_locks)
}

async fn entries_pool() -> PgPool {
    let pool = test_pool().await;
    reset_db(&pool).await;
    populate_entries(&pool).await;
    let program = crate::program::PgProgram::new(
        morpholog_core::PreparedProgram::new(entries_program()).unwrap(),
    );
    crate::indexes::provision_indexes(&pool, &[&program], false)
        .await
        .unwrap();
    pool
}

#[tokio::test]
async fn two_postings_on_disjoint_entries_commit_together_under_keyed_reads() {
    let pool = entries_pool().await;
    let (a, b, relation_locks) = race(
        &pool,
        posting("e_new_a", Reads::Body),
        posting("e_new_b", Reads::Body),
    )
    .await;
    assert!(a.is_ok() && b.is_ok(), "keyed reads: {a:?} / {b:?}");
    assert_eq!(
        relation_locks, 0,
        "a keyed read must leave no relation-level SIRead lock"
    );
}

/// Every filter of the scope made whole: the control each pair runs.
fn whole(mut act: Contender) -> Contender {
    for filter in act.scope.filters.values_mut() {
        *filter = crate::propose::LoadFilter::Whole;
    }
    act
}

/// The control: the same two postings reading the predicate whole
/// conflict, and the lock table says why.
#[tokio::test]
async fn the_same_postings_conflict_under_whole_reads() {
    let pool = entries_pool().await;
    let (a, b, relation_locks) = race(
        &pool,
        whole(posting("e_new_a", Reads::Body)),
        whole(posting("e_new_b", Reads::Body)),
    )
    .await;
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

/// The shape #726 describes: a ship act whose stock read is keyed by
/// the item its allocation names, and which writes what it read.
pub(crate) fn shipments_program() -> morpholog_core::Program {
    morpholog_surface::parse_program(
        "program shipments
predicate Allocation(a: Subject, item: Subject)
predicate Stock(item: Subject, on_hand: Decimal)
transformation ship(a):
    bind Allocation(a, item)
    bind Stock(item, on_hand)
    require on_hand >= 1
    let left = on_hand - 1
    retract Stock(item, on_hand)
    admit Stock(item, left)
",
    )
    .unwrap()
}

/// `n` allocations, each of its own item, and each item's stock of 5:
/// past the page count at which a whole read is one relation lock.
pub(crate) async fn populate_shipments(pool: &PgPool, n: i64) {
    // Safe: the one interpolated value is this test's row count.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         SELECT 'Allocation', jsonb_build_array(jsonb_build_object('type','subject','value','a' || i), jsonb_build_object('type','subject','value','i' || i)), '00000000-0000-0000-0000-000000000000'
         FROM generate_series(1, {n}) AS i;
         INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         SELECT 'Stock', jsonb_build_array(jsonb_build_object('type','subject','value','i' || i), jsonb_build_object('type','decimal','value','5')), '00000000-0000-0000-0000-000000000000'
         FROM generate_series(1, {n}) AS i;
         ANALYZE morpholog.claims"
    )))
    .execute(pool)
    .await
    .unwrap();
}

/// `ship(a)` as a contender: its load, and the stock move it writes.
fn shipment(a: &str, item: &str) -> Contender {
    let program = shipments_program();
    let t = program.transformation("ship").unwrap();
    let transition = Transition {
        transformation_name: t.name.clone(),
        args: vec![subj(a)],
        actor: test_actor(),
        decided_at: None,
    };
    Contender {
        scope: compute_load_scope(
            t,
            Some(&transition),
            &program.invariants,
            &program.definitions,
            Reads::Body,
        ),
        asserted: vec![claim_instance("Stock", &[subj(item), dec(4)])],
        retracted: vec![claim_instance("Stock", &[subj(item), dec(5)])],
    }
}

async fn shipments_pool() -> PgPool {
    let pool = test_pool().await;
    reset_db(&pool).await;
    populate_shipments(&pool, 5000).await;
    let program = crate::program::PgProgram::new(
        morpholog_core::PreparedProgram::new(shipments_program()).unwrap(),
    );
    crate::indexes::provision_indexes(&pool, &[&program], false)
        .await
        .unwrap();
    pool
}

/// Two shipments of different allocations, each moving its own item's
/// stock: both commit, and the stock read keyed through the allocation
/// leaves no relation-level lock.
#[tokio::test]
async fn two_shipments_of_different_items_commit_together_under_bind_keyed_reads() {
    let pool = shipments_pool().await;
    let (a, b, relation_locks) = race(&pool, shipment("a1", "i1"), shipment("a2", "i2")).await;
    assert!(a.is_ok() && b.is_ok(), "bind-keyed reads: {a:?} / {b:?}");
    assert_eq!(
        relation_locks, 0,
        "a read keyed through a bind must leave no relation-level SIRead lock"
    );
}

/// The control: the same two shipments reading stock whole conflict.
#[tokio::test]
async fn the_same_shipments_conflict_under_whole_reads() {
    let pool = shipments_pool().await;
    let (a, b, relation_locks) = race(
        &pool,
        whole(shipment("a1", "i1")),
        whole(shipment("a2", "i2")),
    )
    .await;
    assert!(
        matches!(a, Err(PgError::SerializationFailure))
            || matches!(b, Err(PgError::SerializationFailure)),
        "whole reads must conflict: {a:?} / {b:?}"
    );
    assert!(relation_locks > 0);
}
