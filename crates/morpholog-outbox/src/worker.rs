use std::time::Duration;

use morpholog_postgres::{CompensationSpec, Deliverer, PgError, PgPool, earliest_pending_retry};
use tokio::sync::watch;

use crate::clock::Clock;
use crate::drain::process_available_outbox_rows;
use crate::jitter::JitterRng;

/// Polling worker over the single-row outbox processor.
///
/// Each iteration drains all claimable rows of `intent_type` with
/// [`process_available_outbox_rows`], then sleeps `base_interval` times a [`JitterRng`] factor,
/// or less if a scheduled retry is due sooner. Shutdown cuts the sleep short.
///
/// Tests substitute [`crate::testing::MockClock`] and [`crate::testing::FixedJitter`];
/// production uses [`crate::clock::RealClock`] and [`crate::jitter::RandJitter`].
///
/// A drain pass in progress is not interrupted: with a large backlog, shutdown can wait for
/// the whole pass to finish.
pub struct OutboxWorker<D, C, R>
where
    D: Deliverer + 'static,
    C: Clock,
    R: JitterRng,
{
    pool: PgPool,
    worker_id: String,
    intent_type: String,
    lease_duration: Duration,
    base_interval: Duration,
    jitter_low: f64,
    jitter_high: f64,
    deliverer: D,
    compensation: Option<CompensationSpec>,
    clock: C,
    rng: R,
}

impl<D, C, R> OutboxWorker<D, C, R>
where
    D: Deliverer + 'static,
    C: Clock,
    R: JitterRng,
{
    /// Construct a worker with defaults: 30s lease, 1s base poll interval, +/-25% jitter, no
    /// compensation. Adjust with the `with_*` setters before calling [`Self::run`].
    pub fn new(
        pool: PgPool,
        worker_id: impl Into<String>,
        intent_type: impl Into<String>,
        deliverer: D,
        clock: C,
        rng: R,
    ) -> Self {
        Self {
            pool,
            worker_id: worker_id.into(),
            intent_type: intent_type.into(),
            lease_duration: Duration::from_secs(30),
            base_interval: Duration::from_secs(1),
            jitter_low: 0.75,
            jitter_high: 1.25,
            deliverer,
            compensation: None,
            clock,
            rng,
        }
    }

    pub fn with_lease_duration(mut self, d: Duration) -> Self {
        self.lease_duration = d;
        self
    }

    /// Set the base poll interval between drain passes.
    ///
    /// # Panics
    ///
    /// If `d` is zero, which would busy-poll the database.
    pub fn with_base_interval(mut self, d: Duration) -> Self {
        assert!(
            !d.is_zero(),
            "base_interval must be > 0; a zero interval would busy-poll the database"
        );
        self.base_interval = d;
        self
    }

    /// Set the jitter range `[low, high)` the base interval is multiplied by.
    ///
    /// # Panics
    ///
    /// Unless `0 < low < high`. Equal bounds are refused because [`crate::RandJitter`] cannot
    /// sample an empty range; for no jitter, use a constant [`crate::JitterRng`] such as
    /// [`crate::testing::FixedJitter`].
    pub fn with_jitter(mut self, low: f64, high: f64) -> Self {
        assert!(
            low > 0.0 && high > low,
            "jitter range must be (low > 0, high > low strict); got [{low}, {high})"
        );
        self.jitter_low = low;
        self.jitter_high = high;
        self
    }

    pub fn with_compensation(mut self, c: CompensationSpec) -> Self {
        self.compensation = Some(c);
        self
    }

    /// Run the polling loop until `shutdown` is set to `true`.
    ///
    /// Returns `Ok(())` on shutdown, including when every shutdown sender is dropped.
    ///
    /// # Errors
    ///
    /// The first [`PgError`] from a drain pass stops the worker; restarting is the caller's call.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<(), PgError> {
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            process_available_outbox_rows(
                &self.pool,
                &self.worker_id,
                &self.intent_type,
                self.lease_duration,
                &self.deliverer,
                self.compensation.as_ref(),
            )
            .await?;

            let factor = self.rng.jitter_factor(self.jitter_low, self.jitter_high);
            let base_dur = self.base_interval.mul_f64(factor);
            let sleep_dur = self.smart_sleep_duration(base_dur).await?;
            tokio::select! {
                _ = self.clock.sleep_for(sleep_dur) => {}
                changed = shutdown.changed() => {
                    match changed {
                        Ok(()) => {
                            if *shutdown.borrow() {
                                return Ok(());
                            }
                        }
                        // All senders dropped: no signal can arrive any more, so stop cleanly.
                        Err(_) => return Ok(()),
                    }
                }
            }
        }
    }

    /// How long to sleep before the next drain pass: until the soonest scheduled retry, capped
    /// at `base_dur` so newly enqueued rows are still picked up.
    async fn smart_sleep_duration(&self, base_dur: Duration) -> Result<Duration, PgError> {
        let Some(next) = earliest_pending_retry(&self.pool, &self.intent_type).await? else {
            return Ok(base_dur);
        };
        let now = self.clock.now();
        let Ok(until) = Duration::try_from(next.duration_since(now)) else {
            return Ok(base_dur);
        };
        Ok(std::cmp::min(base_dur, until))
    }
}
