use crate::error::{PgError, classify};
use crate::propose::{PgProposalOutcome, propose_against_pg_inner};
use chrono::{DateTime, Utc};
use morpholog_core::{Definition, EvalValue, Invariant, Subject, Transformation, Transition};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;
/// Sentinel actor for transitions the runtime itself initiates, with
/// no user under whose authority the transition is being proposed.
/// Used by the outbox compensation path: when a delivery fails
/// non-retryably and a [`CompensationSpec`] is configured, the
/// compensating transformation is proposed by the runtime, not by
/// the actor of the original commit. The sentinel names the runtime
/// as the actor; the audit row's attestation records which
/// PostgreSQL-authenticated role the proposing worker connected as,
/// so runtime-initiated commits carry real lineage, not just the
/// sentinel.
pub fn system_actor() -> Subject {
    Subject::from("morpholog-system")
}
/// One row of `morpholog.outbox` decoded into typed runtime values.
///
/// Carries every column on the table. The delivery-state extensions are
/// nullable in the schema and `Option<T>` here; they fill in as a row
/// moves through the delivery state machine. A `pending` row with
/// `attempt_count > 0` and a non-NULL `last_attempt_at` is one a worker
/// has tried and failed transiently, not a fresh enqueue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OutboxRow {
    pub intent_id: Uuid,
    pub transition_id: Uuid,
    pub intent_type: String,
    pub arguments: Vec<EvalValue>,
    pub idempotency_key: String,
    pub status: String,
    pub attempt_count: i32,
    pub enqueued_at: DateTime<Utc>,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub failed_at: Option<DateTime<Utc>>,
    pub failure_reason: Option<String>,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub compensation_transition_id: Option<Uuid>,
    pub locked_by: Option<String>,
    pub lock_expires_at: Option<DateTime<Utc>>,
}
/// Outcome of a state-mutating helper on a leased outbox row.
///
/// A worker that does not hold the current lease (expired and taken
/// over, or wrong `worker_id`) cannot clobber the row's state. Lease
/// loss is a normal operational condition, not an error, so the caller
/// sees [`OutboxUpdate::LeaseLost`] and can log, retry-after-reclaim,
/// or move on.
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
/// `(enqueued_at, intent_id)`. The "what does the worker have to
/// deliver?" query.
///
/// For other statuses or intent-type filtering, use [`list_outbox_rows`].
pub async fn list_pending_outbox(pool: &PgPool) -> Result<Vec<OutboxRow>, PgError> {
    let rows = sqlx::query_as!(
        OutboxRowRaw,
        "SELECT intent_id, transition_id, intent_type, arguments,
                idempotency_key, status, attempt_count, enqueued_at,
                last_attempt_at, delivered_at, failed_at, failure_reason,
                next_attempt_at, compensation_transition_id, locked_by, lock_expires_at
         FROM morpholog.outbox
         WHERE status = $1
         ORDER BY enqueued_at, intent_id",
        "pending",
    )
    .fetch_all(pool)
    .await
    .map_err(classify)?;
    rows.into_iter().map(decode_outbox_row).collect()
}
/// Return outbox rows filtered by status and/or intent type. Both
/// filters are optional: `None` drops that predicate entirely (any
/// status, including the worker's internal compensation states; any
/// intent type). Order matches [`list_pending_outbox`].
///
/// Lets a reader ask "what failed?" or "what is in flight?" without
/// custom SQL; used by `morpholog inspect outbox` for non-pending rows.
pub async fn list_outbox_rows(
    pool: &PgPool,
    status_filter: Option<&str>,
    intent_type_filter: Option<&str>,
) -> Result<Vec<OutboxRow>, PgError> {
    // The macro needs one literal statement per filter shape, so each
    // filter combination is a distinct query.
    let rows = match (status_filter, intent_type_filter) {
        (Some(status), Some(intent_type)) => sqlx::query_as!(
            OutboxRowRaw,
            "SELECT intent_id, transition_id, intent_type, arguments,
                    idempotency_key, status, attempt_count, enqueued_at,
                    last_attempt_at, delivered_at, failed_at, failure_reason,
                    next_attempt_at, compensation_transition_id, locked_by, lock_expires_at
             FROM morpholog.outbox
             WHERE status = $1 AND intent_type = $2
             ORDER BY enqueued_at, intent_id",
            status,
            intent_type,
        )
        .fetch_all(pool)
        .await
        .map_err(classify)?,
        (Some(status), None) => sqlx::query_as!(
            OutboxRowRaw,
            "SELECT intent_id, transition_id, intent_type, arguments,
                    idempotency_key, status, attempt_count, enqueued_at,
                    last_attempt_at, delivered_at, failed_at, failure_reason,
                    next_attempt_at, compensation_transition_id, locked_by, lock_expires_at
             FROM morpholog.outbox
             WHERE status = $1
             ORDER BY enqueued_at, intent_id",
            status,
        )
        .fetch_all(pool)
        .await
        .map_err(classify)?,
        (None, Some(intent_type)) => sqlx::query_as!(
            OutboxRowRaw,
            "SELECT intent_id, transition_id, intent_type, arguments,
                    idempotency_key, status, attempt_count, enqueued_at,
                    last_attempt_at, delivered_at, failed_at, failure_reason,
                    next_attempt_at, compensation_transition_id, locked_by, lock_expires_at
             FROM morpholog.outbox
             WHERE intent_type = $1
             ORDER BY enqueued_at, intent_id",
            intent_type,
        )
        .fetch_all(pool)
        .await
        .map_err(classify)?,
        (None, None) => sqlx::query_as!(
            OutboxRowRaw,
            "SELECT intent_id, transition_id, intent_type, arguments,
                    idempotency_key, status, attempt_count, enqueued_at,
                    last_attempt_at, delivered_at, failed_at, failure_reason,
                    next_attempt_at, compensation_transition_id, locked_by, lock_expires_at
             FROM morpholog.outbox
             ORDER BY enqueued_at, intent_id",
        )
        .fetch_all(pool)
        .await
        .map_err(classify)?,
    };
    rows.into_iter().map(decode_outbox_row).collect()
}
/// One raw `morpholog.outbox` row as `query_as!` decodes it (DB shape
/// only); turned into a typed [`OutboxRow`] by [`decode_outbox_row`].
/// Field order matches the SELECT column order in the queries below.
pub(crate) struct OutboxRowRaw {
    intent_id: Uuid,
    transition_id: Uuid,
    intent_type: String,
    arguments: serde_json::Value,
    idempotency_key: String,
    status: String,
    attempt_count: i32,
    enqueued_at: DateTime<Utc>,
    last_attempt_at: Option<DateTime<Utc>>,
    delivered_at: Option<DateTime<Utc>>,
    failed_at: Option<DateTime<Utc>>,
    failure_reason: Option<String>,
    next_attempt_at: Option<DateTime<Utc>>,
    compensation_transition_id: Option<Uuid>,
    locked_by: Option<String>,
    lock_expires_at: Option<DateTime<Utc>>,
}
pub(crate) fn decode_outbox_row(row: OutboxRowRaw) -> Result<OutboxRow, PgError> {
    Ok(OutboxRow {
        intent_id: row.intent_id,
        transition_id: row.transition_id,
        intent_type: row.intent_type,
        arguments: serde_json::from_value(row.arguments)?,
        idempotency_key: row.idempotency_key,
        status: row.status,
        attempt_count: row.attempt_count,
        enqueued_at: row.enqueued_at,
        last_attempt_at: row.last_attempt_at,
        delivered_at: row.delivered_at,
        failed_at: row.failed_at,
        failure_reason: row.failure_reason,
        next_attempt_at: row.next_attempt_at,
        compensation_transition_id: row.compensation_transition_id,
        locked_by: row.locked_by,
        lock_expires_at: row.lock_expires_at,
    })
}
// ===========================================================================
// Outbox delivery-state mutators
// ===========================================================================
//
// Helpers that move an outbox row through the delivery state machine.
// All `mark_*` helpers gate on the worker holding a valid lease
// (`locked_by = worker_id AND lock_expires_at > now()`) and return
// `OutboxUpdate::LeaseLost` if another worker has taken the lease over.
// `record_compensation` errors instead of returning `LeaseLost`,
// because recording compensation against a non-failed or
// already-compensated row is a programming bug, not an operational
// condition.
/// Mark a successfully-delivered outbox row.
///
/// Transitions `status` to `'delivered'`, sets `delivered_at = now()`,
/// increments `attempt_count`, and clears the lease fields so the row
/// is unambiguously done. Returns `Applied`, or `LeaseLost` if the
/// worker no longer holds the lease.
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
    .map_err(classify)?;
    Ok(if rows.rows_affected() == 1 {
        OutboxUpdate::Applied
    } else {
        OutboxUpdate::LeaseLost
    })
}
/// Record a transient delivery failure: schedule the row for retry
/// at `next_attempt_at`. The row goes back to `status='pending'`
/// (released from its lease) so another worker can pick it up at
/// the scheduled time, or the same worker can on its next claim.
///
/// `next_attempt_at` is a wall-clock instant the caller computes
/// (current time plus retry-after plus jitter); the row stays
/// invisible to claims until that moment.
///
/// **No upfront validation of `next_attempt_at`**: a past or
/// equal-now retry instant is accepted. Re-claim protection lives at
/// [`claim_pending_outbox_row`]'s `claim_before` bound, not here.
/// Validating here would conflict with the helper contract (lease loss
/// surfaces as [`OutboxUpdate::LeaseLost`], not [`PgError`]) and would
/// spuriously fail a slow legitimate delivery whose retry instant
/// elapses in transit.
///
/// Internal substrate of [`process_one_outbox_row`]; use that unless
/// driving the state machine manually.
#[doc(hidden)]
pub async fn mark_outbox_transient_attempt(
    pool: &PgPool,
    intent_id: Uuid,
    worker_id: &str,
    next_attempt_at: DateTime<Utc>,
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
        next_attempt_at,
    )
    .execute(pool)
    .await
    .map_err(classify)?;
    Ok(if rows.rows_affected() == 1 {
        OutboxUpdate::Applied
    } else {
        OutboxUpdate::LeaseLost
    })
}
/// Mark a non-retryable delivery failure. The row moves to
/// `status='failed'`, captures `failed_at` and `failure_reason`,
/// and releases its lease. A compensating transformation can then
/// be invoked and recorded via [`record_compensation`].
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
    .map_err(classify)?;
    Ok(if rows.rows_affected() == 1 {
        OutboxUpdate::Applied
    } else {
        OutboxUpdate::LeaseLost
    })
}
/// Link a compensating transformation to a failed outbox row.
///
/// Gated by two SQL `WHERE` preconditions: the row must be
/// `status='failed'` and must not already carry a
/// `compensation_transition_id`. Violating either is a programming bug
/// and surfaces as [`PgError::InvalidState`], not a silent no-op.
///
/// `compensation_transition_id` must reference a row in
/// `morpholog.audit` (foreign-key-enforced); the worker invokes the
/// compensating transformation via [`crate::propose_against_pg`] and passes
/// the resulting `transition_id` here.
///
/// Does NOT gate on a lease: by the time compensation is recorded the
/// row is in `failed` and the lease was already released by
/// [`mark_outbox_failed`].
///
/// **This is a lineage setter, not a duplicate-invocation guard.** The
/// `compensation_transition_id IS NULL` predicate only stops a second
/// *record* call from overwriting the first; it does not stop a second
/// *compensating transformation* from committing via
/// [`crate::propose_against_pg`] first. If two workers race the same `failed`
/// row, both can commit independent compensations - only the second
/// `record_compensation` fails, by which point a duplicate is already
/// in `morpholog.audit`.
///
/// Preventing that is the caller's responsibility: either retain lease
/// ownership across the failed -> commit -> record arc, or guard the
/// compensating transformation with an `original_intent_id` invariant.
/// See `docs/outbox-sketch.md`.
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
    .map_err(classify)?;
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
/// The row is selected with `FOR UPDATE SKIP LOCKED` inside an
/// `UPDATE ... RETURNING` so concurrent workers cannot race on the
/// same row; if two workers run this query at the same moment, one
/// claims the row, the other skips it and either finds the next
/// candidate or returns `None`.
///
/// Claim eligibility:
/// - `status='pending'` AND (`next_attempt_at IS NULL OR <= claim_before`):
///   a row whose retry backoff has elapsed (or which has no
///   scheduled retry) is eligible.
/// - OR `status='in_progress' AND lock_expires_at < now()`: a row
///   whose previous worker crashed mid-delivery and whose lease
///   has expired is also eligible. Reclaim is transparent.
///
/// `claim_before` is the upper bound for retry eligibility. One-shot
/// callers pass `Utc::now()`. A drain loop captures `Utc::now()` once
/// at the top of the pass and supplies that same instant every
/// iteration, so rows deferred *during* the pass (a deliverer
/// returning `Transient { next_attempt_at: now() + 1ms }`) stay
/// invisible until the next pass. Without this, a sub-second retry
/// would let the drain re-claim the same row indefinitely; the worker
/// would never sleep or observe shutdown.
///
/// Lease-expiry reclaim of `in_progress` rows still uses live `now()`:
/// those are dead-worker recoveries, not scheduling decisions.
///
/// On claim: sets `status='in_progress'`, `locked_by=worker_id`,
/// `lock_expires_at=now()+lease_duration`, and returns the full
/// `OutboxRow`.
///
/// `lease_duration` is the window during which the claiming worker has
/// exclusive rights to mutate the row through the `mark_*` helpers.
/// Choosing it is the worker's responsibility: long enough to cover
/// the deliverer's latency plus headroom, short enough that a crashed
/// worker's rows become reclaimable in reasonable time.
///
/// The deliverer must run **outside** any database transaction; this
/// helper opens and closes the only transaction the claim needs (a
/// single atomic UPDATE ... RETURNING), and the lease is held via the
/// `locked_by`/`lock_expires_at` columns rather than a held row lock.
///
/// Internal substrate of [`process_one_outbox_row`]; use that unless
/// driving the state machine manually.
#[doc(hidden)]
pub async fn claim_pending_outbox_row(
    pool: &PgPool,
    worker_id: &str,
    intent_type: &str,
    lease_duration: std::time::Duration,
    claim_before: DateTime<Utc>,
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
        claim_before,
    )
    .fetch_optional(pool)
    .await
    .map_err(classify)?;
    row_opt.map(decode_outbox_row).transpose()
}
/// Release a held lease without resolving the row to a terminal
/// state. The row returns to `status='pending'`, claimable by another
/// worker on its next pass.
///
/// For shutdown paths: a worker dying gracefully releases its
/// in-flight claims so they re-pick immediately rather than waiting
/// for lease expiry. Returns `LeaseLost` if the worker no longer holds
/// the lease (expected when a slow worker shuts down after expiry).
///
/// Internal substrate of the worker shutdown path; rarely needed
/// directly.
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
    .map_err(classify)?;
    Ok(if rows.rows_affected() == 1 {
        OutboxUpdate::Applied
    } else {
        OutboxUpdate::LeaseLost
    })
}
/// Soonest future `next_attempt_at` over pending rows of the given
/// `intent_type`. Returns `None` if no such row exists.
///
/// A polling worker uses this after an empty drain to wake exactly
/// when the soonest scheduled retry becomes due (but no later than the
/// base poll interval, so newly-enqueued due rows are still picked up
/// promptly) instead of always sleeping the full interval.
///
/// `next_attempt_at` is filtered to `> now()`: a row whose retry
/// instant has already passed would have been claimed by the drain
/// that just ran.
pub async fn earliest_pending_retry(
    pool: &PgPool,
    intent_type: &str,
) -> Result<Option<DateTime<Utc>>, PgError> {
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
    .map_err(classify)?;
    Ok(row.and_then(|r| r.earliest))
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
/// Eligible rows are `status='failed' AND compensation_transition_id
/// IS NULL`. The claim transitions the row to `compensation_in_progress`
/// and sets the lease. Once held, the worker invokes the compensating
/// transformation via [`crate::propose_against_pg`] and resolves the row with
/// [`complete_compensation`] (on `Committed`) or
/// [`mark_compensation_failed`] (on `Rejected`).
///
/// `SELECT ... FOR UPDATE SKIP LOCKED` guarantees at most one worker
/// holds the compensation lease at a time. Returns `Ok(None)` when no
/// eligible row exists for `intent_id` (missing, not `failed`, already
/// compensated, or locked by another worker mid-claim).
///
/// **Does NOT transparently reclaim expired-lease
/// compensation_in_progress rows** (unlike [`claim_pending_outbox_row`]
/// for `in_progress`). Reclaim would risk duplicate compensation if a
/// worker crashed *after* committing the compensating transformation
/// but *before* `complete_compensation`; a stuck row requires operator
/// intervention instead. The lease narrows the duplicate-compensation
/// race to the window between commit and `complete_compensation`;
/// programs needing full immunity should additionally guard the
/// compensating transformation with a
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
    .map_err(classify)?;
    row_opt.map(decode_outbox_row).transpose()
}
/// Resolve a compensation_in_progress row on success: transitions it
/// back to `failed` with `compensation_transition_id` recorded, and
/// releases the lease.
///
/// Gated on the worker holding the lease; returns
/// `OutboxUpdate::LeaseLost` otherwise. `compensation_transition_id`
/// must reference a row in `morpholog.audit` (foreign-key-enforced),
/// typically the `transition_id` [`crate::propose_against_pg`] returned when
/// the compensating transformation committed.
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
    .map_err(classify)?;
    Ok(if rows.rows_affected() == 1 {
        OutboxUpdate::Applied
    } else {
        OutboxUpdate::LeaseLost
    })
}
/// Resolve a compensation_in_progress row on failure: transitions it
/// to `compensation_failed` with `reason` recorded, and releases the
/// lease.
///
/// Use this when the compensating transformation was itself rejected
/// by an invariant ([`crate::propose_against_pg`] returned `Rejected`). This
/// is the genuinely-broken state - the original delivery failed AND
/// the compensation cannot be admitted - and stays in
/// `compensation_failed` until operator intervention.
///
/// Gated on the worker holding the lease; returns
/// `OutboxUpdate::LeaseLost` otherwise.
///
/// `reason` **overwrites** the original delivery `failure_reason`,
/// which is then lost to the morpholog tables: state mutators write no
/// audit rows (only transformations do). Callers needing both reasons
/// must capture the original externally before calling this.
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
    .map_err(classify)?;
    Ok(if rows.rows_affected() == 1 {
        OutboxUpdate::Applied
    } else {
        OutboxUpdate::LeaseLost
    })
}
/// Outcome a [`Deliverer`] returns from a single delivery attempt.
///
/// The processor uses this to route the row through the
/// delivery-state machine: `Delivered` -> `delivered`, `Transient`
/// -> back to `pending` with the requested `next_attempt_at`,
/// `NonRetryable` -> `failed` (and then optional compensation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// Delivery succeeded. The processor will mark the row
    /// `delivered`.
    Delivered,
    /// Delivery failed but should be retried no sooner than
    /// `next_attempt_at`. The processor returns the row to
    /// `pending` and sets that timestamp; the claim helper will
    /// then skip the row until the timestamp has passed. The
    /// deliverer is responsible for backoff policy (constant,
    /// exponential, jittered, etc.); the processor stores
    /// whatever instant the deliverer chose.
    Transient { next_attempt_at: DateTime<Utc> },
    /// Delivery failed in a way that should not be retried (the
    /// counterparty rejected the request authoritatively, the
    /// recipient does not exist, etc.). The processor marks the
    /// row `failed` with `reason` recorded. If the processor was
    /// supplied a [`CompensationSpec`], it then tries to claim the
    /// compensation lease via [`begin_compensation`] and run the
    /// compensating transformation.
    NonRetryable { reason: String },
}
/// A delivery target. Implementors define how to take one
/// admitted-and-enqueued intent and push it to the external world.
///
/// The processor passes the full [`OutboxRow`] - enough context for
/// retry/jitter decisions and any per-target idempotency-key handling
/// the receiver needs.
///
/// Implementors MUST NOT mutate any morpholog tables from `deliver`:
/// the processor owns the state machine, the deliverer owns only the
/// external side effect.
///
/// `Send + Sync` on the implementor and `Send` on the returned future
/// are baked in so polling loops can `tokio::spawn(deliverer.deliver(...))`
/// against an arbitrary `D: Deliverer`. RPITIT does not let callers add
/// the future's `Send` bound later, so it is fixed here.
pub trait Deliverer: Send + Sync {
    fn deliver(&self, row: &OutboxRow)
    -> impl std::future::Future<Output = DeliveryOutcome> + Send;
}
/// Closure mapping the just-failed outbox row to the arguments the
/// compensating transformation is invoked with. Boxed rather than
/// generic so `process_one_outbox_row`'s `Option<&CompensationSpec>`
/// has a single concrete type (callers pass `None` without an
/// inference workaround).
pub type CompensationArgsFromRow = Box<dyn Fn(&OutboxRow) -> Vec<EvalValue> + Send + Sync>;
/// Configuration the processor consults when delivery returns
/// `NonRetryable` and the row is moved to `failed`.
///
/// `args_from_row` is invoked AFTER [`begin_compensation`] has claimed
/// the lease, so the row it receives carries `failure_reason` from the
/// just-failed attempt and the closure can fold it into the
/// compensating transformation's arguments.
///
/// The compensating transformation goes through [`crate::propose_against_pg`]
/// like any other - every invariant check, its own audit row, its own
/// outbox intents - so the audit log preserves the full lineage:
/// original commit, the `compensation_transition_id` linkage, and the
/// compensation's audit row.
pub struct CompensationSpec {
    pub transformation: Transformation,
    pub invariants: Vec<Invariant>,
    /// The programme's definitions, threaded into the compensating
    /// proposal exactly as into any other; empty when the model
    /// declares none.
    pub definitions: Vec<Definition>,
    pub args_from_row: CompensationArgsFromRow,
}
/// Outcome of one [`process_one_outbox_row`] cycle. Surfaces enough
/// information that operational tooling and tests can assert which
/// branch was taken without re-querying the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessOutcome {
    /// No row was claimable: the outbox has no pending or
    /// expired-leased row of the requested `intent_type` whose
    /// `next_attempt_at` is due. The processor did nothing.
    NoRowAvailable,
    /// Delivery succeeded; the row is now `delivered`.
    Delivered { intent_id: Uuid },
    /// Delivery returned `Transient`; the row is back to `pending`
    /// with the supplied retry instant.
    TransientRetry {
        intent_id: Uuid,
        next_attempt_at: DateTime<Utc>,
    },
    /// Delivery returned `NonRetryable` and no [`CompensationSpec`]
    /// was supplied; the row is `failed`.
    Failed { intent_id: Uuid, reason: String },
    /// Delivery returned `NonRetryable` and a compensation was
    /// configured, but another worker already holds the
    /// compensation lease (or compensation already ran on this
    /// row). The processor did not invoke the compensating
    /// transformation. The row is `failed` (or further along) and
    /// this processor cycle is done.
    CompensationDeferred { intent_id: Uuid },
    /// Compensation ran and committed. The row is back to `failed`
    /// with `compensation_transition_id` pointing at the
    /// compensation's audit row.
    Compensated {
        intent_id: Uuid,
        compensation_transition_id: Uuid,
    },
    /// Compensation ran but was rejected by an invariant. The row
    /// is in `compensation_failed`. This is the genuinely-broken
    /// state requiring operator intervention.
    CompensationFailed { intent_id: Uuid, reason: String },
    /// A state-mutating helper returned [`OutboxUpdate::LeaseLost`]:
    /// the deliverer (or compensation arm) ran to completion, but the
    /// lease had expired and another worker reclaimed the row first.
    ///
    /// Not an error - the honest answer when a slow deliverer races the
    /// lease clock. Calling code should log and alert (the orphan-audit
    /// case during compensation, where the compensating transformation
    /// committed but the row's pointer never landed, is the most
    /// noteworthy; reconcile from the audit log) and move on.
    LeaseLost { intent_id: Uuid },
}
/// Drive one outbox row through the full delivery-and-compensation
/// state machine. Intended to be called in a loop by a worker
/// process (one call = one row processed; the loop owns scheduling).
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
/// Concurrency: safe across processes. Both [`claim_pending_outbox_row`]
/// and [`begin_compensation`] use `SELECT ... FOR UPDATE SKIP LOCKED`,
/// so at most one worker claims a given row or invokes the
/// compensating transformation for a given failed row.
///
/// The compensation race is closed under normal operation; a worker
/// crashing between `propose_against_pg` commit and
/// `complete_compensation` leaves the row stuck for operator recovery
/// rather than risking a duplicate (see [`begin_compensation`]).
///
/// `claim_before` is passed through to [`claim_pending_outbox_row`];
/// see it for the drain-loop safety rationale.
#[allow(clippy::too_many_arguments)]
pub async fn process_one_outbox_row<D>(
    pool: &PgPool,
    worker_id: &str,
    intent_type: &str,
    lease_duration: std::time::Duration,
    deliverer: &D,
    compensation: Option<&CompensationSpec>,
    claim_before: DateTime<Utc>,
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
                    // The row is no longer ours; compensation must not
                    // run because we never moved it to 'failed', which
                    // begin_compensation requires.
                    return Ok(ProcessOutcome::LeaseLost { intent_id });
                }
                OutboxUpdate::Applied => {}
            }
            let Some(spec) = compensation else {
                return Ok(ProcessOutcome::Failed { intent_id, reason });
            };
            // Re-claim the compensation lease that mark_outbox_failed
            // just released. SKIP LOCKED ensures at most one worker
            // wins under a concurrent recovery scan.
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
                PgProposalOutcome::Rejected { reason } => {
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
