//! Test-support [`Deliverer`] implementations.
//!
//! Constant-outcome deliverers, one per [`DeliveryOutcome`] variant, for
//! tests that exercise the processor or worker without a real target.
//!
//! Always compiled (no feature flag) so integration tests can use them.
//! Production code should not import them.

use jiff::Timestamp;

use crate::{Deliverer, DeliveryOutcome, OutboxRow};

/// The one reset for a disposable test database: every governed table, in
/// one statement. The test suites and the bench's `--reset` all use it, so
/// a governed table added to the schema must be added here.
pub const RESET_SQL: &str = "DO $$
DECLARE
    ix text;
BEGIN
    TRUNCATE morpholog.outbox, morpholog.claims, morpholog.audit,
        morpholog.audit_checkpoints, morpholog.rejections, morpholog.index_requirement,
        morpholog.managed_index CASCADE;
    -- The managed indexes go with their registry, so a test starts from the
    -- indexes it provisions and none another test left behind.
    FOR ix IN SELECT indexname FROM pg_indexes
               WHERE schemaname = 'morpholog' AND indexname LIKE 'morpholog\\_ci\\_%'
    LOOP
        EXECUTE format('DROP INDEX morpholog.%I', ix);
    END LOOP;
END $$";

/// Always returns [`DeliveryOutcome::Delivered`].
#[derive(Debug, Default, Clone, Copy)]
pub struct AlwaysDelivers;

impl Deliverer for AlwaysDelivers {
    async fn deliver(&self, _row: &OutboxRow) -> DeliveryOutcome {
        DeliveryOutcome::Delivered
    }
}

/// Always returns [`DeliveryOutcome::Transient`] with the configured
/// `next_attempt_at`, for tests of the retry path.
#[derive(Debug, Clone, Copy)]
pub struct AlwaysTransient {
    pub next_attempt_at: Timestamp,
}

impl Deliverer for AlwaysTransient {
    async fn deliver(&self, _row: &OutboxRow) -> DeliveryOutcome {
        DeliveryOutcome::Transient {
            next_attempt_at: self.next_attempt_at,
        }
    }
}

/// Always returns [`DeliveryOutcome::NonRetryable`] with the configured
/// `reason`, for tests of the failed-state and compensation paths.
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
