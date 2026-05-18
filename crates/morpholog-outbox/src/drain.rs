use chrono::Utc;
use morpholog_postgres::{
    CompensationSpec, Deliverer, PgError, PgPool, ProcessOutcome, process_one_outbox_row_before,
};
use std::time::Duration;

/// Drain every currently-claimable outbox row of the given
/// `intent_type` in one pass.
///
/// Repeatedly invokes
/// [`morpholog_postgres::process_one_outbox_row_before`], appending
/// each returned [`ProcessOutcome`] to the result vector, until the
/// processor returns [`ProcessOutcome::NoRowAvailable`]. The
/// NoRowAvailable result itself is NOT appended - it is a stop
/// signal, not work.
///
/// **Pass-boundary semantics**: the drain captures `Utc::now()`
/// once at the top and uses it as the `claim_before` upper bound
/// for every iteration. Rows scheduled to become due *during* the
/// pass (e.g., a deliverer that returns
/// `Transient { next_attempt_at: now() + 1ms }`) are NOT
/// re-claimed in the same pass, even if wall-clock time has
/// advanced past their `next_attempt_at` by the time the next
/// iteration runs. They become claimable on the next drain tick,
/// after the worker has slept. This guarantees the drain
/// terminates and the worker observes shutdown promptly even
/// against deliverers that schedule sub-second retries.
///
/// **Drain only stops on `NoRowAvailable`**. Every other outcome
/// (`Delivered`, `TransientRetry`, `Failed`, `CompensationDeferred`,
/// `Compensated`, `CompensationFailed`, `LeaseLost`) is non-blocking:
/// the row has reached some terminal-or-deferred state and the
/// drain proceeds to the next row. A single transient or failed
/// delivery does not stop the drain from making progress on the
/// rows behind it.
///
/// This function is intentionally synchronous in shape (no sleeps,
/// no jitter, no scheduling). It is the inner action that a
/// polling worker invokes on each tick. Callers who only want
/// "process whatever is due right now" can call this directly.
///
/// Concurrency: safe under concurrent invocation. The processor's
/// claim uses `SELECT ... FOR UPDATE SKIP LOCKED`, so two workers
/// calling this in parallel will each get distinct rows and
/// neither will block the other.
pub async fn process_available_outbox_rows<D>(
    pool: &PgPool,
    worker_id: &str,
    intent_type: &str,
    lease_duration: Duration,
    deliverer: &D,
    compensation: Option<&CompensationSpec>,
) -> Result<Vec<ProcessOutcome>, PgError>
where
    D: Deliverer,
{
    let pass_start = Utc::now();
    let mut outcomes = Vec::new();
    loop {
        let outcome = process_one_outbox_row_before(
            pool,
            worker_id,
            intent_type,
            lease_duration,
            deliverer,
            compensation,
            pass_start,
        )
        .await?;
        if matches!(outcome, ProcessOutcome::NoRowAvailable) {
            return Ok(outcomes);
        }
        outcomes.push(outcome);
    }
}
