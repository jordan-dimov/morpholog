//! Concrete [`Deliverer`] implementations.
//!
//! `StdoutDeliverer` is the canonical first impl: it prints each
//! intent as a JSON line to stdout and reports `Delivered`. Useful
//! for local development, smoke tests, and any deployment whose
//! "downstream" is a structured-log pipeline that ingests stdout.

use std::io::{self, Write};

use morpholog_postgres::{Deliverer, DeliveryOutcome, OutboxRow};
use serde_json::json;

/// Prints each outbox intent as a single JSON line to stdout.
/// Returns `Delivered` on a successful write-and-flush,
/// `NonRetryable` if the stdout sink is broken (e.g., the
/// downstream pipe was closed).
///
/// The serialized shape is:
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
/// Newline-terminated so log-line-oriented consumers can parse
/// each delivery as a discrete record. The `idempotency_key` is
/// included so downstream consumers can deduplicate redelivered
/// intents (the outbox is at-least-once). Use this for
/// development, smoke tests, or as a baseline reference when
/// implementing a real downstream-aware deliverer.
///
/// **NOT a production delivery path.** Stdout has no
/// backpressure, no acknowledgement, no idempotency guarantee
/// beyond what the consumer pipeline provides. The deliverer
/// flushes stdout before reporting `Delivered` (so bytes have
/// left this process's buffer), but downstream behavior past that
/// is whatever the pipeline does. Suitable for demos and smoke
/// tests; a real downstream enters as its own concrete
/// [`Deliverer`] impl.
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
        // Flush before reporting Delivered: writeln! may leave
        // bytes in stdout's buffer (especially when piped to a
        // log collector). If the process crashes between
        // returning Delivered and the buffer flush, the intent
        // is lost; we promised at-least-once.
        if let Err(e) = stdout.flush() {
            return DeliveryOutcome::NonRetryable {
                reason: format!("StdoutDeliverer: flush failed: {e}"),
            };
        }
        DeliveryOutcome::Delivered
    }
}
