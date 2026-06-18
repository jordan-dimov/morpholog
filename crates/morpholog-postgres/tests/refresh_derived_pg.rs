//! The durable half of the derived read-cache contract, against real
//! PostgreSQL. `refresh_derived` recomputes derived claims with the
//! kernel and publishes a generation into `morpholog_read`. These pin
//! the properties only a live engine confirms:
//!   - the cache equals the kernel's on-demand `enumerate_derived`
//!     (exact, because both ARE the kernel - SQL never recomputes);
//!     including a Timestamp-difference Duration, which round-trips
//!     through the tagged-JSONB rows;
//!   - refresh is idempotent and reflects source changes;
//!   - each refresh publishes exactly one generation (the old one is
//!     dropped) and records its metadata;
//!   - an empty source is a lawful-empty projection;
//!   - `propose` never touches the read model.
//!
//! Skipped unless `DATABASE_URL` is set, like the other PG suites.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use morpholog_core::{ClaimInstance, DerivedClaim, EvalValue, PredicateName, Program};
use morpholog_postgres::{PgPool, list_derived, refresh_derived};
use morpholog_surface::parse_program;

use common::{dec, subj};

const SENTINEL_HASH: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

async fn test_pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must be set for morpholog-postgres integration tests \
         (e.g. postgres:///morpholog_dev)",
    );
    PgPool::connect(&url)
        .await
        .expect("failed to connect to PostgreSQL test database")
}

/// Clean governed state and the read model, so each test starts fresh.
async fn reset(pool: &PgPool) {
    sqlx::raw_sql(
        "TRUNCATE morpholog.outbox, morpholog.claims, morpholog.audit, morpholog.rejections CASCADE; \
         TRUNCATE morpholog_read.derived_claims, morpholog_read.derived_active, \
                  morpholog_read.derived_refreshes CASCADE;",
    )
    .execute(pool)
    .await
    .expect("reset");
}

const FIXTURE: &str = "program cache_fixture\n\
    predicate Entry(account: Subject, amount: Decimal)\n\
    predicate Span(item: Subject, started: Timestamp, ended: Timestamp)\n\
    transformation post(account, amount):\n    admit Entry(account, amount)\n\
    transformation record(item, started, ended):\n    admit Span(item, started, ended)\n\
    derived AccountTotal(account):\n    over Entry(account, _)\n    \
        value total = sum(a | Entry(account, a))\n\
    derived SpanLength(item, started, ended):\n    over Span(item, started, ended)\n    \
        value length = ended - started\n";

fn fixture() -> Program {
    parse_program(FIXTURE).expect("fixture parses")
}

fn ts(s: &str) -> EvalValue {
    EvalValue::Timestamp(s.parse().expect("timestamp"))
}

async fn seed_entry(pool: &PgPool, p: &Program, account: &str, amount: i64) {
    let post = p.transformation("post").unwrap();
    common::propose_pg_with_test_actor(pool, post, vec![subj(account), dec(amount)], &[], &[])
        .await
        .expect("post commits");
}

async fn seed_span(pool: &PgPool, p: &Program, item: &str, started: &str, ended: &str) {
    let record = p.transformation("record").unwrap();
    common::propose_pg_with_test_actor(
        pool,
        record,
        vec![subj(item), ts(started), ts(ended)],
        &[],
        &[],
    )
    .await
    .expect("record commits");
}

fn derived<'a>(p: &'a Program, name: &str) -> &'a DerivedClaim {
    p.derived_claims
        .iter()
        .find(|d| d.predicate.as_str() == name)
        .expect("derived exists")
}

/// The active generation's rows for one derived predicate, as
/// `ClaimInstance`s, decoded from the cache's tagged-JSONB arguments.
async fn cache_rows(pool: &PgPool, predicate: &str) -> Vec<ClaimInstance> {
    let rows: Vec<(serde_json::Value,)> = sqlx::query_as(
        "SELECT c.arguments
         FROM morpholog_read.derived_claims c
         JOIN morpholog_read.derived_active a ON c.refresh_id = a.refresh_id
         WHERE c.predicate_name = $1",
    )
    .bind(predicate)
    .fetch_all(pool)
    .await
    .expect("read cache");
    rows.into_iter()
        .map(|(args,)| ClaimInstance {
            predicate: PredicateName::from(predicate),
            args: serde_json::from_value(args).expect("decode args"),
        })
        .collect()
}

/// Canonical order-independent representation for set comparison.
fn repr(rows: &[ClaimInstance]) -> Vec<String> {
    let mut out: Vec<String> = rows
        .iter()
        .map(|c| {
            format!(
                "{}:{}",
                c.predicate,
                serde_json::to_string(&c.args).unwrap()
            )
        })
        .collect();
    out.sort();
    out
}

