//! Migration 27 prepares in steps before its own transaction, so the
//! work that needs no long lock (columns, constraints, indexes, the
//! order's functions) runs with the database in service, and the one
//! transaction that pauses writers holds only EXCLUSIVE locks. Every
//! step is idempotent and checks the objects it owns, so a run resumes
//! after a crash or a hand change by rerunning; an object of another
//! shape under one of its names is refused, never changed. Completed
//! steps are recorded in `morpholog.migration_progress` for reporting.
//!
//! The legacy history's Merkle root is built here too, in passes below a
//! start-time horizon from the census, with the frontier saved after
//! every page, so the cutover hashes only the rows written since the
//! last pass. One runner at a time holds the migration lock.
//!
//! The preflight says what the cutover would cost and refuses it while a
//! fact it depends on cannot be established: a prepared transaction, a
//! session this login cannot see, a baseline the record cannot hold.

use std::time::Duration;

use jiff::Timestamp;
use jiff_sqlx::ToSqlx;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

use crate::audit::{AuditPosition, REPLAY_CHUNK, list_legacy_rows_page};
use crate::error::{PgError, classify, classify_checked_query};
use crate::merkle::{Digest, Frontier, Hash, audit_leaf_hash, merkle_root};

pub(crate) const CANONICAL_ORDER_VERSION: i32 = 27;

/// The preparation steps, in order.
pub(crate) const STEPS: [&str; 5] = ["columns", "constraints", "indexes", "objects", FRONTIER];

const FRONTIER: &str = "frontier";

/// The saved frontier's own row: written after every page, so a pass
/// resumes from it, and never counted as the step being done.
const FRONTIER_STATE: &str = "frontier_state";

/// How many passes one preparation makes over the legacy rows before
/// leaving the rest to the cutover.
const MAX_PASSES: usize = 10;

/// The advisory lock one `migrate` holds for its whole run on a database.
pub const MIGRATION_LOCK_KEY: i64 = 0x6d6f_7270_686f_6c6f;

const COLUMNS_SQL: &str = include_str!("../sql/migrations/027/columns.sql");
const OBJECTS_SQL: &str = include_str!("../sql/migrations/027/objects.sql");

/// The check constraints, added `NOT VALID` (a brief ACCESS EXCLUSIVE
/// lock, no scan) and validated afterwards (a scan under a lock that
/// blocks neither reads nor writes). Beside each expression, the shape
/// the catalog gives it back in, which an existing constraint under the
/// name must match.
const CHECKS: [(&str, &str, &str); 3] = [
    (
        "audit_operations_array",
        "jsonb_typeof(operations) = 'array'",
        "CHECK ((jsonb_typeof(operations) = 'array'::text))",
    ),
    (
        "audit_order_all_or_none",
        "(operations IS NULL) = (audit_order = 0) \
         AND (operations IS NULL) = (ordered_at IS NULL) \
         AND (audit_order > 0 OR act_ordinal = 0)",
        "CHECK ((((operations IS NULL) = (audit_order = 0)) \
         AND ((operations IS NULL) = (ordered_at IS NULL)) \
         AND ((audit_order > 0) OR (act_ordinal = 0))))",
    ),
    (
        "audit_activation_attested",
        "(transformation_name IN ('morpholog.activate', 'morpholog.baseline')) \
         = (attestation ->> 'mode' = 'migration')",
        "CHECK (((transformation_name = ANY (ARRAY['morpholog.activate'::text, \
         'morpholog.baseline'::text])) = ((attestation ->> 'mode'::text) = 'migration'::text)))",
    ),
];

/// The one shape an existing constraint may have besides its own: the
/// attestation check as a build before the baseline chunks wrote it. A
/// database prepared by that build has it replaced, not refused.
const SUPERSEDED: [(&str, &str); 1] = [(
    "audit_activation_attested",
    "CHECK (((transformation_name = 'morpholog.activate'::text) \
     = ((attestation ->> 'mode'::text) = 'migration'::text)))",
)];

/// The columns, as the catalog describes them once added.
const COLUMNS: [(&str, &str); 4] = [
    ("audit_order", "bigint NOT NULL DEFAULT 0"),
    ("act_ordinal", "integer NOT NULL DEFAULT 0"),
    ("ordered_at", "timestamp with time zone"),
    ("operations", "jsonb"),
];

