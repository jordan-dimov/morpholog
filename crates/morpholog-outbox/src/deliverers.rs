//! Concrete [`Deliverer`] implementations.

use std::io::{self, Write};

use morpholog_postgres::{Deliverer, DeliveryOutcome, OutboxRow};
use serde_json::json;

/// Prints each outbox intent as a single JSON line to stdout.
///
/// Returns `Delivered` once the line is written and flushed, and `NonRetryable` if stdout is
/// broken (for example, the downstream pipe was closed). Each line has this shape:
///
/// ```json
/// {
///   "intent_id": "<uuid>",
///   "transition_id": "<uuid>",
///   "intent_type": "<name>",
///   "arguments": [...],
///   "idempotency_key": "<string>",
///   "attempt_count": <int>
/// }
/// ```
///
/// The outbox is at-least-once, so consumers use `idempotency_key` to drop redeliveries.
///
/// **Not a production delivery path.** Stdout has no backpressure and no acknowledgement; once
/// the bytes leave this process, what happens is up to the pipeline. Use it for development
/// and smoke tests. A real downstream gets its own [`Deliverer`] impl.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdoutDeliverer;

impl Deliverer for StdoutDeliverer {
    async fn deliver(&self, row: &OutboxRow) -> DeliveryOutcome {
        let payload = json!({
            "intent_id": row.intent_id,
            "transition_id": row.transition_id,
            "intent_type": row.intent_type,
            "arguments": row.arguments,
            "idempotency_key": row.idempotency_key,
            "attempt_count": row.attempt_count,
        });
        let mut stdout = io::stdout().lock();
        if let Err(e) = writeln!(stdout, "{payload}") {
            return DeliveryOutcome::NonRetryable {
                reason: format!("StdoutDeliverer: writeln to stdout failed: {e}"),
            };
        }
        // Flush before reporting Delivered: a crash with bytes still buffered would lose the
        // intent after it was marked delivered.
        if let Err(e) = stdout.flush() {
            return DeliveryOutcome::NonRetryable {
                reason: format!("StdoutDeliverer: flush failed: {e}"),
            };
        }
        DeliveryOutcome::Delivered
    }
}
