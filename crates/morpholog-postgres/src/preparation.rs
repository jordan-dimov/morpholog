//! Migration 27 prepares in steps before its own transaction, so the
//! work that needs no long lock (columns, constraints, indexes, the
//! order's functions) runs with the database in service, and the one
//! transaction that pauses writers holds only EXCLUSIVE locks. Every
//! step is idempotent and checks the objects it owns, so a run resumes
//! after a crash or a hand change by rerunning; completed steps are
//! recorded in `morpholog.migration_progress` for reporting.
//!
//! The preflight says what the cutover would cost and refuses it while a
//! fact it depends on cannot be established: a prepared transaction, a
//! session this login cannot see, a baseline the record cannot hold.

use std::time::Duration;

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::error::{PgError, classify, classify_checked_query};

pub(crate) const CANONICAL_ORDER_VERSION: i32 = 27;

/// The preparation steps, in order.
pub(crate) const STEPS: [&str; 4] = ["columns", "constraints", "indexes", "objects"];

const COLUMNS_SQL: &str = include_str!("../sql/migrations/027/columns.sql");
const OBJECTS_SQL: &str = include_str!("../sql/migrations/027/objects.sql");

/// The check constraints, added `NOT VALID` (a brief ACCESS EXCLUSIVE
/// lock, no scan) and validated afterwards (a scan under a lock that
/// blocks neither reads nor writes).
const CHECKS: [(&str, &str); 3] = [
    (
        "audit_operations_array",
        "jsonb_typeof(operations) = 'array'",
    ),
    (
        "audit_order_all_or_none",
        "(operations IS NULL) = (audit_order = 0) \
         AND (operations IS NULL) = (ordered_at IS NULL) \
         AND (audit_order > 0 OR act_ordinal = 0)",
    ),
    (
        "audit_activation_attested",
        "(transformation_name = 'morpholog.activate') = (attestation ->> 'mode' = 'migration')",
    ),
];

/// The indexes, built `CONCURRENTLY`: two scans each, no lock a reader or
/// writer notices. A build that fails leaves an invalid index behind,
/// which `IF NOT EXISTS` would keep; it is dropped and rebuilt instead.
const INDEXES: [(&str, &str); 2] = [
    (
        "audit_one_activation",
        "CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS audit_one_activation \
         ON morpholog.audit (transformation_name) \
         WHERE transformation_name = 'morpholog.activate'",
    ),
    (
        "audit_canonical",
        "CREATE INDEX CONCURRENTLY IF NOT EXISTS audit_canonical \
         ON morpholog.audit (audit_order, act_ordinal, committed_at, transition_id)",
    ),
];

const PROGRESS_SQL: &str = "CREATE TABLE IF NOT EXISTS morpholog.migration_progress (
    version    int         NOT NULL,
    step       text        NOT NULL,
    detail     jsonb       NOT NULL DEFAULT '{}'::jsonb,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (version, step)
)";

/// PostgreSQL's cap on the bytes of a JSONB array's elements, which is
/// what the activation record's baseline is.
pub const BASELINE_LIMIT_BYTES: i64 = (1 << 28) - 1;

/// Measured on 2026-10-30 (release build, laptop): preparation scans
/// about 0.6 us per audit row; the cutover's legacy pass 11.5 us per row
/// and its baseline 3.5 us per live claim.
const PREPARATION_NS_PER_ROW: i64 = 600;
const PAUSE_NS_PER_ROW: i64 = 11_500;
const PAUSE_NS_PER_CLAIM: i64 = 3_500;

/// How a brief ACCESS EXCLUSIVE lock is taken: with a short timeout and
/// bounded retries, so a request never queues behind a long-running
/// reader and holds every later reader with it.
#[derive(Debug, Clone)]
pub struct LockPolicy {
    pub timeout: Duration,
    pub attempts: u32,
    pub backoff: Duration,
    pub jitter: Duration,
    pub cap: Duration,
}

impl Default for LockPolicy {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(1),
            attempts: 10,
            backoff: Duration::from_millis(200),
            jitter: Duration::from_millis(300),
            cap: Duration::from_secs(5),
        }
    }
}

impl LockPolicy {
    fn delay(&self, attempt: u32) -> Duration {
        let base = self
            .backoff
            .saturating_mul(1 << attempt.min(16))
            .min(self.cap);
        let nanos = Timestamp::now().subsec_nanosecond().unsigned_abs();
        let jitter_ms = u64::from(nanos) % (self.jitter.as_millis().max(1) as u64);
        base + Duration::from_millis(jitter_ms)
    }
}

