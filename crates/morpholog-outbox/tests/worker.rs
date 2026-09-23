//! Integration tests for [`morpholog_outbox::OutboxWorker`].
//!
//! [`MockClock`] records sleeps without sleeping and [`FixedJitter`] fixes the factor, so the
//! tests check the requested sleep durations deterministically and in no real time.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{commit_simple_entry, reset_db, test_pool};

use std::sync::Arc;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use jiff_sqlx::ToSqlx;
use morpholog_outbox::OutboxWorker;
use morpholog_outbox::testing::{FixedJitter, MockClock};
use morpholog_postgres::{Deliverer, DeliveryOutcome, OutboxRow, testing::AlwaysDelivers};
use tokio::sync::watch;

// ============================================================
// Test infrastructure
// ============================================================

const INTENT_TYPE: &str = "JournalEntryPosted";

/// Delivers every row, and signals shutdown on the first, so the worker stops after one pass.
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
            // The worker only sees this after the current drain pass finishes.
            let _ = self.shutdown.send(true);
        }
        DeliveryOutcome::Delivered
    }
}

// ============================================================
// Tests
// ============================================================

#[tokio::test]
async fn worker_returns_immediately_when_shutdown_is_set_at_start() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let (shutdown_tx, shutdown_rx) = watch::channel(true);
    drop(shutdown_tx); // not modified after this

    let clock = MockClock::new(Timestamp::now());
    let worker = OutboxWorker::new(
        pool,
        "worker_a",
        INTENT_TYPE,
        AlwaysDelivers,
        clock,
        FixedJitter::new(1.0),
    );
    worker.run(shutdown_rx).await.unwrap();
    // Passing means `run` returned at all.
}

#[tokio::test]
async fn worker_drains_pending_rows_and_then_observes_shutdown() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_simple_entry(&pool, "entry_001", "p_worker").await;
    commit_simple_entry(&pool, "entry_002", "p_worker").await;

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let shutdown_tx = Arc::new(shutdown_tx);
    let clock = MockClock::new(Timestamp::now());
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

    // How many sleeps depends on shutdown timing; the first is base_interval * factor.
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
    let clock = MockClock::new(Timestamp::now());

    let worker = OutboxWorker::new(
        pool,
        "worker_a",
        INTENT_TYPE,
        AlwaysDelivers,
        clock.clone(),
        FixedJitter::new(1.25),
    )
    .with_base_interval(Duration::from_millis(80));

    // Mock sleeps resolve at once, so yielding lets the worker loop several times.
    let handle = tokio::spawn(worker.run(shutdown_rx));
    tokio::task::yield_now().await;
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    shutdown_tx_for_task.send(true).unwrap();
    handle.await.unwrap().unwrap();

    let sleeps = clock.sleeps();
    assert!(!sleeps.is_empty(), "worker must have recorded sleeps");
    let expected = Duration::from_millis(80).mul_f64(1.25);
    for s in &sleeps {
        assert_eq!(*s, expected);
    }
}

#[tokio::test]
async fn two_workers_concurrent_do_not_double_claim_a_row() {
    let pool = test_pool().await;
    reset_db(&pool).await;
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
        MockClock::new(Timestamp::now()),
        FixedJitter::new(1.0),
    )
    .with_base_interval(Duration::from_millis(20));
    let worker_b = OutboxWorker::new(
        pool.clone(),
        "worker_b",
        INTENT_TYPE,
        AlwaysDelivers,
        MockClock::new(Timestamp::now()),
        FixedJitter::new(1.0),
    )
    .with_base_interval(Duration::from_millis(20));

    let handle_a = tokio::spawn(worker_a.run(shutdown_rx_a));
    let handle_b = tokio::spawn(worker_b.run(shutdown_rx_b));
    // Let both workers complete at least one drain pass.
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    shutdown_tx.send(true).unwrap();
    handle_a.await.unwrap().unwrap();
    handle_b.await.unwrap().unwrap();

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
    // One row, retry due in 30s: the sleep should be ~30s, not the 5-minute base interval.
    // The offsets are wide enough that a slow machine cannot race past the retry.
    commit_simple_entry(&pool, "entry_001", "p_worker").await;
    let future_retry = Timestamp::now() + SignedDuration::from_secs(30);
    sqlx::query("UPDATE morpholog.outbox SET next_attempt_at=$1 WHERE intent_type=$2")
        .bind(future_retry.to_sqlx())
        .bind(INTENT_TYPE)
        .execute(&pool)
        .await
        .unwrap();

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let clock = MockClock::new(Timestamp::now());

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
    // Wait for the first recorded sleep rather than a fixed number of yields.
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
    // Below 60s rather than exactly 30s, to absorb the gap between building the clock and
    // setting the retry time.
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
    // Empty outbox, so no retry is pending and the sleep is base_interval * factor.
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let clock = MockClock::new(Timestamp::now());

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
    let clock = MockClock::new(Timestamp::now());

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
    // Let the worker reach the select! on shutdown.changed().
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    // With the only sender gone, changed() returns Err at once on every call. A worker that
    // ignored it would spin forever, so it must stop.
    drop(shutdown_tx);

    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("worker did not terminate after shutdown channel closed")
        .unwrap()
        .unwrap();
}

#[tokio::test]
#[should_panic(expected = "base_interval must be > 0")]
async fn with_base_interval_panics_on_zero() {
    // connect_lazy never opens a connection, and the panic comes before any database use.
    let _ = OutboxWorker::new(
        sqlx::PgPool::connect_lazy("postgres:///does_not_matter")
            .expect("lazy connect cannot fail"),
        "worker_a",
        INTENT_TYPE,
        AlwaysDelivers,
        MockClock::new(Timestamp::now()),
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
        MockClock::new(Timestamp::now()),
        FixedJitter::new(1.0),
    )
    .with_jitter(0.5, 0.5);
}