/// The indexes, built `CONCURRENTLY`: two scans each, no lock a reader or
/// writer notices. A build that fails leaves an invalid index behind,
/// which `IF NOT EXISTS` would keep; it is dropped and rebuilt instead.
/// Beside each statement, the shape the catalog describes the index by:
/// table, uniqueness, method, keys and predicate.
const INDEXES: [(&str, &str, &str); 2] = [
    (
        "audit_one_activation",
        "CREATE UNIQUE INDEX CONCURRENTLY IF NOT EXISTS audit_one_activation \
         ON morpholog.audit (transformation_name) \
         WHERE transformation_name = 'morpholog.activate'",
        "ON morpholog.audit UNIQUE btree (transformation_name) \
         WHERE (transformation_name = 'morpholog.activate'::text)",
    ),
    (
        "audit_canonical",
        "CREATE INDEX CONCURRENTLY IF NOT EXISTS audit_canonical \
         ON morpholog.audit (audit_order, act_ordinal, committed_at, transition_id)",
        "ON morpholog.audit btree (audit_order, act_ordinal, committed_at, transition_id)",
    ),
];

const PROGRESS_SQL: &str = "CREATE TABLE IF NOT EXISTS morpholog.migration_progress (
    version    int         NOT NULL,
    step       text        NOT NULL,
    detail     jsonb       NOT NULL DEFAULT '{}'::jsonb,
    updated_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (version, step)
)";

/// Measured on 2026-10-30 (release build, laptop): preparation scans
/// about 0.6 us per audit row for its schema steps and 12 us per row for
/// the frontier; the cutover's legacy tail 11.5 us per row and its
/// baseline 10.4 us per live claim, the chunks' serialisation and leaf
/// hashing included.
const PREPARATION_NS_PER_ROW: i64 = 12_600;
const PAUSE_NS_PER_ROW: i64 = 11_500;
const PAUSE_NS_PER_CLAIM: i64 = 10_400;

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
    /// How many of them the saved frontier already holds.
    pub legacy_rows_prepared: i64,
    pub audit_bytes: i64,
    pub live_claims: i64,
    /// The bytes the baseline chunks will take as JSONB, an estimate of
    /// how much the evidence grows.
    pub baseline_bytes: i64,
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

async fn progress_table_exists(pool: &PgPool) -> Result<bool, PgError> {
    sqlx::query_scalar!(
        r#"SELECT to_regclass('morpholog.migration_progress') IS NOT NULL AS "present!""#
    )
    .fetch_one(pool)
    .await
    .map_err(classify_checked_query)
}

