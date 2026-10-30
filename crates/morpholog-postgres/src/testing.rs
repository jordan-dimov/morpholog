//! Test-support [`Deliverer`] implementations.
//!
//! Constant-outcome deliverers, one per [`DeliveryOutcome`] variant, for
//! tests that exercise the processor or worker without a real target.
//!
//! Always compiled (no feature flag) so integration tests can use them.
//! Production code should not import them.

use jiff::Timestamp;

use crate::{Deliverer, DeliveryOutcome, OutboxRow};

/// The one reset for a disposable test database: every governed table.
/// The test suites and the bench's `--reset` all use it, so a governed
/// table added to the schema must be added here.
pub async fn reset_database(pool: &sqlx::PgPool) -> Result<(), crate::PgError> {
    sqlx::raw_sql(RESET_SQL)
        .execute(pool)
        .await
        .map(|_| ())
        .map_err(crate::error::classify)
}

/// [`reset_database`] as a database stood before the canonical order:
/// the ordering trigger off, so a test can write the rows an older binary
/// wrote. [`activate`] then does what the migration does.
pub async fn reset_before_activation(pool: &sqlx::PgPool) -> Result<(), crate::PgError> {
    use crate::error::classify;
    let mut tx = pool.begin().await.map_err(classify)?;
    sqlx::raw_sql(RESET_SQL)
        .execute(&mut *tx)
        .await
        .map_err(classify)?;
    sqlx::raw_sql("ALTER TABLE morpholog.audit DISABLE TRIGGER audit_order")
        .execute(&mut *tx)
        .await
        .map_err(classify)?;
    tx.commit().await.map_err(classify)
}

/// Turn the ordering trigger back on and write the activation over
/// the rows already in the log, as migration 27 does.
pub async fn activate(pool: &sqlx::PgPool) -> Result<(), crate::PgError> {
    use crate::error::classify;
    let mut tx = pool.begin().await.map_err(classify)?;
    sqlx::raw_sql(
        "ALTER TABLE morpholog.audit ENABLE TRIGGER audit_order;
         LOCK TABLE morpholog.claims, morpholog.audit IN EXCLUSIVE MODE;",
    )
    .execute(&mut *tx)
    .await
    .map_err(classify)?;
    crate::activation::write_activation_record(&mut tx).await?;
    tx.commit().await.map_err(classify)
}

const RESET_SQL: &str = "DO $$
DECLARE
    ix text;
BEGIN
    TRUNCATE morpholog.outbox, morpholog.claims, morpholog.audit,
        morpholog.audit_checkpoints, morpholog.rejections, morpholog.index_requirement,
        morpholog.managed_index, morpholog.audit_order_txn CASCADE;
    ALTER SEQUENCE morpholog.audit_order_seq RESTART;
    ALTER TABLE morpholog.audit ENABLE TRIGGER audit_order;
    -- The managed indexes go with their registry, so a test starts from the
    -- indexes it provisions and none another test left behind.
    FOR ix IN SELECT indexname FROM pg_indexes
               WHERE schemaname = 'morpholog' AND indexname LIKE 'morpholog\\_ci\\_%'
    LOOP
        EXECUTE format('DROP INDEX morpholog.%I', ix);
    END LOOP;
    FOR ix IN SELECT stxname FROM pg_statistic_ext
               WHERE stxnamespace = 'morpholog'::regnamespace AND stxname LIKE 'morpholog\\_cs\\_%'
    LOOP
        EXECUTE format('DROP STATISTICS morpholog.%I', ix);
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