async fn assert_cache_matches_kernel(pool: &PgPool, p: &Program, name: &str) {
    let cache = cache_rows(pool, name).await;
    let kernel = list_derived(pool, derived(p, name), &p.definitions)
        .await
        .expect("list_derived");
    assert_eq!(
        repr(&cache),
        repr(&kernel),
        "cache for `{name}` must equal the kernel's on-demand enumeration"
    );
    assert!(!kernel.is_empty(), "fixture should produce `{name}` rows");
}

#[tokio::test]
async fn cache_matches_the_kernel_for_each_derived() {
    let pool = test_pool().await;
    reset(&pool).await;
    let p = fixture();
    seed_entry(&pool, &p, "a1", 10).await;
    seed_entry(&pool, &p, "a1", 5).await;
    seed_entry(&pool, &p, "a2", 7).await;
    seed_span(
        &pool,
        &p,
        "s1",
        "2026-01-01T00:00:00Z",
        "2026-01-01T06:00:00Z",
    )
    .await;

    refresh_derived(&pool, &p, SENTINEL_HASH).await.unwrap();

    // Decimal sum and the Timestamp-difference Duration both round-trip
    // exactly through the cache's tagged JSONB.
    assert_cache_matches_kernel(&pool, &p, "AccountTotal").await;
    assert_cache_matches_kernel(&pool, &p, "SpanLength").await;
}

#[tokio::test]
async fn refresh_is_idempotent() {
    let pool = test_pool().await;
    reset(&pool).await;
    let p = fixture();
    seed_entry(&pool, &p, "a1", 10).await;
    seed_entry(&pool, &p, "a2", 3).await;

    refresh_derived(&pool, &p, SENTINEL_HASH).await.unwrap();
    let first = repr(&cache_rows(&pool, "AccountTotal").await);
    refresh_derived(&pool, &p, SENTINEL_HASH).await.unwrap();
    let second = repr(&cache_rows(&pool, "AccountTotal").await);
    assert_eq!(first, second);
}

#[tokio::test]
async fn a_changed_source_is_reflected_after_refresh() {
    let pool = test_pool().await;
    reset(&pool).await;
    let p = fixture();
    seed_entry(&pool, &p, "a1", 10).await;
    refresh_derived(&pool, &p, SENTINEL_HASH).await.unwrap();
    let before = cache_rows(&pool, "AccountTotal").await;
    assert_eq!(before.len(), 1);

    seed_entry(&pool, &p, "a2", 4).await;
    refresh_derived(&pool, &p, SENTINEL_HASH).await.unwrap();
    assert_cache_matches_kernel(&pool, &p, "AccountTotal").await;
    assert_eq!(cache_rows(&pool, "AccountTotal").await.len(), 2);
}

#[tokio::test]
async fn no_source_claims_is_a_lawful_empty_projection() {
    let pool = test_pool().await;
    reset(&pool).await;
    let p = fixture();

    let summary = refresh_derived(&pool, &p, SENTINEL_HASH).await.unwrap();
    assert_eq!(summary.derived_claim_count, 0);
    assert_eq!(summary.derived_predicate_count, 2);
    assert!(cache_rows(&pool, "AccountTotal").await.is_empty());

    // The metadata row and active pointer are written even for an empty
    // projection.
    let (count, hash): (i64, String) = sqlx::query_as(
        "SELECT r.derived_claim_count, r.model_hash
         FROM morpholog_read.derived_refreshes r
         JOIN morpholog_read.derived_active a ON r.refresh_id = a.refresh_id",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 0);
    assert_eq!(hash, SENTINEL_HASH);
}

#[tokio::test]
async fn each_refresh_publishes_one_generation() {
    let pool = test_pool().await;
    reset(&pool).await;
    let p = fixture();
    seed_entry(&pool, &p, "a1", 1).await;

    let first = refresh_derived(&pool, &p, SENTINEL_HASH).await.unwrap();
    let second = refresh_derived(&pool, &p, SENTINEL_HASH).await.unwrap();
    assert_ne!(first.refresh_id, second.refresh_id);

    // The prior generation is dropped: exactly one remains, and it is the
    // active one.
    let gen_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM morpholog_read.derived_refreshes")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(gen_count, 1);
    let active: uuid::Uuid =
        sqlx::query_scalar("SELECT refresh_id FROM morpholog_read.derived_active")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(active, second.refresh_id);
}

#[tokio::test]
async fn propose_does_not_touch_the_read_model() {
    let pool = test_pool().await;
    reset(&pool).await;
    let p = fixture();
    seed_entry(&pool, &p, "a1", 10).await;
    refresh_derived(&pool, &p, SENTINEL_HASH).await.unwrap();
    let before = repr(&cache_rows(&pool, "AccountTotal").await);

    // A governed transition commits new claims but never updates the read
    // model - the cache is stale-by-design until the next refresh.
    seed_entry(&pool, &p, "a2", 99).await;
    let after = repr(&cache_rows(&pool, "AccountTotal").await);
    assert_eq!(before, after, "propose must not refresh the read model");
}
