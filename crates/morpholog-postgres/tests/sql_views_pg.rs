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
use common::test_pool;

use jiff::{SignedDuration, Timestamp};
use morpholog_core::{EvalValue, Program, Unit};
use morpholog_postgres::{PgPool, refresh_derived, render_views};
use morpholog_surface::parse_program;
use rust_decimal::Decimal;

const SENTINEL_HASH: &str =
    "sha256:0000000000000000000000000000000000000000000000000000000000000000";

/// This suite's reset is unique - beyond the governed tables and the
/// derived cache it must drop the generated `morpholog_views` (and the
/// `analytics` schema a test renames into), so it keeps its own local
/// reset rather than sharing `common::reset_db_and_read_cache`.
async fn reset(pool: &PgPool) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "{}; TRUNCATE morpholog_read.derived_claims, morpholog_read.derived_active, \
                  morpholog_read.derived_refreshes CASCADE; \
         DROP SCHEMA IF EXISTS morpholog_views CASCADE; \
         DROP SCHEMA IF EXISTS analytics CASCADE;",
        morpholog_postgres::testing::RESET_SQL
    )))
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
        common::propose_pg_with_test_actor(pool, &common::compiled(p.clone()), open, args)
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
    sqlx::raw_sql(sqlx::AssertSqlSafe(render(&p, "morpholog_views")))
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

    let result = sqlx::raw_sql(sqlx::AssertSqlSafe(render(&p, "morpholog_views")))
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
    sqlx::raw_sql(sqlx::AssertSqlSafe(render(&v1, "morpholog_views")))
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
    sqlx::raw_sql(sqlx::AssertSqlSafe(render(&v2, "morpholog_views")))
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
    sqlx::raw_sql(sqlx::AssertSqlSafe(render(&v1, "morpholog_views")))
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
    let result = sqlx::raw_sql(sqlx::AssertSqlSafe(render(&renamed, "morpholog_views")))
        .execute(&pool)
        .await;
    assert!(result.is_err(), "renaming a column must be rejected");
}

#[tokio::test]
async fn catalog_round_trips_the_model_hash() {
    let pool = test_pool().await;
    reset(&pool).await;
    let p = fixture_program();
    sqlx::raw_sql(sqlx::AssertSqlSafe(render(&p, "morpholog_views")))
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
    sqlx::raw_sql(sqlx::AssertSqlSafe(render(&p, "morpholog_views")))
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
    sqlx::raw_sql(sqlx::AssertSqlSafe(render(&p, "morpholog_views")))
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
    sqlx::raw_sql(sqlx::AssertSqlSafe(render(&p, "morpholog_views")))
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
    sqlx::raw_sql(sqlx::AssertSqlSafe(render(&p, "morpholog_views")))
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

// ---- derived views over the morpholog_read cache ----
//
// A derived view projects the kernel-computed read cache, not
// `morpholog.claims`. It reads the active generation whose model hash
// matches the generated surface, so it is empty until `refresh derived`
// has run for the same programme. The kernel stays the sole evaluator.

const DERIVED_FIXTURE: &str = "program dv\n\
    predicate Entry(account: Subject, amount: Decimal)\n\
    predicate AccountTotal(account: Subject, total: Decimal)\n\
    transformation post(account, amount):\n    admit Entry(account, amount)\n\
    derived AccountTotal(account):\n    over Entry(account, _)\n    \
        value total = sum(a | Entry(account, a))\n";

fn derived_fixture() -> Program {
    let p = parse_program(DERIVED_FIXTURE).expect("dv parses");
    p.validate().expect("dv validates");
    p
}

async fn seed_entry(pool: &PgPool, p: &Program, account: &str, amount: i64) {
    let post = p.transformation("post").unwrap();
    let args = vec![
        EvalValue::Subject(account.into()),
        EvalValue::Decimal(Decimal::from(amount)),
    ];
    let outcome =
        common::propose_pg_with_test_actor(pool, &common::compiled(p.clone()), post, args)
            .await
            .expect("post proposes");
    assert!(
        matches!(
            outcome,
            morpholog_postgres::PgProposalOutcome::Committed { .. }
        ),
        "post commits: {outcome:?}"
    );
}

async fn account_total(pool: &PgPool, account: &str) -> Option<Decimal> {
    sqlx::query_scalar::<_, Decimal>(
        "SELECT total FROM morpholog_views.account_total WHERE account = $1",
    )
    .bind(account)
    .fetch_optional(pool)
    .await
    .expect("query account_total view")
}

#[tokio::test]
async fn derived_view_returns_kernel_rows_after_refresh() {
    let pool = test_pool().await;
    reset(&pool).await;
    let p = derived_fixture();
    seed_entry(&pool, &p, "a1", 10).await;
    seed_entry(&pool, &p, "a1", 5).await;
    refresh_derived(&pool, p.validated().unwrap(), SENTINEL_HASH)
        .await
        .unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(render(&p, "morpholog_views")))
        .execute(&pool)
        .await
        .expect("script applies");
    assert_eq!(account_total(&pool, "a1").await, Some(Decimal::from(15)));
}

