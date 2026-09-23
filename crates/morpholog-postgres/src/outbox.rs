use crate::error::{PgError, classify_checked_query};
use crate::propose::{PgProposalOutcome, propose_against_pg_inner};
use jiff::Timestamp;
use jiff_sqlx::ToSqlx;
use morpholog_core::{Definition, EvalValue, Invariant, Subject, Transformation, Transition};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;
/// The actor for transitions the runtime itself proposes, such as a
/// compensation after a non-retryable delivery failure (see
/// [`CompensationSpec`]). The audit row's attestation still records the
/// login role the worker connected as, so these commits carry real
/// lineage.
pub fn system_actor() -> Subject {
    Subject::from("morpholog-system")
}
/// The `morpholog.outbox.status` vocabulary, the same closed set as the
/// schema's CHECK constraint. Shared by decoding, the [`list_outbox_rows`]
/// filter and the CLI's `--status` flag, so they cannot drift. Serialises
/// as the exact database string (`compensation_in_progress`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboxStatus {
    Pending,
    InProgress,
    Delivered,
    Failed,
    CompensationInProgress,
    CompensationFailed,
}

impl OutboxStatus {
    /// The database string, identical to the serialised form.
    pub fn as_str(self) -> &'static str {
        match self {
            OutboxStatus::Pending => "pending",
            OutboxStatus::InProgress => "in_progress",
            OutboxStatus::Delivered => "delivered",
            OutboxStatus::Failed => "failed",
            OutboxStatus::CompensationInProgress => "compensation_in_progress",
            OutboxStatus::CompensationFailed => "compensation_failed",
        }
    }
}

impl std::str::FromStr for OutboxStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(OutboxStatus::Pending),
            "in_progress" => Ok(OutboxStatus::InProgress),
            "delivered" => Ok(OutboxStatus::Delivered),
            "failed" => Ok(OutboxStatus::Failed),
            "compensation_in_progress" => Ok(OutboxStatus::CompensationInProgress),
            "compensation_failed" => Ok(OutboxStatus::CompensationFailed),
            other => Err(format!("unknown outbox status `{other}`")),
        }
    }
}

impl std::fmt::Display for OutboxStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The verdict of a lease-gated update: exactly one row means the
/// lease still held; none means another worker took it over first.
fn lease_outcome(rows: &sqlx::postgres::PgQueryResult) -> OutboxUpdate {
    if rows.rows_affected() == 1 {
        OutboxUpdate::Applied
    } else {
        OutboxUpdate::LeaseLost
    }
}

