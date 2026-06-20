//! Integration tests for [`morpholog_outbox::OutboxWorker`].
//!
//! Timing assertions use [`MockClock`] and [`FixedJitter`] so the
//! tests are deterministic: the clock never sleeps, and the
//! jitter factor is a configured constant. The mock clock records
//! every `sleep_for` call into an inspectable buffer; we assert
//! the worker tried to sleep the expected jittered duration
//! (`base_interval * factor`) without burning real time.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{commit_simple_entry, reset_db, test_pool};

use std::sync::Arc;
use std::time::Duration;

use chrono::{Duration as ChronoDuration, Utc};
use morpholog_outbox::OutboxWorker;
use morpholog_outbox::testing::{FixedJitter, MockClock};
use morpholog_postgres::{Deliverer, DeliveryOutcome, OutboxRow, testing::AlwaysDelivers};
use tokio::sync::watch;

// ============================================================
// Test infrastructure
// ============================================================

const INTENT_TYPE: &str = "JournalEntryPosted";

/// Deliverer that, after delivering its first row successfully,
/// signals shutdown via the supplied watch sender. Tests use this
/// to bound the worker's run after a single drain has done its
/// work, without depending on real wall time.
struct ShutdownAfterFirstDelivery {
    shutdown: Arc<watch::Sender<bool>>,
    call_count: std::sync::atomic::AtomicU32,
}

impl Deliverer for ShutdownAfterFirstDelivery {
    async fn deliver(&self, _row: &OutboxRow) -> DeliveryOutcome {
        let prior = self
            .call_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if prior == 0 {
            // Sent on the first call but the drain keeps going
            // through any further claimable rows; shutdown is
            // observed by the worker AFTER the current drain pass
            // and AFTER the post-drain sleep race resolves.
            let _ = self.shutdown.send(true);
        }
        DeliveryOutcome::Delivered
    }
}

// AlwaysDelivers lives in `morpholog_postgres::testing` (imported
// above). Test-file-local shapes that need worker-state access
// (ShutdownAfterFirstDelivery) stay above.

// ============================================================
// Tests
// ============================================================

#[tokio::test]
async fn worker_returns_immediately_when_shutdown_is_set_at_start() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (shutdown_tx, shutdown_rx) = watch::channel(true);
    drop(shutdown_tx); // not modified after this

    let clock = MockClock::new(Utc::now());
    let worker = OutboxWorker::new(
        pool,
        "worker_a",
        INTENT_TYPE,
        AlwaysDelivers,
        clock,
        FixedJitter::new(1.0),
    );
    worker.run(shutdown_rx).await.unwrap();
    // The test passes by virtue of run returning at all; an
    // unfortunate bug would block here forever.
}

#[tokio::test]
async fn worker_drains_pending_rows_and_then_observes_shutdown() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_simple_entry(&pool, "entry_001", "p_worker").await;
    commit_simple_entry(&pool, "entry_002", "p_worker").await;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let shutdown_tx = Arc::new(shutdown_tx);
    let clock = MockClock::new(Utc::now());
    let worker = OutboxWorker::new(
        pool.clone(),
        "worker_a",
        INTENT_TYPE,
        ShutdownAfterFirstDelivery {
            shutdown: shutdown_tx.clone(),
            call_count: 0.into(),
        },
        clock.clone(),
        FixedJitter::new(1.0),
    )
    .with_base_interval(Duration::from_millis(50));

    worker.run(shutdown_rx).await.unwrap();

    // Both rows were delivered before the worker exited.
    let pending: (i64,) =
        sqlx::query_as("SELECT count(*) FROM morpholog.outbox WHERE status='pending'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pending.0, 0);
    let delivered: (i64,) =
        sqlx::query_as("SELECT count(*) FROM morpholog.outbox WHERE status='delivered'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(delivered.0, 2);

    // The worker tried to sleep at least once (after the drain
    // pass) before observing shutdown. The exact count depends on
    // shutdown timing, but the recorded duration should match
    // base_interval * jitter_factor.
    let sleeps = clock.sleeps();
    assert!(
        !sleeps.is_empty(),
        "worker must have called sleep_for at least once after the drain"
    );
    assert_eq!(
        sleeps[0],
        Duration::from_millis(50),
        "sleep_for must request base_interval * jitter_factor (50ms * 1.0)"
    );
}

#[tokio::test]
async fn worker_applies_jitter_factor_to_base_interval() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let shutdown_tx_for_task = shutdown_tx.clone();
    let clock = MockClock::new(Utc::now());

    let worker = OutboxWorker::new(
        pool,
        "worker_a",
        INTENT_TYPE,
        AlwaysDelivers,
        clock.clone(),
        FixedJitter::new(1.25),
    )
    .with_base_interval(Duration::from_millis(80));

    // Spawn the worker; it will drain (no rows), then sleep, then
    // attempt to read the (mock, instantly-resolving) sleep and
    // loop. After a few iterations we trigger shutdown.
    let handle = tokio::spawn(worker.run(shutdown_rx));
    tokio::task::yield_now().await;
    // Let the worker run several iterations. Because MockClock's
    // sleep_for resolves immediately, each iteration is just one
    // drain + record-the-sleep. We yield several times to let the
    // scheduler advance the worker.
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    shutdown_tx_for_task.send(true).unwrap();
    handle.await.unwrap().unwrap();

    let sleeps = clock.sleeps();
    assert!(!sleeps.is_empty(), "worker must have recorded sleeps");
    // Every recorded sleep should be base_interval * jitter_factor.
    let expected = Duration::from_millis(80).mul_f64(1.25);
    for s in &sleeps {
        assert_eq!(*s, expected);
    }
}

