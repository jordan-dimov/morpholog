//! Concrete [`Deliverer`] implementations.
//!
//! `StdoutDeliverer` is the canonical first impl: it prints each
//! intent as a JSON line to stdout and reports `Delivered`. Useful
//! for local development, smoke tests, and any deployment whose
//! "downstream" is a structured-log pipeline that ingests stdout.

use morpholog_postgres::{Deliverer, DeliveryOutcome, OutboxRow};
use serde_json::json;

/// Prints each outbox intent as a single JSON line to stdout and
/// reports `Delivered`. The serialized shape is:
///
/// ```json
/// {
///   "intent_id": "<uuid>",
///   "transition_id": "<uuid>",
///   "intent_type": "<name>",
///   "arguments": [...],
///   "attempt_count": <int>
/// }
/// ```
///
/// Newline-terminated so log-line-oriented consumers can parse
/// each delivery as a discrete record. Never fails: there is no
/// `Transient` or `NonRetryable` path. Use this for development,
/// smoke tests, or as a baseline reference when implementing a
/// real downstream-aware deliverer.
#[derive(Debug, Default, Clone, Copy)]
pub struct StdoutDeliverer;

impl Deliverer for StdoutDeliverer {
    async fn deliver(&self, row: &OutboxRow) -> DeliveryOutcome {
        let payload = json!({
            "intent_id": row.intent_id,
            "transition_id": row.transition_id,
            "intent_type": row.intent_type,
            "arguments": row.arguments,
            "attempt_count": row.attempt_count,
        });
        println!("{payload}");
        DeliveryOutcome::Delivered
    }
}
