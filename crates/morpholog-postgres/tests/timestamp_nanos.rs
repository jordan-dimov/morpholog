//! `morpholog.timestamp_nanos` is the coordinate the compiled checks order
//! instants by, so it must agree with jiff's nanosecond count on every
//! stored timestamp: the epoch and its neighbours, both extremes, the leap
//! rules, every fraction width the codec trims to, year zero and negative
//! years, plus a pseudo-random sweep of the whole range.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::test_pool;
use jiff::Timestamp;
use rust_decimal::Decimal;
use serde_json::json;
use sqlx::PgPool;

async fn nanos(pool: &PgPool, value: serde_json::Value) -> Result<Option<Decimal>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<Decimal>>("SELECT morpholog.timestamp_nanos($1)")
        .bind(value)
        .fetch_one(pool)
        .await
}

fn tagged(t: Timestamp) -> serde_json::Value {
    serde_json::to_value(morpholog_core::EvalValue::Timestamp(t)).expect("serialises")
}

const VECTORS: &[&str] = &[
    "1970-01-01T00:00:00Z",
    "1970-01-01T00:00:00.000000001Z",
    "1969-12-31T23:59:59.999999999Z",
    "1969-12-31T23:59:59Z",
    "2000-02-29T00:00:00Z",
    "2000-03-01T00:00:00Z",
    "1900-02-28T23:59:59Z",
    "1900-03-01T00:00:00Z",
    "2100-02-28T12:00:00Z",
    "2024-02-29T23:59:59.999999999Z",
    "1600-02-29T00:00:00Z",
    "2400-02-29T00:00:00Z",
    "-000004-02-29T00:00:00Z",
    "2026-01-01T12:00:00.5Z",
    "2026-01-01T12:00:00.123Z",
    "2026-01-01T12:00:00.123456Z",
    "2026-01-01T12:00:00.123456789Z",
    "2026-12-31T23:59:59Z",
    "0000-01-01T00:00:00Z",
    "0000-02-29T00:00:00Z",
    "0000-12-31T23:59:59.999999999Z",
    "-000001-12-31T23:59:59Z",
    "-000100-03-01T00:00:00Z",
    "-004713-11-24T00:00:00Z",
    "-009999-01-02T01:59:59Z",
    "9999-12-30T22:00:00.999999999Z",
];

#[tokio::test]
async fn the_coordinate_is_jiffs_nanosecond_count() {
    let pool = test_pool().await;
    let mut vectors: Vec<Timestamp> = VECTORS
        .iter()
        .map(|s| s.parse().expect("vector parses"))
        .collect();
    vectors.push(Timestamp::MIN);
    vectors.push(Timestamp::MAX);
    // A linear congruential sweep over the whole range, seeded so a
    // failure names a reproducible instant.
    let span = (Timestamp::MAX.as_nanosecond() - Timestamp::MIN.as_nanosecond()).unsigned_abs();
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..500 {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        // The top 32 bits of x as a fraction of the span; the product
        // stays inside u128.
        let offset = (span * u128::from(x >> 32)) >> 32;
        let nanos = Timestamp::MIN.as_nanosecond() + i128::try_from(offset).unwrap();
        vectors.push(Timestamp::from_nanosecond(nanos).expect("inside the range"));
    }
    for t in vectors {
        let got = nanos(&pool, tagged(t))
            .await
            .unwrap_or_else(|e| panic!("{t}: {e}"))
            .unwrap_or_else(|| panic!("{t}: NULL for a timestamp"));
        let expected: Decimal = t.as_nanosecond().to_string().parse().unwrap();
        assert_eq!(got, expected, "{t}");
    }
}