#[tokio::test]
async fn two_workers_concurrent_do_not_double_claim_a_row() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    // Pre-load enough rows that both workers can make progress.
    for i in 0..6 {
        commit_simple_entry(&pool, &format!("entry_{i:03}"), "p_worker").await;
    }

    let (shutdown_tx, shutdown_rx_a) = watch::channel(false);
    let shutdown_rx_b = shutdown_rx_a.clone();
    let shutdown_tx = Arc::new(shutdown_tx);

    let worker_a = OutboxWorker::new(
        pool.clone(),
        "worker_a",
        INTENT_TYPE,
        AlwaysDelivers,
        MockClock::new(Utc::now()),
        FixedJitter::new(1.0),
    )
    .with_base_interval(Duration::from_millis(20));
    let worker_b = OutboxWorker::new(
        pool.clone(),
        "worker_b",
        INTENT_TYPE,
        AlwaysDelivers,
        MockClock::new(Utc::now()),
        FixedJitter::new(1.0),
    )
    .with_base_interval(Duration::from_millis(20));

    let handle_a = tokio::spawn(worker_a.run(shutdown_rx_a));
    let handle_b = tokio::spawn(worker_b.run(shutdown_rx_b));
    // Yield enough times that both workers complete at least one
    // drain pass against the pre-loaded rows.
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    shutdown_tx.send(true).unwrap();
    handle_a.await.unwrap().unwrap();
    handle_b.await.unwrap().unwrap();

    // All six rows are delivered exactly once. The total count of
    // delivered rows equals the count of rows; no row was
    // processed by both workers (which would have shown up as a
    // double-update on `delivered_at` - but PG's row-level lock
    // via SKIP LOCKED prevents the second worker from claiming
    // the same row at all).
    let delivered: (i64,) =
        sqlx::query_as("SELECT count(*) FROM morpholog.outbox WHERE status='delivered'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(delivered.0, 6);
    let pending: (i64,) =
        sqlx::query_as("SELECT count(*) FROM morpholog.outbox WHERE status='pending'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(pending.0, 0);
}

#[tokio::test]
async fn worker_smart_sleeps_until_soonest_next_attempt_at_when_no_work_is_due() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    // Commit a row, then set its `next_attempt_at` 30 seconds in
    // the future. The drain will return empty (the row is not due
    // yet); smart sleep should clamp the post-drain sleep to ~30s
    // rather than the 5-minute base interval.
    //
    // Generous offsets: 30s + 5min base + shutdown-after-first-
    // sleep means the test cannot race past next_attempt_at even
    // on a slow CI box.
    commit_simple_entry(&pool, "entry_001", "p_worker").await;
    let future_retry = Utc::now() + ChronoDuration::seconds(30);
    sqlx::query("UPDATE morpholog.outbox SET next_attempt_at=$1 WHERE intent_type=$2")
        .bind(future_retry)
        .bind(INTENT_TYPE)
        .execute(&pool)
        .await
        .unwrap();

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let clock = MockClock::new(Utc::now());

    let worker = OutboxWorker::new(
        pool,
        "worker_a",
        INTENT_TYPE,
        AlwaysDelivers,
        clock.clone(),
        FixedJitter::new(1.0),
    )
    .with_base_interval(Duration::from_secs(300));

    let handle = tokio::spawn(worker.run(shutdown_rx));
    // Poll the mock clock until the worker records its first
    // sleep, then shut it down. This avoids the race the earlier
    // version had (waiting a fixed number of yields and hoping
    // the worker got past drain + smart-sleep computation in
    // time).
    loop {
        if !clock.sleeps().is_empty() {
            break;
        }
        tokio::task::yield_now().await;
    }
    shutdown_tx.send(true).unwrap();
    handle.await.unwrap().unwrap();

    let sleeps = clock.sleeps();
    assert!(!sleeps.is_empty(), "worker must have recorded sleeps");
    // The first recorded sleep is the smart-sleep value: must be
    // clamped well below the 5-minute base_interval. The clamp
    // target is ~30s; we assert below 60s to absorb the small gap
    // between MockClock construction and the next_attempt_at row
    // update without making the assertion meaningless.
    assert!(
        sleeps[0] < Duration::from_secs(60),
        "first smart sleep must clamp below base_interval (300s) to roughly \
         next_attempt_at (~30s); got {:?}",
        sleeps[0]
    );
}