/// Where the preparation of migration 27 stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreparationState {
    /// No step has run.
    Pending,
    /// Some steps are done.
    Preparing,
    /// Every step is done; only the cutover transaction remains.
    Ready,
}

/// What the cutover of migration 27 would cost, and what stands in its
/// way. Reported by `migrate --check` while the migration is pending and
/// asked again by `migrate` before it pauses writers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivationPreflight {
    pub state: PreparationState,
    pub steps_done: Vec<String>,
    pub legacy_rows: i64,
    pub audit_bytes: i64,
    pub live_claims: i64,
    /// The bytes the baseline's JSONB elements would take, as PostgreSQL
    /// counts them against `baseline_limit_bytes`.
    pub baseline_bytes: i64,
    pub baseline_limit_bytes: i64,
    pub estimated_preparation_ms: i64,
    pub estimated_writer_pause_ms: i64,
    /// Why the cutover is refused now; empty when it may proceed.
    pub refusals: Vec<String>,
}

/// The census that makes a start-time horizon over the legacy rows
/// trustworthy: taken as two statements in this order, sessions then
/// prepared transactions. A session in the census that prepares
/// afterwards started at or after the horizon; one that prepared before
/// the census is absent from it and is caught by the count that follows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Census {
    pub horizon: Timestamp,
    pub hidden_sessions: i64,
    pub prepared_transactions: i64,
}

impl Census {
    pub fn refusals(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.hidden_sessions > 0 {
            out.push(format!(
                "{} session(s) on this database are hidden from this login; run as a role \
                 that can see them (`pg_read_all_stats`, or the writers' role)",
                self.hidden_sessions
            ));
        }
        if self.prepared_transactions > 0 {
            out.push(format!(
                "{} prepared transaction(s) are open on this database; commit or roll \
                 them back first (`pg_prepared_xacts`)",
                self.prepared_transactions
            ));
        }
        out
    }
}