#[tokio::test]
async fn another_tag_is_null_and_a_malformed_timestamp_is_an_error() {
    let pool = test_pool().await;
    // A subject whose text would parse as a timestamp is still a subject.
    let subject = json!({"type": "subject", "value": "2026-01-01T00:00:00Z"});
    assert_eq!(nanos(&pool, subject).await.unwrap(), None);
    let date = json!({"type": "date", "value": "2026-01-01"});
    assert_eq!(nanos(&pool, date).await.unwrap(), None);
    for bad in [
        json!({"type": "timestamp", "value": "2026-01-01T00:00:00+02:00"}),
        json!({"type": "timestamp", "value": "2026-01-01 00:00:00Z"}),
        json!({"type": "timestamp", "value": "2026-01-01T00:00:00.1234567890Z"}),
        json!({"type": "timestamp", "value": 5}),
        // Shaped like the codec's text, but no such instant exists, or
        // the codec would never spell it so.
        json!({"type": "timestamp", "value": "2026-02-30T00:00:00Z"}),
        json!({"type": "timestamp", "value": "2025-02-29T00:00:00Z"}),
        json!({"type": "timestamp", "value": "2026-13-01T00:00:00Z"}),
        json!({"type": "timestamp", "value": "2026-01-01T25:00:00Z"}),
        json!({"type": "timestamp", "value": "2026-01-01T00:60:00Z"}),
        json!({"type": "timestamp", "value": "2026-01-01T00:00:60Z"}),
        json!({"type": "timestamp", "value": "002026-01-01T00:00:00Z"}),
        json!({"type": "timestamp", "value": "-0001-01-01T00:00:00Z"}),
        json!({"type": "timestamp", "value": "-010000-01-01T00:00:00Z"}),
        json!({"type": "timestamp", "value": "-000000-01-01T00:00:00Z"}),
    ] {
        let err = nanos(&pool, bad.clone())
            .await
            .expect_err(&format!("{bad} must be refused"));
        assert!(
            err.to_string().contains("not a stored timestamp"),
            "{bad}: {err}"
        );
    }
}

/// The embedded schema and migration 016 define each function once; the
/// bodies must be the same text, and each carries its marker.
#[test]
fn the_schema_and_the_migration_define_the_same_functions() {
    fn body<'a>(sql: &'a str, head: &str) -> &'a str {
        let from = sql.find(head).unwrap_or_else(|| panic!("{head} missing"));
        let sql = &sql[from..];
        let start = sql.find("AS $$\n").expect("function body start") + "AS $$\n".len();
        let end = sql[start..].find("\n$$;").expect("function body end") + start;
        &sql[start..end]
    }
    let schema = include_str!("../../morpholog-core/sql/schema.sql");
    let migration = include_str!("../../morpholog-core/sql/migrations/016_timestamp_nanos.sql");
    for (name, marker) in [
        ("timestamp_nanos", "morpholog timestamp coordinate v1"),
        ("declared_kind", "morpholog declared kind guard v1"),
    ] {
        assert_eq!(
            body(schema, &format!("CREATE FUNCTION {name}(")),
            body(
                migration,
                &format!("CREATE OR REPLACE FUNCTION morpholog.{name}(")
            ),
            "{name}"
        );
        assert!(
            schema.contains(&format!("IS '{marker}'")),
            "{name} marker in the schema"
        );
        assert!(
            migration.contains(&format!("IS '{marker}'")),
            "{name} marker in the migration"
        );
    }
}

/// A row of another kind at a guarded position is refused with the
/// position named; a row of another predicate passes, whatever the
/// planner evaluated first.
#[tokio::test]
async fn the_guard_names_the_drift_and_ignores_other_predicates() {
    let pool = test_pool().await;
    let ok: bool =
        sqlx::query_scalar("SELECT morpholog.declared_kind('Terms', 'Terms', $1, 'quantity', 1)")
            .bind(json!({"type":"quantity","value":{"amount":"1","unit":"MW"}}))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(ok);
    let other: bool =
        sqlx::query_scalar("SELECT morpholog.declared_kind('Noise', 'Terms', $1, 'quantity', 1)")
            .bind(json!({"type":"subject","value":"legacy"}))
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(other);
    let err = sqlx::query_scalar::<_, bool>(
        "SELECT morpholog.declared_kind('Terms', 'Terms', $1, 'quantity', 1)",
    )
    .bind(json!({"type":"subject","value":"legacy"}))
    .fetch_one(&pool)
    .await
    .expect_err("drift is refused");
    let db = err.as_database_error().expect("a database error");
    assert_eq!(db.code().as_deref(), Some("MP001"));
    assert_eq!(
        db.message(),
        "Terms[1] holds a subject where the programme declares quantity"
    );
}
