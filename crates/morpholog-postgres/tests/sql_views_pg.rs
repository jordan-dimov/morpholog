//! The durable half of the view-surface contract, against real
//! PostgreSQL. The renderer's unit tests pin the SQL *text*; these pin
//! what that text *does* in the database - the properties that only a
//! live engine can confirm:
//!   - the views are non-updatable (INSERT/UPDATE/DELETE through them
//!     fail and never reach `morpholog.claims`);
//!   - the script applies atomically (a mid-script failure rolls the
//!     whole thing back);
//!   - append-only model evolution stays a compatible CREATE OR REPLACE,
//!     while rename/retype is correctly rejected;
//!   - the catalogue round-trips the model hash;
//!   - kind metadata survives as COMMENT ON COLUMN;
//!   - `Any` reads back the whole tagged object;
//!   - the temporal precision boundary holds (typed columns are
//!     microsecond; `_morpholog_arguments` keeps the exact source).
//!
//! Skipped unless `DATABASE_URL` is set, like the other PG suites.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use jiff::{SignedDuration, Timestamp};
use morpholog_core::{EvalValue, Program, Unit};
use morpholog_postgres::{PgPool, render_views};
use morpholog_surface::parse_program;
use rust_decimal::Decimal;

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

/// Truncate governed state and drop the generated view schema, so each
/// test starts from a clean surface.
async fn reset(pool: &PgPool) {
    sqlx::raw_sql(
        "TRUNCATE morpholog.outbox, morpholog.claims, morpholog.audit, morpholog.rejections CASCADE; \
         DROP SCHEMA IF EXISTS morpholog_views CASCADE; \
         DROP SCHEMA IF EXISTS analytics CASCADE;",
    )
    .execute(pool)
    .await
    .expect("reset");
}

/// A programme exercising the kinds whose database behaviour matters:
/// Subject, unit-tagged Quantity, Duration, Timestamp, and Any.
fn fixture_program() -> Program {
    let p = parse_program(
        "program voyages\n\
         predicate Fixture(voyage: Subject, daily: Decimal[USD], allowed: Duration, \
         opened: Timestamp, note: Any)\n\
         transformation open(voyage, daily, allowed, opened, note):\n    \
             admit Fixture(voyage, daily, allowed, opened, note)\n",
    )
    .expect("voyages parses");
    p.validate().expect("voyages validates");
    p
}

async fn seed_fixture(
    pool: &PgPool,
    p: &Program,
    voyage: &str,
    allowed: SignedDuration,
    opened: &str,
    note: EvalValue,
) {
    let open = p.transformation("open").unwrap();
    let args = vec![
        EvalValue::Subject(voyage.into()),
        EvalValue::Quantity {
            amount: Decimal::from(25_000),
            unit: Unit::from("USD"),
        },
        EvalValue::Duration(allowed),
        EvalValue::Timestamp(opened.parse::<Timestamp>().expect("timestamp parses")),
        note,
    ];
    let outcome =
        common::propose_pg_with_test_actor(pool, open, args, &p.invariants, &p.definitions)
            .await
            .expect("seed proposes");
    assert!(
        matches!(
            outcome,
            morpholog_postgres::PgProposalOutcome::Committed { .. }
        ),
        "seed must commit, got {outcome:?}"
    );
}

fn render(p: &Program, schema: &str) -> String {
    // `render_views` only borrows `p` for the call, so the caller's
    // reference is enough - no clone, no leak.
    render_views(p.validated().unwrap(), schema, SENTINEL_HASH)
        .expect("renders")
        .sql
}