/// Take the census, pausing between its two statements: the seam a test
/// uses to prepare a transaction during the census.
#[doc(hidden)]
pub async fn census_paused(
    pool: &PgPool,
    pause: impl Future<Output = ()>,
) -> Result<Census, PgError> {
    let mut conn = pool.acquire().await.map_err(classify)?;
    // `horizon!` / `hidden!`: coalesce(_, now()) and count(*) are never
    // null. An unseen session shows '<insufficient privilege>' as its
    // query and hides its start, so it is counted, not skipped.
    let sessions = sqlx::query!(
        r#"SELECT coalesce(min(xact_start), now()) AS "horizon!",
                  count(*) FILTER (WHERE query = '<insufficient privilege>') AS "hidden!"
           FROM pg_stat_activity
           WHERE datname = current_database()
             AND pid <> pg_backend_pid()
             AND backend_type IS DISTINCT FROM 'autovacuum worker'"#
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(classify_checked_query)?;
    pause.await;
    // `prepared!`: count(*) is never null.
    let prepared = sqlx::query_scalar!(
        r#"SELECT count(*) AS "prepared!" FROM pg_prepared_xacts
           WHERE database = current_database()"#
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(classify_checked_query)?;
    Ok(Census {
        horizon: sessions.horizon.to_jiff(),
        hidden_sessions: sessions.hidden,
        prepared_transactions: prepared,
    })
}

pub async fn census(pool: &PgPool) -> Result<Census, PgError> {
    census_paused(pool, async {}).await
}

/// The steps recorded done for migration 27, in step order.
async fn steps_done(pool: &PgPool) -> Result<Vec<String>, PgError> {
    let present = sqlx::query_scalar!(
        r#"SELECT to_regclass('morpholog.migration_progress') IS NOT NULL AS "present!""#
    )
    .fetch_one(pool)
    .await
    .map_err(classify_checked_query)?;
    if !present {
        return Ok(Vec::new());
    }
    let done = sqlx::query_scalar!(
        "SELECT step FROM morpholog.migration_progress WHERE version = $1",
        CANONICAL_ORDER_VERSION
    )
    .fetch_all(pool)
    .await
    .map_err(classify_checked_query)?;
    Ok(STEPS
        .iter()
        .filter(|s| done.iter().any(|d| d == *s))
        .map(|s| (*s).to_string())
        .collect())
}

async fn mark_done(pool: &PgPool, step: &str, detail: serde_json::Value) -> Result<(), PgError> {
    sqlx::query!(
        "INSERT INTO morpholog.migration_progress (version, step, detail)
         VALUES ($1, $2, $3)
         ON CONFLICT (version, step) DO UPDATE
            SET detail = EXCLUDED.detail, updated_at = now()",
        CANONICAL_ORDER_VERSION,
        step,
        detail,
    )
    .execute(pool)
    .await
    .map_err(classify_checked_query)?;
    Ok(())
}

/// Run `sql`, which takes a brief ACCESS EXCLUSIVE lock, under the policy:
/// each attempt in its own transaction with `lock_timeout`, retried only
/// when that timeout fired (`55P03`), so the transaction is already
/// rolled back; any other error stops the run.
async fn with_brief_lock(pool: &PgPool, policy: &LockPolicy, sql: &str) -> Result<u32, PgError> {
    for attempt in 0..policy.attempts {
        let mut tx = pool.begin().await.map_err(classify)?;
        let set = format!(
            "SET LOCAL lock_timeout = '{}ms'",
            policy.timeout.as_millis()
        );
        sqlx::raw_sql(sqlx::AssertSqlSafe(set))
            .execute(&mut *tx)
            .await
            .map_err(classify)?;
        match sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
            .execute(&mut *tx)
            .await
        {
            Ok(_) => {
                tx.commit().await.map_err(classify)?;
                return Ok(attempt + 1);
            }
            Err(err) if lock_timed_out(&err) => {
                drop(tx);
                tokio::time::sleep(policy.delay(attempt)).await;
            }
            Err(err) => return Err(classify(err)),
        }
    }
    Err(PgError::LockNotAcquired {
        statement: first_line(sql),
        attempts: policy.attempts,
    })
}

fn lock_timed_out(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .is_some_and(|code| code == "55P03")
}

fn first_line(sql: &str) -> String {
    sql.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("--"))
        .unwrap_or("")
        .to_string()
}

async fn step_columns(pool: &PgPool, policy: &LockPolicy) -> Result<serde_json::Value, PgError> {
    let missing = sqlx::query_scalar!(
        r#"SELECT count(*) AS "missing!" FROM unnest(ARRAY['audit_order', 'act_ordinal', 'ordered_at', 'operations']) AS wanted(name)
           WHERE NOT EXISTS (
               SELECT 1 FROM pg_attribute
               WHERE attrelid = 'morpholog.audit'::regclass AND attname = wanted.name AND NOT attisdropped)"#
    )
    .fetch_one(pool)
    .await
    .map_err(classify_checked_query)?;
    if missing == 0 {
        return Ok(serde_json::json!({ "attempts": 0 }));
    }
    let attempts = with_brief_lock(pool, policy, COLUMNS_SQL).await?;
    Ok(serde_json::json!({ "attempts": attempts }))
}

async fn step_constraints(
    pool: &PgPool,
    policy: &LockPolicy,
) -> Result<serde_json::Value, PgError> {
    let mut added = Vec::new();
    let mut validated = Vec::new();
    for (name, expression) in CHECKS {
        let state = sqlx::query!(
            r#"SELECT convalidated AS "validated!" FROM pg_constraint
               WHERE conrelid = 'morpholog.audit'::regclass AND conname = $1"#,
            name
        )
        .fetch_optional(pool)
        .await
        .map_err(classify_checked_query)?;
        let valid = match state {
            Some(row) => row.validated,
            None => {
                let sql = format!(
                    "ALTER TABLE morpholog.audit ADD CONSTRAINT {name} CHECK ({expression}) NOT VALID"
                );
                with_brief_lock(pool, policy, &sql).await?;
                added.push(name);
                false
            }
        };
        if !valid {
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "ALTER TABLE morpholog.audit VALIDATE CONSTRAINT {name}"
            )))
            .execute(pool)
            .await
            .map_err(classify)?;
            validated.push(name);
        }
    }
    Ok(serde_json::json!({ "added": added, "validated": validated }))
}

async fn step_indexes(pool: &PgPool) -> Result<serde_json::Value, PgError> {
    let mut built = Vec::new();
    let mut rebuilt = Vec::new();
    for (name, create) in INDEXES {
        let state = sqlx::query!(
            r#"SELECT i.indisvalid AS "valid!" FROM pg_index i
               JOIN pg_class c ON c.oid = i.indexrelid
               JOIN pg_namespace n ON n.oid = c.relnamespace
               WHERE n.nspname = 'morpholog' AND c.relname = $1"#,
            name
        )
        .fetch_optional(pool)
        .await
        .map_err(classify_checked_query)?;
        match state {
            Some(row) if row.valid => continue,
            Some(_) => {
                sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                    "DROP INDEX CONCURRENTLY morpholog.{name}"
                )))
                .execute(pool)
                .await
                .map_err(classify)?;
                rebuilt.push(name);
            }
            None => built.push(name),
        }
        sqlx::raw_sql(sqlx::AssertSqlSafe(create.to_string()))
            .execute(pool)
            .await
            .map_err(classify)?;
    }
    Ok(serde_json::json!({ "built": built, "rebuilt": rebuilt }))
}