#[tokio::test]
async fn derived_view_is_empty_before_any_refresh() {
    let pool = test_pool().await;
    reset(&pool).await;
    let p = derived_fixture();
    seed_entry(&pool, &p, "a1", 10).await;
    // Views applied, but no refresh has published a generation yet.
    sqlx::raw_sql(sqlx::AssertSqlSafe(render(&p, "morpholog_views")))
        .execute(&pool)
        .await
        .expect("script applies");
    assert_eq!(account_total(&pool, "a1").await, None);
}

#[tokio::test]
async fn derived_view_is_empty_for_a_mismatched_model_hash() {
    let pool = test_pool().await;
    reset(&pool).await;
    let p = derived_fixture();
    seed_entry(&pool, &p, "a1", 10).await;
    // The active generation is from a DIFFERENT model than the views are
    // generated for: the view filters on its own model hash and shows
    // nothing, rather than projecting another model's rows.
    refresh_derived(&pool, p.validated().unwrap(), "sha256:another-model")
        .await
        .unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(render(&p, "morpholog_views")))
        .execute(&pool)
        .await
        .expect("script applies");
    assert_eq!(account_total(&pool, "a1").await, None);
    // Refreshing for the matching model surfaces the row.
    refresh_derived(&pool, p.validated().unwrap(), SENTINEL_HASH)
        .await
        .unwrap();
    assert_eq!(account_total(&pool, "a1").await, Some(Decimal::from(10)));
}

#[tokio::test]
async fn derived_view_is_not_updatable() {
    let pool = test_pool().await;
    reset(&pool).await;
    let p = derived_fixture();
    seed_entry(&pool, &p, "a1", 10).await;
    refresh_derived(&pool, p.validated().unwrap(), SENTINEL_HASH)
        .await
        .unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(render(&p, "morpholog_views")))
        .execute(&pool)
        .await
        .expect("script applies");
    let err = sqlx::query("DELETE FROM morpholog_views.account_total")
        .execute(&pool)
        .await
        .expect_err("a write through the derived view must fail");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("cannot") || msg.contains("updat"),
        "expected a non-updatable-view error, got: {msg}"
    );
}

#[tokio::test]
async fn derived_view_reflects_a_new_generation() {
    let pool = test_pool().await;
    reset(&pool).await;
    let p = derived_fixture();
    seed_entry(&pool, &p, "a1", 10).await;
    refresh_derived(&pool, p.validated().unwrap(), SENTINEL_HASH)
        .await
        .unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(render(&p, "morpholog_views")))
        .execute(&pool)
        .await
        .expect("script applies");
    assert_eq!(account_total(&pool, "a1").await, Some(Decimal::from(10)));
    // A new claim and a new refresh generation: the view follows the
    // active-generation pointer.
    seed_entry(&pool, &p, "a1", 7).await;
    refresh_derived(&pool, p.validated().unwrap(), SENTINEL_HASH)
        .await
        .unwrap();
    assert_eq!(account_total(&pool, "a1").await, Some(Decimal::from(17)));
}
