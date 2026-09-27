//! `morpholog.date_ordinal` is the coordinate the compiled checks order
//! civil dates by, so it must order every stored date as jiff does and
//! agree with the literal ordinal the compiler computes: the epoch and
//! its neighbours, both extremes, the leap rules, year zero and negative
//! years, plus a pseudo-random sweep of the whole range.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::test_pool;
use jiff::ToSpan as _;
use jiff::civil::Date;
use serde_json::json;
use sqlx::PgPool;

async fn ordinal(pool: &PgPool, value: serde_json::Value) -> Result<Option<i32>, sqlx::Error> {
    sqlx::query_scalar::<_, Option<i32>>("SELECT morpholog.date_ordinal($1)")
        .bind(value)
        .fetch_one(pool)
        .await
}

fn tagged(d: Date) -> serde_json::Value {
    serde_json::to_value(morpholog_core::EvalValue::Date(d)).expect("serialises")
}

fn expected(d: Date) -> i32 {
    i32::from(d.year()) * 10000 + i32::from(d.month()) * 100 + i32::from(d.day())
}

const VECTORS: &[&str] = &[
    "1970-01-01",
    "1969-12-31",
    "2000-02-29",
    "2000-03-01",
    "1900-02-28",
    "1900-03-01",
    "2024-02-29",
    "1600-02-29",
    "-000004-02-29",
    "2026-12-31",
    "0000-01-01",
    "0000-02-29",
    "0000-12-31",
    "-000001-12-31",
    "-000001-01-01",
    "-000100-03-01",
    "-009999-01-01",
    "9999-12-31",
];

#[tokio::test]
async fn the_coordinate_orders_dates_as_jiff_does() {
    let pool = test_pool().await;
    let mut vectors: Vec<Date> = VECTORS
        .iter()
        .map(|s| s.parse().expect("vector parses"))
        .collect();
    vectors.push(Date::MIN);
    vectors.push(Date::MAX);
    let span_days = Date::MIN.until(Date::MAX).unwrap().get_days();
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..500 {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let offset =
            i32::try_from((u64::from(span_days.unsigned_abs()) * (x >> 32)) >> 32).unwrap();
        vectors.push(
            Date::MIN
                .checked_add(offset.days())
                .expect("inside the range"),
        );
    }
    let mut previous: Option<(Date, i32)> = None;
    vectors.sort();
    for d in vectors {
        let got = ordinal(&pool, tagged(d))
            .await
            .unwrap_or_else(|e| panic!("{d}: {e}"))
            .unwrap_or_else(|| panic!("{d}: NULL for a date"));
        assert_eq!(got, expected(d), "{d}");
        if let Some((earlier, ordinal)) = previous
            && earlier < d
        {
            assert!(ordinal < got, "{earlier} before {d} but {ordinal} >= {got}");
        }
        previous = Some((d, got));
    }
}

#[tokio::test]
async fn another_tag_is_null_and_a_malformed_date_is_an_error() {
    let pool = test_pool().await;
    let subject = json!({"type": "subject", "value": "2026-01-01"});
    assert_eq!(ordinal(&pool, subject).await.unwrap(), None);
    let timestamp = json!({"type": "timestamp", "value": "2026-01-01T00:00:00Z"});
    assert_eq!(ordinal(&pool, timestamp).await.unwrap(), None);
    for bad in [
        json!({"type": "date", "value": "2026-1-1"}),
        json!({"type": "date", "value": "2026-01-01T00:00:00Z"}),
        json!({"type": "date", "value": 5}),
        json!({"type": "date", "value": "2026-02-30"}),
        json!({"type": "date", "value": "2025-02-29"}),
        json!({"type": "date", "value": "2026-13-01"}),
        json!({"type": "date", "value": "002026-01-01"}),
        json!({"type": "date", "value": "-0001-01-01"}),
        json!({"type": "date", "value": "-010000-01-01"}),
        json!({"type": "date", "value": "-000000-01-01"}),
    ] {
        let err = ordinal(&pool, bad.clone())
            .await
            .expect_err(&format!("{bad} must be refused"));
        assert!(
            err.to_string().contains("not a stored date"),
            "{bad}: {err}"
        );
    }
}