/// The steps recorded done for migration 27, in step order.
async fn steps_done(pool: &PgPool) -> Result<Vec<String>, PgError> {
    if !progress_table_exists(pool).await? {
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

async fn upsert_progress(
    pool: &PgPool,
    step: &str,
    detail: serde_json::Value,
) -> Result<(), PgError> {
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

/// An existing object under a name the migration owns must have the
/// shape the migration would give it; another shape is someone else's
/// object, refused rather than changed.
fn same_shape(object: &str, expected: &str, found: &str) -> Result<(), PgError> {
    if found == expected {
        Ok(())
    } else {
        Err(PgError::ObjectDiffers {
            object: object.to_string(),
            expected: expected.to_string(),
            found: found.to_string(),
        })
    }
}

async fn step_columns(pool: &PgPool, policy: &LockPolicy) -> Result<serde_json::Value, PgError> {
    let names: Vec<String> = COLUMNS
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect();
    // `shape!`: format_type is never null for a live column.
    let present = sqlx::query!(
        r#"SELECT a.attname AS "name!",
                  format_type(a.atttypid, a.atttypmod)
                      || CASE WHEN a.attnotnull THEN ' NOT NULL' ELSE '' END
                      || coalesce(' DEFAULT ' || pg_get_expr(d.adbin, d.adrelid), '') AS "shape!"
           FROM pg_attribute a
           LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
           WHERE a.attrelid = 'morpholog.audit'::regclass
             AND a.attname = ANY($1)
             AND NOT a.attisdropped"#,
        &names
    )
    .fetch_all(pool)
    .await
    .map_err(classify_checked_query)?;
    let mut missing = 0;
    for (name, expected) in COLUMNS {
        match present.iter().find(|c| c.name == name) {
            Some(column) => {
                same_shape(&format!("morpholog.audit.{name}"), expected, &column.shape)?;
            }
            None => missing += 1,
        }
    }
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
    let mut replaced = Vec::new();
    let mut validated = Vec::new();
    for (name, expression, expected) in CHECKS {
        // `shape!`: pg_get_constraintdef is never null for a constraint row.
        let state = sqlx::query!(
            r#"SELECT convalidated AS "validated!",
                      pg_get_constraintdef(oid) AS "shape!"
               FROM pg_constraint
               WHERE conrelid = 'morpholog.audit'::regclass AND conname = $1"#,
            name
        )
        .fetch_optional(pool)
        .await
        .map_err(classify_checked_query)?;
        let existing = match state {
            Some(row) => {
                let shape = row.shape.strip_suffix(" NOT VALID").unwrap_or(&row.shape);
                if SUPERSEDED.contains(&(name, shape)) {
                    let sql = format!("ALTER TABLE morpholog.audit DROP CONSTRAINT {name}");
                    with_brief_lock(pool, policy, &sql).await?;
                    replaced.push(name);
                    None
                } else {
                    same_shape(&format!("constraint {name}"), expected, shape)?;
                    Some(row.validated)
                }
            }
            None => None,
        };
        let valid = match existing {
            Some(validated) => validated,
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
    Ok(serde_json::json!({ "added": added, "replaced": replaced, "validated": validated }))
}

async fn step_indexes(pool: &PgPool) -> Result<serde_json::Value, PgError> {
    let mut built = Vec::new();
    let mut rebuilt = Vec::new();
    for (name, create, expected) in INDEXES {
        // `shape!`: every part is non-null for an index row (the predicate
        // is coalesced away).
        let state = sqlx::query!(
            r#"SELECT i.indisvalid AS "valid!",
                      'ON ' || tn.nspname || '.' || t.relname
                          || CASE WHEN i.indisunique THEN ' UNIQUE ' ELSE ' ' END
                          || am.amname || ' ('
                          || (SELECT string_agg(pg_get_indexdef(i.indexrelid, k, true), ', ' ORDER BY k)
                              FROM generate_series(1, i.indnatts) AS k)
                          || ')'
                          || coalesce(' WHERE ' || pg_get_expr(i.indpred, i.indrelid), '') AS "shape!"
               FROM pg_index i
               JOIN pg_class c ON c.oid = i.indexrelid
               JOIN pg_namespace n ON n.oid = c.relnamespace
               JOIN pg_am am ON am.oid = c.relam
               JOIN pg_class t ON t.oid = i.indrelid
               JOIN pg_namespace tn ON tn.oid = t.relnamespace
               WHERE n.nspname = 'morpholog' AND c.relname = $1"#,
            name
        )
        .fetch_optional(pool)
        .await
        .map_err(classify_checked_query)?;
        match state {
            Some(row) => {
                same_shape(&format!("index morpholog.{name}"), expected, &row.shape)?;
                if row.valid {
                    continue;
                }
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
            FRONTIER => step_frontier(pool).await?,
            _ => step_objects(pool).await?,
        };
        upsert_progress(pool, step, detail).await?;
    }
    Ok(())
}

/// Take the migration lock without waiting. The connection holds it
/// until closed; a second runner is refused.
pub(crate) async fn take_migration_lock(pool: &PgPool) -> Result<sqlx::PgConnection, PgError> {
    let mut conn = pool.acquire().await.map_err(classify)?.detach();
    // `taken!`: pg_try_advisory_lock is never null.
    let taken = sqlx::query_scalar!(
        r#"SELECT pg_try_advisory_lock($1) AS "taken!""#,
        MIGRATION_LOCK_KEY
    )
    .fetch_one(&mut conn)
    .await
    .map_err(classify_checked_query)?;
    if taken {
        Ok(conn)
    } else {
        Err(PgError::MigrationRunning)
    }
}

/// The saved frontier: the tree over the legacy rows up to `cursor`, as
/// `Frontier` keeps it, and the horizon of the pass that saved it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FrontierState {
    size: u64,
    subtrees: Vec<String>,
    cursor: Option<FrontierCursor>,
    horizon: Timestamp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct FrontierCursor {
    committed_at: Timestamp,
    transition_id: Uuid,
}

fn malformed_frontier(why: &str) -> PgError {
    PgError::InvalidState(format!(
        "the saved frontier of migration 27 is malformed ({why}); delete it \
         (`DELETE FROM morpholog.migration_progress WHERE version = 27 AND step = \
         'frontier_state'`) and run `morpholog migrate --prepare` again"
    ))
}

/// The saved frontier, if any. A malformed one is refused, never reset
/// or trusted. The progress table must exist.
async fn load_frontier<'e, E>(executor: E) -> Result<Option<(Frontier, FrontierState)>, PgError>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let detail = sqlx::query_scalar!(
        "SELECT detail FROM morpholog.migration_progress WHERE version = $1 AND step = $2",
        CANONICAL_ORDER_VERSION,
        FRONTIER_STATE
    )
    .fetch_optional(executor)
    .await
    .map_err(classify_checked_query)?;
    let Some(detail) = detail else {
        return Ok(None);
    };
    let state: FrontierState =
        serde_json::from_value(detail).map_err(|e| malformed_frontier(&e.to_string()))?;
    let size = usize::try_from(state.size).map_err(|_| malformed_frontier("size"))?;
    let subtrees = state
        .subtrees
        .iter()
        .map(|hex| {
            hex::decode(hex)
                .ok()
                .and_then(|bytes| Hash::try_from(bytes).ok())
                .ok_or_else(|| malformed_frontier("a subtree hash"))
        })
        .collect::<Result<Vec<Hash>, PgError>>()?;
    let frontier = Frontier::from_parts(size, subtrees)
        .ok_or_else(|| malformed_frontier("subtree count against size"))?;
    if state.cursor.is_some() != (size > 0) {
        return Err(malformed_frontier("cursor against size"));
    }
    Ok(Some((frontier, state)))
}

async fn save_frontier(
    pool: &PgPool,
    frontier: &Frontier,
    cursor: Option<FrontierCursor>,
    horizon: Timestamp,
) -> Result<(), PgError> {
    let state = FrontierState {
        size: frontier.len() as u64,
        subtrees: frontier.subtrees().iter().map(hex::encode).collect(),
        cursor,
        horizon,
    };
    upsert_progress(pool, FRONTIER_STATE, serde_json::to_value(&state)?).await
}

/// Where the frontier stands after a pass: what is saved, never what a
/// pass hashed and lost.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrontierProgress {
    pub legacy_rows_prepared: u64,
    pub legacy_root: String,
    pub rows_added: u64,
}

fn progress(frontier: &Frontier, rows_added: u64) -> FrontierProgress {
    FrontierProgress {
        legacy_rows_prepared: frontier.len() as u64,
        legacy_root: Digest::from_bytes(frontier.root()).to_string(),
        rows_added,
    }
}

/// One pass over the legacy rows below a fresh horizon, from the saved
/// cursor, saving the frontier after every page. Each page reads under
/// its own snapshot: below the horizon every row is final. The seam
/// stops a pass after hashing its n-th page and before saving it, as a
/// crash there would.
#[doc(hidden)]
pub async fn frontier_pass_with(
    pool: &PgPool,
    limit: i64,
    stop_before_saving_page: Option<usize>,
) -> Result<FrontierProgress, PgError> {
    let census = census(pool).await?;
    let refusals = census.refusals();
    if !refusals.is_empty() {
        return Err(PgError::CutoverRefused { reasons: refusals });
    }
    let (mut frontier, mut cursor) = match load_frontier(pool).await? {
        Some((frontier, state)) => (frontier, state.cursor),
        None => (Frontier::default(), None),
    };
    let mut saved = progress(&frontier, 0);
    let mut pages = 0;
    loop {
        let page = list_legacy_rows_page(
            pool,
            cursor.map(|c| (c.committed_at, c.transition_id)),
            census.horizon,
            limit,
        )
        .await?;
        pages += 1;
        for row in &page {
            frontier.push(audit_leaf_hash(row)?);
        }
        if stop_before_saving_page == Some(pages) {
            return Ok(saved);
        }
        if let Some(last) = page.last() {
            cursor = Some(FrontierCursor {
                committed_at: last.committed_at,
                transition_id: last.transition_id,
            });
        }
        save_frontier(pool, &frontier, cursor, census.horizon).await?;
        saved = progress(&frontier, saved.rows_added + page.len() as u64);
        if (page.len() as i64) < limit {
            return Ok(saved);
        }
    }
}

async fn step_frontier(pool: &PgPool) -> Result<serde_json::Value, PgError> {
    for _ in 0..MAX_PASSES {
        let pass = frontier_pass_with(pool, REPLAY_CHUNK, None).await?;
        if pass.rows_added < REPLAY_CHUNK as u64 {
            break;
        }
    }
    let (frontier, state) = load_frontier(pool)
        .await?
        .ok_or_else(|| malformed_frontier("absent after a pass"))?;
    Ok(serde_json::json!({
        "legacy_rows_prepared": frontier.len(),
        "horizon": state.horizon,
    }))
}

/// Every legacy row hashed as one tree from scratch: the ruler the
/// frontier is held to.
#[doc(hidden)]
pub async fn legacy_root_full_scan(pool: &PgPool) -> Result<(u64, String), PgError> {
    let mut leaves = Vec::new();
    let mut cursor = None;
    loop {
        let page = list_legacy_rows_page(pool, cursor, Timestamp::MAX, REPLAY_CHUNK).await?;
        for row in &page {
            leaves.push(audit_leaf_hash(row)?);
        }
        match page.last() {
            Some(last) if page.len() as i64 == REPLAY_CHUNK => {
                cursor = Some((last.committed_at, last.transition_id));
            }
            _ => break,
        }
    }
    Ok((
        leaves.len() as u64,
        Digest::from_bytes(merkle_root(&leaves)).to_string(),
    ))
}

/// The prepared frontier for the cutover, with the position the scan of
/// the remaining legacy rows continues from. The rows at or below the
/// cursor are counted against the frontier's size: a consistency check
/// that the legacy rows did not change under it, not tamper evidence,
/// which stays with checkpoints and verification.
pub(crate) async fn prepared_frontier(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<(Frontier, Option<AuditPosition>), PgError> {
    let Some((frontier, state)) = load_frontier(&mut **tx).await? else {
        return Ok((Frontier::default(), None));
    };
    let Some(cursor) = state.cursor else {
        return Ok((frontier, None));
    };
    // `below!`: count(*) is never null.
    let below = sqlx::query_scalar!(
        r#"SELECT count(*) AS "below!" FROM morpholog.audit
           WHERE audit_order = 0 AND act_ordinal = 0
             AND (committed_at, transition_id) <= ($1, $2)"#,
        cursor.committed_at.to_sqlx(),
        cursor.transition_id
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(classify_checked_query)?;
    if below != frontier.len() as i64 {
        return Err(PgError::CutoverRefused {
            reasons: vec![format!(
                "the prepared frontier holds {} legacy rows but {below} are at or below its \
                 cursor: the legacy rows changed under it. Delete it (`DELETE FROM \
                 morpholog.migration_progress WHERE version = 27 AND step = 'frontier_state'`) \
                 and run `morpholog migrate --prepare` again",
                frontier.len()
            )],
        });
    }
    Ok((
        frontier,
        Some(AuditPosition {
            audit_order: 0,
            act_ordinal: 0,
            committed_at: cursor.committed_at,
            transition_id: cursor.transition_id,
        }),
    ))
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
    let prepared = match progress_table_exists(pool).await? {
        true => load_frontier(pool)
            .await?
            .map_or(0, |(frontier, _)| frontier.len() as i64),
        false => 0,
    };
    let remaining = (audit.rows - prepared).max(0);
    let refusals = census(pool).await?.refusals();
    Ok(ActivationPreflight {
        state,
        steps_done,
        legacy_rows: audit.rows,
        legacy_rows_prepared: prepared,
        audit_bytes: audit.bytes,
        live_claims: claims.claims,
        baseline_bytes: claims.bytes,
        estimated_preparation_ms: audit.rows * PREPARATION_NS_PER_ROW / 1_000_000,
        estimated_writer_pause_ms: (remaining * PAUSE_NS_PER_ROW
            + claims.claims * PAUSE_NS_PER_CLAIM)
            / 1_000_000,
        refusals,
    })
}
