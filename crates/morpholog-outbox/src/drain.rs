use jiff::Timestamp;
use morpholog_postgres::{
    CompensationSpec, Deliverer, PgError, PgPool, ProcessOutcome, process_one_outbox_row,
};
use std::time::Duration;

/// Drain every currently-claimable outbox row of the given `intent_type` in one pass.
///
/// Calls [`morpholog_postgres::process_one_outbox_row`] until it returns
/// [`ProcessOutcome::NoRowAvailable`], and returns every other outcome in order. The final
/// `NoRowAvailable` is not included. A transient or failed delivery does not stop the drain.
///
/// The pass start time is the claim cutoff for the whole pass. A row that becomes due during
/// the pass, such as a retry scheduled 1ms out, waits for the next pass. This keeps the drain
/// finite, so the worker notices shutdown promptly even against sub-second retries.
///
/// No sleeping or scheduling happens here; call it directly to process whatever is due now.
/// Safe to run concurrently: claims use `FOR UPDATE SKIP LOCKED`, so parallel drains take
/// distinct rows without blocking each other.
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
    let pass_start = Timestamp::now();
    let mut outcomes = Vec::new();
    loop {
        let outcome = process_one_outbox_row(
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