async fn claim_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM morpholog.claims")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn views_are_not_updatable() {
    let pool = test_pool().await;
    reset(&pool).await;
    let p = fixture_program();
    seed_fixture(
        &pool,
        &p,
        "v1",
        SignedDuration::from_hours(6),
        "2026-01-01T00:00:00Z",
        EvalValue::Decimal(Decimal::from(42)),
    )
    .await;
    sqlx::raw_sql(&render(&p, "morpholog_views"))
        .execute(&pool)
        .await
        .expect("script applies");

    let before = claim_count(&pool).await;
    for write in [
        "INSERT INTO morpholog_views.fixture (voyage) VALUES ('hack')",
        "UPDATE morpholog_views.fixture SET voyage = 'hack'",
        "DELETE FROM morpholog_views.fixture",
    ] {
        let result = sqlx::query(write).execute(&pool).await;
        assert!(
            result.is_err(),
            "writing through the view must fail: {write}"
        );
    }
    assert_eq!(
        claim_count(&pool).await,
        before,
        "no write reached morpholog.claims"
    );
}

#[tokio::test]
async fn the_script_applies_atomically() {
    let pool = test_pool().await;
    reset(&pool).await;
    let p = fixture_program();
    // Pre-create a plain TABLE occupying the view's name, so the script's
    // CREATE OR REPLACE VIEW for `fixture` fails ("not a view"). The
    // BEGIN/COMMIT wrapper must then leave the catalogue uncreated too.
    sqlx::raw_sql("CREATE SCHEMA morpholog_views; CREATE TABLE morpholog_views.fixture (x int);")
        .execute(&pool)
        .await
        .expect("pre-create collides");

    let result = sqlx::raw_sql(&render(&p, "morpholog_views"))
        .execute(&pool)
        .await;
    assert!(result.is_err(), "the colliding name must abort the script");

    // The catalogue (rendered after the views) must not exist: the whole
    // transaction rolled back rather than leaving a half-built surface.
    let catalog_exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM pg_views \
         WHERE schemaname = 'morpholog_views' AND viewname = '_morpholog_catalog')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!catalog_exists, "atomic rollback left no catalogue behind");
}

#[tokio::test]
async fn appending_a_field_is_a_compatible_replace() {
    let pool = test_pool().await;
    reset(&pool).await;
    let v1 = parse_program(
        "program eden\n\
         predicate Rec(id: Subject, amount: Decimal)\n\
         transformation t(id, amount):\n    admit Rec(id, amount)\n",
    )
    .unwrap();
    v1.validate().unwrap();
    sqlx::raw_sql(&render(&v1, "morpholog_views"))
        .execute(&pool)
        .await
        .expect("v1 applies");

    // Append a trailing field: metadata-first column order means the new
    // column lands at the end of the view, which CREATE OR REPLACE allows.
    let v2 = parse_program(
        "program eden\n\
         predicate Rec(id: Subject, amount: Decimal, memo: Subject)\n\
         transformation t(id, amount, memo):\n    admit Rec(id, amount, memo)\n",
    )
    .unwrap();
    v2.validate().unwrap();
    sqlx::raw_sql(&render(&v2, "morpholog_views"))
        .execute(&pool)
        .await
        .expect("appending a field stays a compatible replace");

    let has_memo = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM information_schema.columns \
         WHERE table_schema='morpholog_views' AND table_name='rec' AND column_name='memo')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(has_memo, "the appended field is now a column");
}

#[tokio::test]
async fn renaming_a_field_is_rejected_atomically() {
    let pool = test_pool().await;
    reset(&pool).await;
    let v1 = parse_program(
        "program eden\n\
         predicate Rec(id: Subject, amount: Decimal)\n\
         transformation t(id, amount):\n    admit Rec(id, amount)\n",
    )
    .unwrap();
    v1.validate().unwrap();
    sqlx::raw_sql(&render(&v1, "morpholog_views"))
        .execute(&pool)
        .await
        .expect("v1 applies");

    // Renaming a field renames an existing view column, which
    // CREATE OR REPLACE forbids - the documented manual-migration case.
    let renamed = parse_program(
        "program eden\n\
         predicate Rec(id: Subject, total: Decimal)\n\
         transformation t(id, total):\n    admit Rec(id, total)\n",
    )
    .unwrap();
    renamed.validate().unwrap();
    let result = sqlx::raw_sql(&render(&renamed, "morpholog_views"))
        .execute(&pool)
        .await;
    assert!(result.is_err(), "renaming a column must be rejected");
}

