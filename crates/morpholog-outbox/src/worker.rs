use std::time::Duration;

use morpholog_postgres::{
    CompensationSpec, Deliverer, PgError, PgPool, ProcessOutcome, earliest_pending_retry,
};
use tokio::sync::watch;

use crate::clock::Clock;
use crate::drain::process_available_outbox_rows;
use crate::jitter::JitterRng;

/// Polling worker over the single-row outbox processor.
///
/// The worker owns a loop that:
/// 1. drains all currently-claimable rows of `intent_type` via
///    [`process_available_outbox_rows`];
/// 2. computes a jittered sleep duration (`base_interval *
///    jitter_factor`, where the factor is drawn by the
///    [`JitterRng`] from `[jitter_low, jitter_high]`);
/// 3. sleeps that long via the [`Clock`], OR returns early if the
///    `shutdown` signal flips;
/// 4. checks `shutdown` and either loops or returns.
///
/// The [`Clock`] and [`JitterRng`] generics let tests substitute
/// deterministic implementations: see [`crate::testing::MockClock`]
/// and [`crate::testing::FixedJitter`]. Production code uses
/// [`crate::clock::RealClock`] and [`crate::jitter::RandJitter`].
///
/// **Shutdown semantics**: the worker checks `shutdown.borrow()`
/// before each drain pass and uses `tokio::select!` to race the
/// sleep against `shutdown.changed()`. A drain pass already in
/// progress is NOT interruptible - it will finish all claimable
/// work before the loop notices shutdown. For a busy queue with a
/// large backlog this could mean a multi-second drain after
/// shutdown is requested. If interruption mid-drain becomes
/// important, the drain helper itself will need a cancellation
/// argument; not yet forced.
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
    /// Construct a worker with the supplied essentials and
    /// sensible defaults: 30s lease, 1s base poll interval, ±25%
    /// jitter, no compensation. Adjust via the `with_*` setters
    /// before calling [`Self::run`].
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

    /// Set the base poll interval between drain passes. Must be
    /// non-zero - a zero interval would busy-poll the database
    /// continuously.
    pub fn with_base_interval(mut self, d: Duration) -> Self {
        assert!(
            !d.is_zero(),
            "base_interval must be > 0; a zero interval would busy-poll the database"
        );
        self.base_interval = d;
        self
    }

    /// Set the jitter range `[low, high)` used to multiply the
    /// base interval each iteration. Bounds must satisfy
    /// `0 < low < high` (strict on both inequalities). Equal bounds
    /// are not allowed because [`RandJitter`] samples the
    /// half-open range `[low, high)`, which is empty when
    /// `low == high` and would panic at runtime; if you want no
    /// jitter, use a custom [`crate::JitterRng`] impl that returns
    /// a constant (see [`crate::testing::FixedJitter`]).
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
    /// Consumes `self` so the worker cannot be accidentally reused
    /// after shutdown. Returns `Ok(())` on clean shutdown, or
    /// propagates the first [`PgError`] from a drain pass (the
    /// loop does NOT swallow database errors; a permanent DB
    /// problem stops the worker so a supervisor - PR 4 - can
    /// decide whether to restart it).
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) -> Result<(), PgError> {
        loop {
            if *shutdown.borrow() {
                return Ok(());
            }
            let outcomes = process_available_outbox_rows(
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
            let sleep_dur = self.smart_sleep_duration(&outcomes, base_dur).await?;
            tokio::select! {
                _ = self.clock.sleep_for(sleep_dur) => {}
                changed = shutdown.changed() => {
                    match changed {
                        // The shutdown value changed - return iff
                        // it changed to `true`.
                        Ok(()) => {
                            if *shutdown.borrow() {
                                return Ok(());
                            }
                        }
                        // All senders dropped. No further shutdown
                        // signals possible; the caller has lost the
                        // ability to stop us through this channel,
                        // so termination is the only safe option.
                        // Returning Ok(()) treats this as a clean
                        // shutdown (the channel close is the
                        // signal); returning Err would imply the
                        // worker hit a database problem, which it
                        // did not.
                        Err(_) => return Ok(()),
                    }
                }
            }
        }
    }

    /// Decide how long to sleep before the next drain pass.
    ///
    /// Default is the jittered base interval. The smart-sleep
    /// shortcut applies ONLY when the drain just returned no
    /// terminal work (`outcomes` is empty or contains only
    /// `TransientRetry` outcomes, both of which indicate "no due
    /// work to deliver right now"). In that case we ask
    /// [`earliest_pending_retry`] for the soonest future
    /// `next_attempt_at`; if it is sooner than `base_dur`, the
    /// worker sleeps until that instant instead, so a scheduled
    /// retry becomes claimable as soon as it is due.
    ///
    /// The base interval remains the ceiling: even if the next
    /// retry is hours away, we still wake up after `base_dur` to
    /// catch newly-enqueued immediately-due rows.
    async fn smart_sleep_duration(
        &self,
        outcomes: &[ProcessOutcome],
        base_dur: Duration,
    ) -> Result<Duration, PgError> {
        let only_deferred = outcomes
            .iter()
            .all(|o| matches!(o, ProcessOutcome::TransientRetry { .. }));
        if !only_deferred {
            return Ok(base_dur);
        }
        let Some(next) = earliest_pending_retry(&self.pool, &self.intent_type).await? else {
            return Ok(base_dur);
        };
        let now = self.clock.now();
        let until = match (next - now).to_std() {
            Ok(d) => d,
            Err(_) => return Ok(base_dur),
        };
        Ok(std::cmp::min(base_dur, until))
    }
}