/// One row of `morpholog.outbox` decoded into typed runtime values.
///
/// Every column of the table. The delivery-state columns are optional and
/// fill in as the row moves through delivery. A `pending` row with
/// `attempt_count > 0` and a `last_attempt_at` has failed transiently, not
/// just been enqueued.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OutboxRow {
    pub intent_id: Uuid,
    pub transition_id: Uuid,
    pub intent_type: String,
    pub arguments: Vec<EvalValue>,
    pub idempotency_key: String,
    pub status: OutboxStatus,
    pub attempt_count: i32,
    #[serde(with = "crate::wire_time")]
    pub enqueued_at: Timestamp,
    #[serde(with = "crate::wire_time::option")]
    pub last_attempt_at: Option<Timestamp>,
    #[serde(with = "crate::wire_time::option")]
    pub delivered_at: Option<Timestamp>,
    #[serde(with = "crate::wire_time::option")]
    pub failed_at: Option<Timestamp>,
    pub failure_reason: Option<String>,
    #[serde(with = "crate::wire_time::option")]
    pub next_attempt_at: Option<Timestamp>,
    pub compensation_transition_id: Option<Uuid>,
    pub locked_by: Option<String>,
    #[serde(with = "crate::wire_time::option")]
    pub lock_expires_at: Option<Timestamp>,
}
/// Outcome of a state-mutating helper on a leased outbox row.
///
/// A worker without the current lease (expired and taken over, or wrong
/// `worker_id`) cannot change the row. Losing a lease is normal, not an
/// error, so the caller gets [`OutboxUpdate::LeaseLost`].
#[doc(hidden)]
#[must_use = "an outbox update outcome must be inspected; `LeaseLost` means the requested state change did not apply"]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum OutboxUpdate {
    /// The row was updated as requested.
    Applied,
    /// The lease was no longer held by the supplied `worker_id`
    /// (expired, released, or never held). No change was made.
    LeaseLost,
}
/// Return outbox rows whose `status = 'pending'`, ordered by
/// `(enqueued_at, intent_id)`.
///
/// For other statuses or intent-type filtering, use [`list_outbox_rows`].
pub async fn list_pending_outbox(pool: &PgPool) -> Result<Vec<OutboxRow>, PgError> {
    // Its own query, not the nullable filter below: the literal predicate
    // lets the planner use the partial `outbox_pending` index.
    let rows = sqlx::query_as!(
        OutboxRowRaw,
        "SELECT intent_id, transition_id, intent_type, arguments,
                idempotency_key, status, attempt_count, enqueued_at,
                last_attempt_at, delivered_at, failed_at, failure_reason,
                next_attempt_at, compensation_transition_id, locked_by, lock_expires_at
         FROM morpholog.outbox
         WHERE status = 'pending'
         ORDER BY enqueued_at, intent_id",
    )
    .fetch_all(pool)
    .await
    .map_err(classify_checked_query)?;
    rows.into_iter().map(decode_outbox_row).collect()
}
/// Return outbox rows filtered by status and/or intent type. `None` drops
/// that filter (any status, including the compensation states; any intent
/// type). Ordered by `(enqueued_at, intent_id)`.
pub async fn list_outbox_rows(
    pool: &PgPool,
    status_filter: Option<OutboxStatus>,
    intent_type_filter: Option<&str>,
) -> Result<Vec<OutboxRow>, PgError> {
    let status_filter = status_filter.map(OutboxStatus::as_str);
    // One statement for every filter shape: a NULL parameter drops its
    // predicate. This read scans.
    let rows = sqlx::query_as!(
        OutboxRowRaw,
        r#"SELECT intent_id, transition_id, intent_type, arguments,
                idempotency_key, status, attempt_count, enqueued_at,
                last_attempt_at, delivered_at, failed_at, failure_reason,
                next_attempt_at, compensation_transition_id, locked_by, lock_expires_at
         FROM morpholog.outbox
         WHERE ($1::text IS NULL OR status = $1)
           AND ($2::text IS NULL OR intent_type = $2)
         ORDER BY enqueued_at, intent_id"#,
        status_filter,
        intent_type_filter,
    )
    .fetch_all(pool)
    .await
    .map_err(classify_checked_query)?;
    rows.into_iter().map(decode_outbox_row).collect()
}
/// One raw `morpholog.outbox` row as `query_as!` decodes it; see
/// [`decode_outbox_row`].
pub(crate) struct OutboxRowRaw {
    intent_id: Uuid,
    transition_id: Uuid,
    intent_type: String,
    arguments: serde_json::Value,
    idempotency_key: String,
    status: String,
    attempt_count: i32,
    enqueued_at: Timestamp,
    last_attempt_at: Option<jiff_sqlx::Timestamp>,
    delivered_at: Option<jiff_sqlx::Timestamp>,
    failed_at: Option<jiff_sqlx::Timestamp>,
    failure_reason: Option<String>,
    next_attempt_at: Option<jiff_sqlx::Timestamp>,
    compensation_transition_id: Option<Uuid>,
    locked_by: Option<String>,
    lock_expires_at: Option<jiff_sqlx::Timestamp>,
}
pub(crate) fn decode_outbox_row(row: OutboxRowRaw) -> Result<OutboxRow, PgError> {
    Ok(OutboxRow {
        intent_id: row.intent_id,
        transition_id: row.transition_id,
        intent_type: row.intent_type,
        arguments: serde_json::from_value(row.arguments)?,
        idempotency_key: row.idempotency_key,
        // The CHECK constraint makes an unknown string unreachable; a
        // decode failure here means the schema and this enum drifted.
        status: row.status.parse().map_err(PgError::InvalidState)?,
        attempt_count: row.attempt_count,
        enqueued_at: row.enqueued_at,
        last_attempt_at: row.last_attempt_at.map(Into::into),
        delivered_at: row.delivered_at.map(Into::into),
        failed_at: row.failed_at.map(Into::into),
        failure_reason: row.failure_reason,
        next_attempt_at: row.next_attempt_at.map(Into::into),
        compensation_transition_id: row.compensation_transition_id,
        locked_by: row.locked_by,
        lock_expires_at: row.lock_expires_at.map(Into::into),
    })
}
// ===========================================================================
// Outbox delivery-state mutators
// ===========================================================================
//
// Every `mark_*` helper requires the worker to hold a valid lease
// (`locked_by = worker_id AND lock_expires_at > now()`) and returns
// `OutboxUpdate::LeaseLost` otherwise. `record_compensation` errors
// instead: recording against a non-failed or already-compensated row is a
// bug, not an operational condition.
/// Mark a successfully-delivered outbox row.
///
/// Returns `Applied`, or `LeaseLost` if the worker no longer holds the
/// lease.
///
/// Internal substrate of [`process_one_outbox_row`]; use that unless
/// driving the state machine manually.
#[doc(hidden)]
pub async fn mark_outbox_delivered(
    pool: &PgPool,
    intent_id: Uuid,
    worker_id: &str,
) -> Result<OutboxUpdate, PgError> {
    let rows = sqlx::query!(
        "UPDATE morpholog.outbox
         SET status='delivered',
             delivered_at=now(),
             attempt_count=attempt_count+1,
             locked_by=NULL,
             lock_expires_at=NULL
         WHERE intent_id=$1
           AND locked_by=$2
           AND lock_expires_at > now()",
        intent_id,
        worker_id,
    )
    .execute(pool)
    .await
    .map_err(classify_checked_query)?;
    Ok(lease_outcome(&rows))
}
/// Record a transient delivery failure: release the lease and return the
/// row to `pending`, invisible to claims until `next_attempt_at`.
///
/// A past `next_attempt_at` is accepted: a slow delivery's retry instant
/// can elapse in transit. [`claim_pending_outbox_row`]'s `claim_before`
/// bound is what stops an immediate re-claim.
///
/// Internal substrate of [`process_one_outbox_row`]; use that unless
/// driving the state machine manually.
#[doc(hidden)]
pub async fn mark_outbox_transient_attempt(
    pool: &PgPool,
    intent_id: Uuid,
    worker_id: &str,
    next_attempt_at: Timestamp,
) -> Result<OutboxUpdate, PgError> {
    let rows = sqlx::query!(
        "UPDATE morpholog.outbox
         SET status='pending',
             attempt_count=attempt_count+1,
             last_attempt_at=now(),
             next_attempt_at=$3,
             locked_by=NULL,
             lock_expires_at=NULL
         WHERE intent_id=$1
           AND locked_by=$2
           AND lock_expires_at > now()",
        intent_id,
        worker_id,
        next_attempt_at.to_sqlx(),
    )
    .execute(pool)
    .await
    .map_err(classify_checked_query)?;
    Ok(lease_outcome(&rows))
}
/// Mark a non-retryable delivery failure and release the lease. A
/// compensation can then be recorded via [`record_compensation`].
///
/// Internal substrate of [`process_one_outbox_row`]; use that unless
/// driving the state machine manually.
#[doc(hidden)]
pub async fn mark_outbox_failed(
    pool: &PgPool,
    intent_id: Uuid,
    worker_id: &str,
    reason: &str,
) -> Result<OutboxUpdate, PgError> {
    let rows = sqlx::query!(
        "UPDATE morpholog.outbox
         SET status='failed',
             failed_at=now(),
             failure_reason=$3,
             attempt_count=attempt_count+1,
             last_attempt_at=now(),
             locked_by=NULL,
             lock_expires_at=NULL
         WHERE intent_id=$1
           AND locked_by=$2
           AND lock_expires_at > now()",
        intent_id,
        worker_id,
        reason,
    )
    .execute(pool)
    .await
    .map_err(classify_checked_query)?;
    Ok(lease_outcome(&rows))
}
/// Link a compensating transformation to a failed outbox row.
///
/// The row must be `failed` and not yet carry a
/// `compensation_transition_id`; otherwise this is a bug and returns
/// [`PgError::InvalidState`]. The id must reference a row in
/// `morpholog.audit` (foreign key). No lease is needed:
/// [`mark_outbox_failed`] already released it.
///
/// **This records lineage; it does not prevent duplicates.** It stops a
/// second record from overwriting the first, not a second compensation
/// from committing. Two workers racing one `failed` row can both commit
/// compensations; only the second record fails. To prevent that, keep the
/// lease across failed -> commit -> record, or guard the compensating
/// transformation with an `original_intent_id` invariant. See
/// `docs/outbox-sketch.md`.
///
/// Internal substrate of [`process_one_outbox_row`]; use that unless
/// driving the state machine manually.
#[doc(hidden)]
pub async fn record_compensation(
    pool: &PgPool,
    intent_id: Uuid,
    compensation_transition_id: Uuid,
) -> Result<(), PgError> {
    let rows = sqlx::query!(
        "UPDATE morpholog.outbox
         SET compensation_transition_id=$2
         WHERE intent_id=$1
           AND status='failed'
           AND compensation_transition_id IS NULL",
        intent_id,
        compensation_transition_id,
    )
    .execute(pool)
    .await
    .map_err(classify_checked_query)?;
    if rows.rows_affected() == 1 {
        Ok(())
    } else {
        Err(PgError::InvalidState(format!(
            "record_compensation({intent_id}): 0 rows matched. The outbox \
             row was either not found, not in status='failed', or already \
             carries a compensation_transition_id."
        )))
    }
}
/// Atomically claim one due-pending (or expired-leased) outbox row
/// of the given `intent_type` for delivery by `worker_id`.
///
/// `FOR UPDATE SKIP LOCKED` inside one `UPDATE ... RETURNING`, so two
/// concurrent workers never claim the same row; the loser takes the next
/// candidate or gets `None`.
///
/// Eligible rows:
/// - `pending` with `next_attempt_at` unset or `<= claim_before`;
/// - `in_progress` with an expired lease (a crashed worker's row).
///
/// A drain loop passes the same `claim_before` (captured once per pass)
/// on every iteration, so rows deferred during the pass wait for the next
/// one. Otherwise a sub-second retry could be re-claimed forever and the
/// worker would never sleep or see shutdown. One-shot callers pass
/// `Timestamp::now()`. Expired leases are judged by live `now()`.
///
/// `lease_duration` is how long the worker alone may change the row via
/// the `mark_*` helpers: long enough to cover delivery, short enough that
/// a crashed worker's rows come back soon.
///
/// The lease lives in the `locked_by` / `lock_expires_at` columns, not a
/// held row lock, so the deliverer must run **outside** any transaction.
///
/// Internal substrate of [`process_one_outbox_row`]; use that unless
/// driving the state machine manually.
#[doc(hidden)]
pub async fn claim_pending_outbox_row(
    pool: &PgPool,
    worker_id: &str,
    intent_type: &str,
    lease_duration: std::time::Duration,
    claim_before: Timestamp,
) -> Result<Option<OutboxRow>, PgError> {
    let lease_secs = lease_duration_to_secs(lease_duration)?;
    let row_opt = sqlx::query_as!(
        OutboxRowRaw,
        "UPDATE morpholog.outbox
         SET status='in_progress',
             locked_by=$1,
             lock_expires_at=now() + ($2::bigint * interval '1 second')
         WHERE intent_id = (
             SELECT intent_id
             FROM morpholog.outbox
             WHERE intent_type=$3
               AND (
                   (status='pending'
                    AND (next_attempt_at IS NULL OR next_attempt_at <= $4))
                OR (status='in_progress'
                    AND lock_expires_at < now())
               )
             ORDER BY enqueued_at, intent_id
             LIMIT 1
             FOR UPDATE SKIP LOCKED
         )
         RETURNING intent_id, transition_id, intent_type, arguments,
                   idempotency_key, status, attempt_count, enqueued_at,
                   last_attempt_at, delivered_at, failed_at, failure_reason,
                   next_attempt_at, compensation_transition_id, locked_by,
                   lock_expires_at",
        worker_id,
        lease_secs,
        intent_type,
        claim_before.to_sqlx(),
    )
    .fetch_optional(pool)
    .await
    .map_err(classify_checked_query)?;
    row_opt.map(decode_outbox_row).transpose()
}
/// Release a held lease and return the row to `pending`, so another
/// worker can claim it at once instead of waiting for the lease to expire.
/// For graceful shutdown. Returns `LeaseLost` if the lease already
/// expired.
#[doc(hidden)]
pub async fn release_outbox_claim(
    pool: &PgPool,
    intent_id: Uuid,
    worker_id: &str,
) -> Result<OutboxUpdate, PgError> {
    let rows = sqlx::query!(
        "UPDATE morpholog.outbox
         SET status='pending',
             locked_by=NULL,
             lock_expires_at=NULL
         WHERE intent_id=$1
           AND locked_by=$2
           AND lock_expires_at > now()",
        intent_id,
        worker_id,
    )
    .execute(pool)
    .await
    .map_err(classify_checked_query)?;
    Ok(lease_outcome(&rows))
}
/// Soonest future `next_attempt_at` over pending rows of the given
/// `intent_type`. Returns `None` if no such row exists.
///
/// A polling worker uses this after an empty drain to wake when the next
/// retry is due, rather than always sleeping the full interval. Only
/// future instants count: a due row would have been claimed by that
/// drain.
pub async fn earliest_pending_retry(
    pool: &PgPool,
    intent_type: &str,
) -> Result<Option<Timestamp>, PgError> {
    let row = sqlx::query!(
        "SELECT min(next_attempt_at) AS earliest
         FROM morpholog.outbox
         WHERE status='pending'
           AND intent_type=$1
           AND next_attempt_at IS NOT NULL
           AND next_attempt_at > now()",
        intent_type,
    )
    .fetch_optional(pool)
    .await
    .map_err(classify_checked_query)?;
    Ok(row.and_then(|r| r.earliest.map(Into::into)))
}
pub(crate) fn lease_duration_to_secs(lease_duration: std::time::Duration) -> Result<i64, PgError> {
    let lease_secs: i64 = lease_duration
        .as_secs()
        .try_into()
        .map_err(|_| PgError::InvalidState("lease_duration too large for i64".to_string()))?;
    if lease_secs < 1 {
        return Err(PgError::InvalidState(format!(
            "lease_duration must be at least 1 second (got {lease_duration:?}); \
             a sub-second lease would expire before the claiming worker could \
             call any mark_* / complete_* helper, leaving the row effectively \
             un-updatable"
        )));
    }
    Ok(lease_secs)
}
/// Atomically claim the right to run a compensating transformation
/// for a previously-failed outbox row.
///
/// Only a `failed` row with no `compensation_transition_id` is eligible;
/// it moves to `compensation_in_progress` under a lease. The worker then
/// proposes the compensation and resolves the row with
/// [`complete_compensation`] (on `Committed`) or
/// [`mark_compensation_failed`] (on `Rejected`).
///
/// `FOR UPDATE SKIP LOCKED` lets at most one worker hold the lease.
/// Returns `Ok(None)` when the row is missing, not `failed`, already
/// compensated, or being claimed by another worker.
///
/// **An expired compensation lease is NOT reclaimed** (unlike
/// [`claim_pending_outbox_row`]). A worker may have crashed after the
/// compensation committed but before `complete_compensation`, so reclaim
/// could compensate twice; a stuck row needs an operator instead. For full
/// immunity, guard the compensating transformation with a
/// `CompensationApplied(original_intent_id)` invariant. See
/// `docs/outbox-sketch.md`.
///
/// Internal substrate of [`process_one_outbox_row`]'s compensation
/// arm; use that unless driving the state machine manually.
#[doc(hidden)]
pub async fn begin_compensation(
    pool: &PgPool,
    intent_id: Uuid,
    worker_id: &str,
    lease_duration: std::time::Duration,
) -> Result<Option<OutboxRow>, PgError> {
    let lease_secs = lease_duration_to_secs(lease_duration)?;
    let row_opt = sqlx::query_as!(
        OutboxRowRaw,
        "UPDATE morpholog.outbox
         SET status='compensation_in_progress',
             locked_by=$1,
             lock_expires_at=now() + ($2::bigint * interval '1 second')
         WHERE intent_id = (
             SELECT intent_id
             FROM morpholog.outbox
             WHERE intent_id=$3
               AND status='failed'
               AND compensation_transition_id IS NULL
             FOR UPDATE SKIP LOCKED
         )
         RETURNING intent_id, transition_id, intent_type, arguments,
                   idempotency_key, status, attempt_count, enqueued_at,
                   last_attempt_at, delivered_at, failed_at, failure_reason,
                   next_attempt_at, compensation_transition_id, locked_by,
                   lock_expires_at",
        worker_id,
        lease_secs,
        intent_id,
    )
    .fetch_optional(pool)
    .await
    .map_err(classify_checked_query)?;
    row_opt.map(decode_outbox_row).transpose()
}
/// Resolve a `compensation_in_progress` row on success: back to `failed`
/// with `compensation_transition_id` recorded, lease released.
///
/// Returns `OutboxUpdate::LeaseLost` if the worker lost the lease.
/// `compensation_transition_id` must reference a row in `morpholog.audit`
/// (foreign key).
///
/// Internal substrate of [`process_one_outbox_row`]'s compensation
/// arm; use that unless driving the state machine manually.
#[doc(hidden)]
pub async fn complete_compensation(
    pool: &PgPool,
    intent_id: Uuid,
    worker_id: &str,
    compensation_transition_id: Uuid,
) -> Result<OutboxUpdate, PgError> {
    let rows = sqlx::query!(
        "UPDATE morpholog.outbox
         SET status='failed',
             compensation_transition_id=$3,
             locked_by=NULL,
             lock_expires_at=NULL
         WHERE intent_id=$1
           AND status='compensation_in_progress'
           AND locked_by=$2
           AND lock_expires_at > now()",
        intent_id,
        worker_id,
        compensation_transition_id,
    )
    .execute(pool)
    .await
    .map_err(classify_checked_query)?;
    Ok(lease_outcome(&rows))
}
/// Resolve a `compensation_in_progress` row whose compensation was
/// rejected: it moves to `compensation_failed`, lease released, and stays
/// there until an operator steps in.
///
/// Returns `OutboxUpdate::LeaseLost` if the worker lost the lease.
///
/// `reason` **overwrites** the original `failure_reason`, and no audit row
/// keeps it. Callers needing both must save the original first.
///
/// Internal substrate of [`process_one_outbox_row`]'s compensation
/// arm; use that unless driving the state machine manually.
#[doc(hidden)]
pub async fn mark_compensation_failed(
    pool: &PgPool,
    intent_id: Uuid,
    worker_id: &str,
    reason: &str,
) -> Result<OutboxUpdate, PgError> {
    let rows = sqlx::query!(
        "UPDATE morpholog.outbox
         SET status='compensation_failed',
             failure_reason=$3,
             locked_by=NULL,
             lock_expires_at=NULL
         WHERE intent_id=$1
           AND status='compensation_in_progress'
           AND locked_by=$2
           AND lock_expires_at > now()",
        intent_id,
        worker_id,
        reason,
    )
    .execute(pool)
    .await
    .map_err(classify_checked_query)?;
    Ok(lease_outcome(&rows))
}
/// Outcome a [`Deliverer`] returns from a single delivery attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// Delivery succeeded; the row becomes `delivered`.
    Delivered,
    /// Retry no sooner than `next_attempt_at`. The row returns to
    /// `pending`. The deliverer owns the backoff policy; the processor
    /// stores whatever instant it chose.
    Transient { next_attempt_at: Timestamp },
    /// Do not retry (for example, the recipient does not exist). The row
    /// becomes `failed` with `reason`; with a [`CompensationSpec`], the
    /// processor then tries to run the compensation.
    NonRetryable { reason: String },
}
/// A delivery target: pushes one enqueued intent to the outside world.
/// It gets the full [`OutboxRow`], including the idempotency key.
///
/// `deliver` MUST NOT write to morpholog tables: the processor owns the
/// row's state; the deliverer owns only the external effect.
///
/// The future is `Send` so loops can `tokio::spawn` it for any
/// `D: Deliverer`; callers cannot add that bound later, so it is fixed
/// here.
pub trait Deliverer: Send + Sync {
    fn deliver(&self, row: &OutboxRow)
    -> impl std::future::Future<Output = DeliveryOutcome> + Send;
}
/// Maps the failed outbox row to the compensating transformation's
/// arguments. Boxed so callers can pass `None` for the
/// `Option<&CompensationSpec>` without a type annotation.
pub type CompensationArgsFromRow = Box<dyn Fn(&OutboxRow) -> Vec<EvalValue> + Send + Sync>;
/// What to run when delivery returns `NonRetryable`.
///
/// `args_from_row` runs after [`begin_compensation`] claims the lease, so
/// the row carries the new `failure_reason`.
///
/// The compensation is an ordinary proposal: every invariant, its own
/// audit row and outbox intents. The audit log keeps the full lineage.
pub struct CompensationSpec {
    pub transformation: Transformation,
    pub invariants: Vec<Invariant>,
    /// The programme's definitions; empty when it declares none.
    pub definitions: Vec<Definition>,
    pub args_from_row: CompensationArgsFromRow,
}
/// Outcome of one [`process_one_outbox_row`] cycle: which branch ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessOutcome {
    /// No row of this `intent_type` was due; nothing happened.
    NoRowAvailable,
    /// Delivery succeeded; the row is now `delivered`.
    Delivered { intent_id: Uuid },
    /// Delivery returned `Transient`; the row is back to `pending`
    /// with the supplied retry instant.
    TransientRetry {
        intent_id: Uuid,
        next_attempt_at: Timestamp,
    },
    /// Delivery returned `NonRetryable` and no [`CompensationSpec`]
    /// was supplied; the row is `failed`.
    Failed { intent_id: Uuid, reason: String },
    /// Delivery returned `NonRetryable` and a compensation is configured,
    /// but another worker holds its lease or it already ran. Nothing was
    /// proposed.
    CompensationDeferred { intent_id: Uuid },
    /// Compensation ran and committed. The row is back to `failed`
    /// with `compensation_transition_id` pointing at the
    /// compensation's audit row.
    Compensated {
        intent_id: Uuid,
        compensation_transition_id: Uuid,
    },
    /// The compensation was rejected. The row is `compensation_failed`
    /// and needs an operator.
    CompensationFailed { intent_id: Uuid, reason: String },
    /// The work finished, but the lease had expired and another worker
    /// took the row first. Not an error; log it and move on. If it happens
    /// during compensation, the compensation committed but the row never
    /// points at it: reconcile from the audit log.
    LeaseLost { intent_id: Uuid },
}
/// Drive one outbox row through delivery and, if needed, compensation.
/// A worker calls this in a loop; the loop owns scheduling.
///
/// The cycle:
/// 1. Claim a due row of the requested `intent_type` via
///    [`claim_pending_outbox_row`]. If none is claimable, return
///    [`ProcessOutcome::NoRowAvailable`].
/// 2. Invoke `deliverer.deliver(&row).await`.
/// 3. Route the [`DeliveryOutcome`]:
///    - `Delivered` -> [`mark_outbox_delivered`].
///    - `Transient` -> [`mark_outbox_transient_attempt`].
///    - `NonRetryable` -> [`mark_outbox_failed`], then if a
///      [`CompensationSpec`] is supplied, attempt
///      [`begin_compensation`] + invoke the compensating
///      transformation via [`crate::propose_against_pg`] + resolve via
///      [`complete_compensation`] or [`mark_compensation_failed`].
///
/// Safe across processes: both claims use `FOR UPDATE SKIP LOCKED`, so at
/// most one worker delivers a row or compensates it. A crash between the
/// compensation's commit and `complete_compensation` leaves the row stuck
/// for an operator rather than risk a duplicate (see
/// [`begin_compensation`]).
///
/// `claim_before`: see [`claim_pending_outbox_row`].
#[allow(clippy::too_many_arguments)]
pub async fn process_one_outbox_row<D>(
    pool: &PgPool,
    worker_id: &str,
    intent_type: &str,
    lease_duration: std::time::Duration,
    deliverer: &D,
    compensation: Option<&CompensationSpec>,
    claim_before: Timestamp,
) -> Result<ProcessOutcome, PgError>
where
    D: Deliverer,
{
    let Some(row) =
        claim_pending_outbox_row(pool, worker_id, intent_type, lease_duration, claim_before)
            .await?
    else {
        return Ok(ProcessOutcome::NoRowAvailable);
    };
    let intent_id = row.intent_id;
    match deliverer.deliver(&row).await {
        DeliveryOutcome::Delivered => {
            match mark_outbox_delivered(pool, intent_id, worker_id).await? {
                OutboxUpdate::Applied => Ok(ProcessOutcome::Delivered { intent_id }),
                OutboxUpdate::LeaseLost => Ok(ProcessOutcome::LeaseLost { intent_id }),
            }
        }
        DeliveryOutcome::Transient { next_attempt_at } => {
            match mark_outbox_transient_attempt(pool, intent_id, worker_id, next_attempt_at).await?
            {
                OutboxUpdate::Applied => Ok(ProcessOutcome::TransientRetry {
                    intent_id,
                    next_attempt_at,
                }),
                OutboxUpdate::LeaseLost => Ok(ProcessOutcome::LeaseLost { intent_id }),
            }
        }
        DeliveryOutcome::NonRetryable { reason } => {
            match mark_outbox_failed(pool, intent_id, worker_id, &reason).await? {
                OutboxUpdate::LeaseLost => {
                    // Not ours any more, and not moved to 'failed', so
                    // compensation must not run.
                    return Ok(ProcessOutcome::LeaseLost { intent_id });
                }
                OutboxUpdate::Applied => {}
            }
            let Some(spec) = compensation else {
                return Ok(ProcessOutcome::Failed { intent_id, reason });
            };
            // mark_outbox_failed released the lease; claim it again for
            // compensation. At most one worker wins.
            let claimed = begin_compensation(pool, intent_id, worker_id, lease_duration).await?;
            let Some(failed_row) = claimed else {
                return Ok(ProcessOutcome::CompensationDeferred { intent_id });
            };
            let args = (spec.args_from_row)(&failed_row);
            let compensation_transition = Transition {
                transformation_name: spec.transformation.name.clone(),
                args,
                actor: system_actor(),
            };
            let outcome = propose_against_pg_inner(
                pool,
                &spec.transformation,
                &compensation_transition,
                &spec.invariants,
                &spec.definitions,
            )
            .await?;
            match outcome {
                PgProposalOutcome::Committed { transition_id, .. } => {
                    match complete_compensation(pool, intent_id, worker_id, transition_id).await? {
                        OutboxUpdate::Applied => Ok(ProcessOutcome::Compensated {
                            intent_id,
                            compensation_transition_id: transition_id,
                        }),
                        OutboxUpdate::LeaseLost => Ok(ProcessOutcome::LeaseLost { intent_id }),
                    }
                }
                PgProposalOutcome::Rejected { reason, .. } => {
                    match mark_compensation_failed(pool, intent_id, worker_id, &reason).await? {
                        OutboxUpdate::Applied => {
                            Ok(ProcessOutcome::CompensationFailed { intent_id, reason })
                        }
                        OutboxUpdate::LeaseLost => Ok(ProcessOutcome::LeaseLost { intent_id }),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod status_tests {
    use super::OutboxStatus;

    /// Every variant round-trips through its database string, and serde
    /// emits exactly that string.
    #[test]
    fn status_vocabulary_round_trips_and_serialises_as_the_db_string() {
        let all = [
            OutboxStatus::Pending,
            OutboxStatus::InProgress,
            OutboxStatus::Delivered,
            OutboxStatus::Failed,
            OutboxStatus::CompensationInProgress,
            OutboxStatus::CompensationFailed,
        ];
        for status in all {
            assert_eq!(status.as_str().parse::<OutboxStatus>(), Ok(status));
            assert_eq!(
                serde_json::to_value(status).unwrap(),
                serde_json::Value::String(status.as_str().to_string())
            );
        }
        assert!("bogus".parse::<OutboxStatus>().is_err());
    }
}