#[tokio::test]
async fn worker_uses_base_interval_when_no_pending_retries_exist() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    // Empty outbox: drain returns nothing; smart sleep finds no
    // pending retry; worker falls back to base_interval *
    // jitter_factor.

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let clock = MockClock::new(Utc::now());

    let worker = OutboxWorker::new(
        pool,
        "worker_a",
        INTENT_TYPE,
        AlwaysDelivers,
        clock.clone(),
        FixedJitter::new(1.0),
    )
    .with_base_interval(Duration::from_millis(40));

    let handle = tokio::spawn(worker.run(shutdown_rx));
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    shutdown_tx.send(true).unwrap();
    handle.await.unwrap().unwrap();

    let sleeps = clock.sleeps();
    assert!(!sleeps.is_empty());
    for s in &sleeps {
        assert_eq!(
            *s,
            Duration::from_millis(40),
            "with no pending retries, sleep must equal base_interval * jitter_factor"
        );
    }
}

#[tokio::test]
async fn worker_terminates_when_shutdown_channel_closes() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let clock = MockClock::new(Utc::now());

    let worker = OutboxWorker::new(
        pool,
        "worker_a",
        INTENT_TYPE,
        AlwaysDelivers,
        clock,
        FixedJitter::new(1.0),
    )
    .with_base_interval(Duration::from_millis(20));

    let handle = tokio::spawn(worker.run(shutdown_rx));
    // Yield once so the worker reaches the select! and is
    // awaiting shutdown.changed().
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    // Drop the only Sender. The worker's shutdown.changed() must
    // now return Err, which the worker must treat as termination
    // rather than ignoring (the bug Copilot flagged: ignoring Err
    // would cause changed() to be immediately ready every loop and
    // burn CPU forever with no way to stop the worker).
    drop(shutdown_tx);

    // The worker must exit within a generous bound. If it ignored
    // the closed channel, this await would hang and the test
    // harness would time out, not pass.
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("worker did not terminate after shutdown channel closed")
        .unwrap()
        .unwrap();
}

#[tokio::test]
#[should_panic(expected = "base_interval must be > 0")]
async fn with_base_interval_panics_on_zero() {
    // PgPool is not used by the builder; the panic fires
    // before any DB activity. connect_lazy requires a tokio
    // context but never actually opens a connection, so it works
    // here.
    let _ = OutboxWorker::new(
        sqlx::PgPool::connect_lazy("postgres:///does_not_matter")
            .expect("lazy connect cannot fail"),
        "worker_a",
        INTENT_TYPE,
        AlwaysDelivers,
        MockClock::new(Utc::now()),
        FixedJitter::new(1.0),
    )
    .with_base_interval(Duration::ZERO);
}

#[tokio::test]
#[should_panic(expected = "jitter range must be")]
async fn with_jitter_panics_on_equal_bounds() {
    let _ = OutboxWorker::new(
        sqlx::PgPool::connect_lazy("postgres:///does_not_matter")
            .expect("lazy connect cannot fail"),
        "worker_a",
        INTENT_TYPE,
        AlwaysDelivers,
        MockClock::new(Utc::now()),
        FixedJitter::new(1.0),
    )
    .with_jitter(0.5, 0.5);
}
