use morpholog_postgres::{
    CompensationSpec, Deliverer, PgError, PgPool, ProcessOutcome, process_one_outbox_row,
};
use std::time::Duration;

/// Drain every currently-claimable outbox row of the given
/// `intent_type` in one pass.
///
/// Repeatedly invokes [`process_one_outbox_row`], appending each
/// returned [`ProcessOutcome`] to the result vector, until the
/// processor returns [`ProcessOutcome::NoRowAvailable`]. The
/// NoRowAvailable result itself is NOT appended - it is a stop
/// signal, not work.
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
/// `claim_pending_outbox_row` already uses `SELECT ... FOR UPDATE
/// SKIP LOCKED`, so two workers calling this in parallel will
/// each get distinct rows and neither will block the other.
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
    let mut outcomes = Vec::new();
    loop {
        let outcome = process_one_outbox_row(
            pool,
            worker_id,
            intent_type,
            lease_duration,
            deliverer,
            compensation,
        )
        .await?;
        if matches!(outcome, ProcessOutcome::NoRowAvailable) {
            return Ok(outcomes);
        }
        outcomes.push(outcome);
    }
}