async fn step_objects(pool: &PgPool) -> Result<serde_json::Value, PgError> {
    sqlx::raw_sql(OBJECTS_SQL)
        .execute(pool)
        .await
        .map_err(classify)?;
    Ok(serde_json::json!({}))
}

/// Run every preparation step, each idempotent, recording each as done.
pub(crate) async fn prepare_canonical_order(
    pool: &PgPool,
    policy: &LockPolicy,
) -> Result<(), PgError> {
    sqlx::raw_sql(PROGRESS_SQL)
        .execute(pool)
        .await
        .map_err(classify)?;
    for step in STEPS {
        let detail = match step {
            "columns" => step_columns(pool, policy).await?,
            "constraints" => step_constraints(pool, policy).await?,
            "indexes" => step_indexes(pool).await?,
            _ => step_objects(pool).await?,
        };
        mark_done(pool, step, detail).await?;
    }
    Ok(())
}

/// Forget the recorded steps once the migration itself is recorded.
pub(crate) async fn clear_progress(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), PgError> {
    sqlx::query!(
        "DELETE FROM morpholog.migration_progress WHERE version = $1",
        CANONICAL_ORDER_VERSION
    )
    .execute(&mut **tx)
    .await
    .map_err(classify_checked_query)?;
    Ok(())
}

/// The preflight for a database on which migration 27 is pending.
pub async fn activation_preflight(pool: &PgPool) -> Result<ActivationPreflight, PgError> {
    activation_preflight_within(pool, BASELINE_LIMIT_BYTES).await
}

/// The preflight against a stated baseline limit: the test seam for the
/// refusal a real limit needs a million claims to reach.
#[doc(hidden)]
pub async fn activation_preflight_within(
    pool: &PgPool,
    baseline_limit_bytes: i64,
) -> Result<ActivationPreflight, PgError> {
    let steps_done = steps_done(pool).await?;
    let state = match steps_done.len() {
        0 => PreparationState::Pending,
        n if n == STEPS.len() => PreparationState::Ready,
        _ => PreparationState::Preparing,
    };
    // `rows!` / `bytes!`: count(*) and pg_total_relation_size are never
    // null for a table that exists.
    let audit = sqlx::query!(
        r#"SELECT count(*) AS "rows!",
                  pg_total_relation_size('morpholog.audit') AS "bytes!"
           FROM morpholog.audit"#
    )
    .fetch_one(pool)
    .await
    .map_err(classify_checked_query)?;
    // The bytes each baseline element takes as JSONB, which is what the
    // limit counts; `claims!` / `bytes!`: count and coalesce are never null.
    let claims = sqlx::query!(
        r#"SELECT count(*) AS "claims!",
                  coalesce(sum(pg_column_size(jsonb_build_object(
                      'op', 'ins', 'predicate', predicate_name,
                      'args', arguments, 'asserted_in', asserted_in))), 0)::bigint AS "bytes!"
           FROM morpholog.claims"#
    )
    .fetch_one(pool)
    .await
    .map_err(classify_checked_query)?;
    let mut refusals = census(pool).await?.refusals();
    if claims.bytes >= baseline_limit_bytes {
        refusals.push(format!(
            "the baseline of {} live claims would take {} bytes as JSONB, over the \
             {} byte limit of one activation record; this register cannot activate \
             under this release",
            claims.claims, claims.bytes, baseline_limit_bytes
        ));
    }
    Ok(ActivationPreflight {
        state,
        steps_done,
        legacy_rows: audit.rows,
        audit_bytes: audit.bytes,
        live_claims: claims.claims,
        baseline_bytes: claims.bytes,
        baseline_limit_bytes,
        estimated_preparation_ms: audit.rows * PREPARATION_NS_PER_ROW / 1_000_000,
        estimated_writer_pause_ms: (audit.rows * PAUSE_NS_PER_ROW
            + claims.claims * PAUSE_NS_PER_CLAIM)
            / 1_000_000,
        refusals,
    })
}
