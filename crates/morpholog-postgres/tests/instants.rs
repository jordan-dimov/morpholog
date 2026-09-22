//! An adapter instant crosses into PostgreSQL as microseconds counted
//! from 2000-01-01, so finer digits are dropped toward that epoch: an
//! instant just before it rounds up, one just after rounds down. The
//! vectors straddle the epoch with sub-microsecond parts on both sides,
//! and each is read back twice - as the database spells it, and decoded
//! - so the encoding is pinned apart from the decoder.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use jiff::Timestamp;
use jiff_sqlx::ToSqlx;

mod common;
use common::test_pool;

#[tokio::test]
async fn instants_lose_sub_microsecond_digits_toward_the_postgres_epoch() {
    let pool = test_pool().await;
    for (sent, stored) in [
        ("2000-01-01T00:00:00Z", "2000-01-01 00:00:00"),
        ("2000-01-01T00:00:00.000000900Z", "2000-01-01 00:00:00"),
        (
            "2000-01-01T00:00:00.000001500Z",
            "2000-01-01 00:00:00.000001",
        ),
        ("1999-12-31T23:59:59.999999100Z", "2000-01-01 00:00:00"),
        (
            "1999-12-31T23:59:59.999998500Z",
            "1999-12-31 23:59:59.999999",
        ),
        (
            "2026-06-01T12:00:00.123456789Z",
            "2026-06-01 12:00:00.123456",
        ),
    ] {
        let at: Timestamp = sent.parse().unwrap();
        let (spelled, decoded): (String, jiff_sqlx::Timestamp) =
            sqlx::query_as("SELECT ($1::timestamptz AT TIME ZONE 'UTC')::text, $1::timestamptz")
                .bind(at.to_sqlx())
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(spelled, stored, "the database's reading of {sent}");
        let expected: Timestamp = format!("{}Z", stored.replace(' ', "T")).parse().unwrap();
        assert_eq!(decoded.to_jiff(), expected, "decoded {sent}");
    }
}