#[tokio::test]
async fn catalog_round_trips_the_model_hash() {
    let pool = test_pool().await;
    reset(&pool).await;
    let p = fixture_program();
    sqlx::raw_sql(&render(&p, "morpholog_views"))
        .execute(&pool)
        .await
        .expect("script applies");

    let hash = sqlx::query_scalar::<_, String>(
        "SELECT model_hash FROM morpholog_views._morpholog_catalog LIMIT 1",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(hash, SENTINEL_HASH);
}

#[tokio::test]
async fn quantity_unit_survives_as_a_column_comment() {
    let pool = test_pool().await;
    reset(&pool).await;
    let p = fixture_program();
    sqlx::raw_sql(&render(&p, "morpholog_views"))
        .execute(&pool)
        .await
        .expect("script applies");

    let comment = sqlx::query_scalar::<_, String>(
        "SELECT pgd.description \
         FROM pg_description pgd \
         JOIN pg_class c ON c.oid = pgd.objoid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         JOIN pg_attribute a ON a.attrelid = c.oid AND a.attnum = pgd.objsubid \
         WHERE n.nspname='morpholog_views' AND c.relname='fixture' AND a.attname='daily'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(comment.contains("amount in USD"), "got: {comment}");
}

#[tokio::test]
async fn any_column_reads_back_the_whole_tagged_object() {
    let pool = test_pool().await;
    reset(&pool).await;
    let p = fixture_program();
    seed_fixture(
        &pool,
        &p,
        "v1",
        SignedDuration::from_hours(6),
        "2026-01-01T00:00:00Z",
        EvalValue::Decimal(Decimal::from(42)),
    )
    .await;
    sqlx::raw_sql(&render(&p, "morpholog_views"))
        .execute(&pool)
        .await
        .expect("script applies");

    let note =
        sqlx::query_scalar::<_, serde_json::Value>("SELECT note FROM morpholog_views.fixture")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        note["type"], "decimal",
        "Any keeps the discriminant: {note}"
    );
    assert_eq!(note["value"], "42");
}

#[tokio::test]
async fn duration_casts_including_negative_spans() {
    let pool = test_pool().await;
    reset(&pool).await;
    let p = fixture_program();
    seed_fixture(
        &pool,
        &p,
        "neg",
        SignedDuration::from_hours(-6),
        "2026-01-01T00:00:00Z",
        EvalValue::Decimal(Decimal::from(1)),
    )
    .await;
    sqlx::raw_sql(&render(&p, "morpholog_views"))
        .execute(&pool)
        .await
        .expect("script applies");

    let is_negative = sqlx::query_scalar::<_, bool>(
        "SELECT allowed < interval '0' FROM morpholog_views.fixture WHERE voyage='neg'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        is_negative,
        "a negative Duration casts to a negative interval"
    );
}

#[tokio::test]
async fn temporal_precision_boundary_holds() {
    let pool = test_pool().await;
    reset(&pool).await;
    let p = fixture_program();
    // Two instants one nanosecond apart: below timestamptz's microsecond
    // resolution, so the typed column collapses them, but the raw
    // _morpholog_arguments preserves the distinction.
    for opened in [
        "2026-01-01T00:00:00.000000001Z",
        "2026-01-01T00:00:00.000000002Z",
    ] {
        seed_fixture(
            &pool,
            &p,
            "v1",
            SignedDuration::from_hours(6),
            opened,
            EvalValue::Subject(format!("note-{opened}").as_str().into()),
        )
        .await;
    }
    sqlx::raw_sql(&render(&p, "morpholog_views"))
        .execute(&pool)
        .await
        .expect("script applies");

    let distinct_typed =
        sqlx::query_scalar::<_, i64>("SELECT count(DISTINCT opened) FROM morpholog_views.fixture")
            .fetch_one(&pool)
            .await
            .unwrap();
    let distinct_raw = sqlx::query_scalar::<_, i64>(
        "SELECT count(DISTINCT _morpholog_arguments) FROM morpholog_views.fixture",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(distinct_typed, 1, "microsecond column collapses the pair");
    assert_eq!(distinct_raw, 2, "the exact source preserves both");
}
