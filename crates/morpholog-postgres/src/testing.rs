//! Test-support [`Deliverer`] implementations.
//!
//! Constant-outcome deliverers, one per [`DeliveryOutcome`] variant.
//! Integration tests in this crate and in `morpholog-outbox` use
//! them as drop-in stubs whenever a test wants to exercise the
//! processor or worker pipeline without tying behaviour to a
//! specific external target.
//!
//! These types are always compiled (no feature flag) so integration
//! tests can use them without configuring a feature. Production code
//! should not import them - they are decision-pinned stubs, not
//! deployable deliverers.

use chrono::{DateTime, Utc};

use crate::{Deliverer, DeliveryOutcome, OutboxRow};

/// The one authoritative reset for a disposable test database: every
/// governed table, in one statement. Consumed by the integration
/// suites here, in `morpholog-outbox`, in the CLI, and by the bench's
/// `--reset` - a governed table added to the schema is added HERE,
/// once (a hand-copied list in the bench once drifted and silently
/// stopped truncating checkpoints).
pub const RESET_SQL: &str = "TRUNCATE morpholog.outbox, morpholog.claims, morpholog.audit, \
     morpholog.audit_checkpoints, morpholog.rejections CASCADE";

/// Always returns [`DeliveryOutcome::Delivered`]. The simplest
/// happy-path deliverer for tests that want to verify the processor
/// moves rows to `delivered` and that the worker reports successful
/// delivery counts.
#[derive(Debug, Default, Clone, Copy)]
pub struct AlwaysDelivers;

impl Deliverer for AlwaysDelivers {
    async fn deliver(&self, _row: &OutboxRow) -> DeliveryOutcome {
        DeliveryOutcome::Delivered
    }
}

/// Always returns [`DeliveryOutcome::Transient`] with the configured
/// `next_attempt_at`. Useful for tests that exercise the retry path,
/// including ones that need to assert the processor honours the
/// deliverer-chosen retry instant.
#[derive(Debug, Clone, Copy)]
pub struct AlwaysTransient {
    pub next_attempt_at: DateTime<Utc>,
}

impl Deliverer for AlwaysTransient {
    async fn deliver(&self, _row: &OutboxRow) -> DeliveryOutcome {
        DeliveryOutcome::Transient {
            next_attempt_at: self.next_attempt_at,
        }
    }
}

/// Always returns [`DeliveryOutcome::NonRetryable`] with the
/// configured `reason`. Useful for tests that exercise the
/// failed-state and compensation paths.
#[derive(Debug, Clone)]
pub struct AlwaysNonRetryable {
    pub reason: String,
}

impl AlwaysNonRetryable {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl Deliverer for AlwaysNonRetryable {
    async fn deliver(&self, _row: &OutboxRow) -> DeliveryOutcome {
        DeliveryOutcome::NonRetryable {
            reason: self.reason.clone(),
        }
    }
}
