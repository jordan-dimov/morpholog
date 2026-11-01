//! Migration 027 killed at its worst moments, through the real binary.
//! Each test makes `morpholog migrate` block at a chosen statement by
//! holding a lock from the test's own session, waits until PostgreSQL
//! shows the child's backend waiting on it, kills the child with SIGKILL,
//! releases the lock and waits for the backend to go, then reads what
//! survived. Attacker capability modelled: none; the adversary is the
//! operator's host dying, or the connection with it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use sqlx::{Connection as _, PgConnection, PgPool};
use uuid::Uuid;

const BASELINE_SCHEMA: &str =
    include_str!("../../morpholog-postgres/tests/fixtures/schema_v0.0.14.sql");

fn with_database(url: &str, name: &str) -> String {
    let (base, query) = url
        .split_once('?')
        .map_or((url, None), |(b, q)| (b, Some(q)));
    let swapped = match base.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/{name}"),
        None => base.to_string(),
    };
    match query {
        Some(q) => format!("{swapped}?{q}"),
        None => swapped,
    }
}

/// A database at the v0.0.14 baseline, its own, with the admin pool that
/// drops it.
struct Scratch {
    pool: PgPool,
    admin: PgPool,
    name: String,
    url: String,
}

async fn scratch(tag: &str) -> Option<Scratch> {
    let base = std::env::var("DATABASE_URL").ok()?;
    let name = format!("morpholog_crash_{tag}_{}", std::process::id());
    let admin = PgPool::connect(&morpholog_postgres::with_default_user(&with_database(
        &base, "postgres",
    )))
    .await
    .unwrap();
    ddl(
        &admin,
        &format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
    )
    .await;
    ddl(&admin, &format!("CREATE DATABASE {name}")).await;
    let url = with_database(&base, &name);
    let pool = PgPool::connect(&morpholog_postgres::with_default_user(&url))
        .await
        .unwrap();
    sqlx::raw_sql(BASELINE_SCHEMA)
        .execute(&pool)
        .await
        .expect("the baseline schema");
    Some(Scratch {
        pool,
        admin,
        name,
        url,
    })
}

impl Scratch {
    async fn drop(self) {
        self.pool.close().await;
        ddl(
            &self.admin,
            &format!("DROP DATABASE IF EXISTS {} WITH (FORCE)", self.name),
        )
        .await;
    }

    async fn connection(&self) -> PgConnection {
        PgConnection::connect(&morpholog_postgres::with_default_user(&self.url))
            .await
            .unwrap()
    }
}

async fn ddl(pool: &PgPool, sql: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(pool)
        .await
        .unwrap();
}

/// The statement a binary before the canonical order ran for one commit
/// admitting `X(k)`, and the claim it left, so the legacy history and the
/// table agree and `audit verify` can pass.
const LEGACY_INSERT: &str = "WITH row AS (
    INSERT INTO morpholog.audit (
        transition_id, transformation_name, arguments, actor, invariant_epoch,
        invariants_checked, asserted_claims, retracted_claims, emitted_intents,
        committed_at, attestation, parameters, model_hash, semantics_version,
        drawn_subjects)
    VALUES ($1, 'admit_x', jsonb_build_array(jsonb_build_object('type', 'subject', 'value', $2::text)),
        '{\"type\": \"subject\", \"value\": \"w\"}', 1, '[]',
        jsonb_build_array(jsonb_build_object('predicate', 'X', 'args',
            jsonb_build_array(jsonb_build_object('type', 'subject', 'value', $2::text)))),
        '[]', '[]', timestamptz '2026-06-01T00:00:00Z' + $3 * interval '1 second',
        '{\"mode\": \"gateway\", \"authenticated_by\": \"w\"}', '[\"k\"]',
        'sha256:' || repeat('d', 64), 1, '[]')
    RETURNING transition_id)
 INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
 SELECT 'X', jsonb_build_array(jsonb_build_object('type', 'subject', 'value', $2::text)), transition_id
 FROM row";

async fn legacy_rows(pool: &PgPool, from: u32, count: u32) {
    for i in from..from + count {
        legacy_row(pool, i).await;
    }
}

async fn legacy_row<'e, E>(executor: E, i: u32)
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    sqlx::query(LEGACY_INSERT)
        .bind(Uuid::now_v7())
        .bind(format!("k{i:06}"))
        .bind(f64::from(i))
        .execute(executor)
        .await
        .unwrap();
}

fn migrate(url: &str, prepare: bool) -> Command {
    let mut command = Command::new(common::bin());
    command.arg("migrate");
    if prepare {
        command.arg("--prepare");
    }
    command
        .args(["--database-url", url])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

fn run(url: &str, prepare: bool) -> bool {
    migrate(url, prepare).status().unwrap().success()
}

fn cli_json(url: &str, args: &[&str]) -> serde_json::Value {
    let output = Command::new(common::bin())
        .args(args)
        .args(["--database-url", url])
        .output()
        .unwrap();
    serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        panic!(
            "{args:?}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

const PATIENCE: Duration = Duration::from_secs(60);

/// The pid of the one backend of this database blocked on a lock inside a
/// statement matching `pattern`, once there is one.
async fn wait_blocked(conn: &mut PgConnection, pattern: &str) -> i32 {
    let started = Instant::now();
    loop {
        let pids: Vec<i32> = sqlx::query_scalar(
            "SELECT pid FROM pg_stat_activity
             WHERE datname = current_database() AND wait_event_type = 'Lock'
               AND query ILIKE $1 AND cardinality(pg_blocking_pids(pid)) > 0",
        )
        .bind(pattern)
        .fetch_all(&mut *conn)
        .await
        .unwrap();
        if let [pid] = pids[..] {
            return pid;
        }
        assert!(
            started.elapsed() < PATIENCE,
            "no backend blocked on {pattern:?} within {PATIENCE:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Kill the child, then wait for its backend to leave once `release`
/// has let it go on: a backend waiting on a lock learns of its dead
/// client only when it next writes to it.
async fn kill(child: &mut Child, conn: &mut PgConnection, pid: i32, release: &mut PgConnection) {
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert!(!status.success(), "the child was killed: {status}");
    sqlx::raw_sql("ROLLBACK")
        .execute(&mut *release)
        .await
        .unwrap();
    let started = Instant::now();
    loop {
        let alive: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE pid = $1")
            .bind(pid)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        if alive == 0 {
            return;
        }
        assert!(
            started.elapsed() < PATIENCE,
            "backend {pid} still present {PATIENCE:?} after its client died"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

struct Seen {
    version: i32,
    first_order: Vec<(String, i32)>,
    progress_rows: i64,
    sequence: (i64, bool),
}

async fn seen(pool: &PgPool) -> Seen {
    let version: i32 = sqlx::query_scalar("SELECT max(version) FROM morpholog.schema_migrations")
        .fetch_one(pool)
        .await
        .unwrap();
    let first_order = sqlx::query_as(
        "SELECT transformation_name, act_ordinal FROM morpholog.audit
         WHERE audit_order = 1 ORDER BY act_ordinal",
    )
    .fetch_all(pool)
    .await
    .unwrap();
    let progress_rows = sqlx::query_scalar("SELECT count(*) FROM morpholog.migration_progress")
        .fetch_one(pool)
        .await
        .unwrap();
    let sequence = sqlx::query_as("SELECT last_value, is_called FROM morpholog.audit_order_seq")
        .fetch_one(pool)
        .await
        .unwrap();
    Seen {
        version,
        first_order,
        progress_rows,
        sequence,
    }
}

async fn record_arguments(pool: &PgPool) -> serde_json::Value {
    sqlx::query_scalar(
        "SELECT arguments FROM morpholog.audit WHERE transformation_name = 'morpholog.activate'",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn frontier_size(pool: &PgPool) -> u64 {
    sqlx::query_scalar::<_, serde_json::Value>(
        "SELECT detail FROM morpholog.migration_progress WHERE step = 'frontier_state'",
    )
    .fetch_one(pool)
    .await
    .unwrap()["size"]
        .as_u64()
        .unwrap()
}

fn one_activation(first_order: &[(String, i32)]) -> bool {
    first_order
        == [
            ("morpholog.baseline".to_string(), 0),
            ("morpholog.activate".to_string(), 1),
        ]
}

/// The cutover dies with the activation written in its transaction and
/// the migration record's insert blocked, twice over; each time nothing
/// of it stays, the preparation is intact, and the order sequence is
/// where the cutover restarts it, so the third attempt activates at
/// order 1. The sequence is why this is a gate: a sequence does not roll
/// back, and before the cutover restarted it the second attempt drew
/// order 2 and was refused for good.
#[tokio::test]
async fn an_interrupted_cutover_rolls_back_and_the_next_attempt_activates() {
    let Some(db) = scratch("cutover").await else {
        return;
    };
    legacy_rows(&db.pool, 0, 300).await;
    assert!(run(&db.url, true), "prepare");
    let before = seen(&db.pool).await;
    assert_eq!((before.version, before.progress_rows), (26, 6));

    for attempt in 1..=2 {
        let mut holder = db.connection().await;
        sqlx::raw_sql("BEGIN; LOCK TABLE morpholog.schema_migrations IN EXCLUSIVE MODE")
            .execute(&mut holder)
            .await
            .unwrap();
        let mut child = migrate(&db.url, false).spawn().unwrap();
        let mut watch = db.connection().await;
        let pid = wait_blocked(&mut watch, "INSERT INTO morpholog.schema_migrations%").await;
        // Written in the cutover's transaction, visible to nobody else.
        assert!(
            seen(&db.pool).await.first_order.is_empty(),
            "attempt {attempt}"
        );
        kill(&mut child, &mut watch, pid, &mut holder).await;
        let after = seen(&db.pool).await;
        assert_eq!(
            after.version, 26,
            "attempt {attempt}: migration 27 did not land"
        );
        assert!(
            after.first_order.is_empty(),
            "attempt {attempt}: no row of order 1 stays"
        );
        assert_eq!(
            after.progress_rows, 6,
            "attempt {attempt}: the preparation is intact"
        );
        assert_eq!(
            after.sequence,
            (1, false),
            "attempt {attempt}: the order is not drawn"
        );
    }

    assert!(run(&db.url, false), "the third attempt");
    let after = seen(&db.pool).await;
    assert_eq!(after.version, morpholog_postgres::head_version());
    assert!(
        one_activation(&after.first_order),
        "{:?}",
        after.first_order
    );
    assert_eq!(after.progress_rows, 0);
    assert_eq!(record_arguments(&db.pool).await[0]["value"], "300");
    let verify = cli_json(&db.url, &["audit", "verify"]);
    assert_eq!(verify["passes"], true, "{verify}");
    db.drop().await;
}

/// The cutover commits, then the client dies before it finishes: the
/// activation stands, and running `migrate` again does nothing to it.
#[tokio::test]
async fn a_client_lost_after_the_commit_leaves_one_activation_and_a_harmless_rerun() {
    let Some(db) = scratch("committed").await else {
        return;
    };
    legacy_rows(&db.pool, 0, 50).await;
    assert!(run(&db.url, true), "prepare");

    let mut before_commit = db.connection().await;
    sqlx::raw_sql("BEGIN; LOCK TABLE morpholog.schema_migrations IN EXCLUSIVE MODE")
        .execute(&mut before_commit)
        .await
        .unwrap();
    let mut child = migrate(&db.url, false).spawn().unwrap();
    let mut watch = db.connection().await;
    let pid = wait_blocked(&mut watch, "INSERT INTO morpholog.schema_migrations%").await;
    // After the commit the runner re-applies the privilege floor, which
    // reads the deployment roles: hold that table, then let the commit go.
    let mut after_commit = db.connection().await;
    sqlx::raw_sql("BEGIN; LOCK TABLE morpholog.deployment_roles IN ACCESS EXCLUSIVE MODE")
        .execute(&mut after_commit)
        .await
        .unwrap();
    sqlx::raw_sql("ROLLBACK")
        .execute(&mut before_commit)
        .await
        .unwrap();
    let pid_after = wait_blocked(&mut watch, "%FROM morpholog.deployment_roles%").await;
    assert_eq!(pid_after, pid, "the same backend, past its commit");
    let committed = seen(&db.pool).await;
    assert_eq!(
        committed.version,
        morpholog_postgres::head_version(),
        "the cutover committed, and the migrations after it"
    );
    assert!(one_activation(&committed.first_order));
    kill(&mut child, &mut watch, pid, &mut after_commit).await;

    let after = seen(&db.pool).await;
    assert_eq!(after.version, morpholog_postgres::head_version());
    assert!(
        one_activation(&after.first_order),
        "{:?}",
        after.first_order
    );
    let rerun = cli_json(&db.url, &["migrate"]);
    assert_eq!(rerun["applied"], serde_json::json!([]), "{rerun}");
    assert_eq!(
        rerun["recorded_version_after"],
        morpholog_postgres::head_version(),
        "{rerun}"
    );
    assert!(one_activation(&seen(&db.pool).await.first_order));
    db.drop().await;
}

/// The cutover dies while waiting for a writer that already wrote: the
/// writer's row lands in the legacy prefix the next attempt records.
#[tokio::test]
async fn a_cutover_killed_waiting_for_a_writer_leaves_nothing_and_the_writer_lands() {
    let Some(db) = scratch("writer").await else {
        return;
    };
    legacy_rows(&db.pool, 0, 40).await;
    assert!(run(&db.url, true), "prepare");

    let mut writer = db.connection().await;
    sqlx::raw_sql("BEGIN").execute(&mut writer).await.unwrap();
    legacy_row(&mut writer, 40).await;
    let mut child = migrate(&db.url, false).spawn().unwrap();
    let mut watch = db.connection().await;
    let pid = wait_blocked(&mut watch, "%LOCK TABLE morpholog.claims, morpholog.audit%").await;
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    sqlx::raw_sql("COMMIT").execute(&mut writer).await.unwrap();
    let started = Instant::now();
    while sqlx::query_scalar::<_, i64>("SELECT count(*) FROM pg_stat_activity WHERE pid = $1")
        .bind(pid)
        .fetch_one(&mut watch)
        .await
        .unwrap()
        > 0
    {
        assert!(started.elapsed() < PATIENCE, "backend {pid} still present");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let after = seen(&db.pool).await;
    assert_eq!((after.version, after.first_order.len()), (26, 0));

    assert!(run(&db.url, false), "the next attempt");
    assert_eq!(record_arguments(&db.pool).await[0]["value"], "41");
    let (rows, root) = morpholog_postgres::legacy_root_full_scan(&db.pool)
        .await
        .unwrap();
    assert_eq!(rows, 41);
    assert_eq!(record_arguments(&db.pool).await[1]["value"], root);
    db.drop().await;
}

/// A frontier pass dies with a page hashed and its save blocked. The
/// save is one autocommit statement, so once the lock goes it completes
/// on its own and the state stands at a page boundary, with every page
/// after it lost; the next pass goes on from there to the root a scan
/// from nothing gives.
#[tokio::test]
async fn an_interrupted_frontier_pass_resumes_from_its_last_saved_page() {
    let Some(db) = scratch("frontier").await else {
        return;
    };
    legacy_rows(&db.pool, 0, 1500).await;
    assert!(run(&db.url, true), "the first preparation");
    assert_eq!(frontier_size(&db.pool).await, 1500);
    legacy_rows(&db.pool, 1500, 3000).await;

    let mut holder = db.connection().await;
    sqlx::raw_sql(
        "BEGIN; SELECT 1 FROM morpholog.migration_progress
         WHERE version = 27 AND step = 'frontier_state' FOR UPDATE",
    )
    .execute(&mut holder)
    .await
    .unwrap();
    let mut child = migrate(&db.url, true).spawn().unwrap();
    let mut watch = db.connection().await;
    let pid = wait_blocked(&mut watch, "INSERT INTO morpholog.migration_progress%").await;
    kill(&mut child, &mut watch, pid, &mut holder).await;
    let saved = frontier_size(&db.pool).await;
    assert!(
        saved < 4500 && (saved - 1500) % 1024 == 0,
        "the state stands at a page boundary short of the end: {saved}"
    );

    assert!(run(&db.url, true), "the second preparation");
    assert_eq!(frontier_size(&db.pool).await, 4500);
    assert!(run(&db.url, false), "the cutover");
    let (rows, root) = morpholog_postgres::legacy_root_full_scan(&db.pool)
        .await
        .unwrap();
    let arguments = record_arguments(&db.pool).await;
    assert_eq!(
        (rows, arguments[0]["value"].as_str().unwrap()),
        (4500, "4500")
    );
    assert_eq!(arguments[1]["value"], root);
    db.drop().await;
}

/// A concurrent index build is cut off while it waits for an old
/// snapshot: the index it leaves is invalid, and the next preparation
/// drops and rebuilds it.
#[tokio::test]
async fn an_interrupted_index_build_is_repaired() {
    let Some(db) = scratch("index").await else {
        return;
    };
    legacy_rows(&db.pool, 0, 20).await;
    // A snapshot older than the build's, held without a lock on the table
    // (a lock would hold up the column step before it) is what the build
    // waits for in its last phase.
    let mut old_snapshot = db.connection().await;
    sqlx::raw_sql("BEGIN ISOLATION LEVEL REPEATABLE READ; SELECT 1")
        .execute(&mut old_snapshot)
        .await
        .unwrap();
    let mut child = migrate(&db.url, true).spawn().unwrap();
    let mut watch = db.connection().await;
    let started = Instant::now();
    let (pid, index) = loop {
        let building: Vec<(i32, String, String)> = sqlx::query_as(
            "SELECT p.pid, p.phase, c.relname FROM pg_stat_progress_create_index p
             JOIN pg_stat_activity a USING (pid) JOIN pg_class c ON c.oid = p.index_relid
             WHERE a.datname = current_database()",
        )
        .fetch_all(&mut watch)
        .await
        .unwrap();
        if let Some((pid, _, index)) = building
            .into_iter()
            .find(|(_, phase, _)| phase == "waiting for old snapshots")
        {
            break (pid, index);
        }
        assert!(
            started.elapsed() < PATIENCE,
            "no index build waited for the old snapshot"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    // The connection goes, as a host dying takes it, mid-build.
    let terminated: bool = sqlx::query_scalar("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .fetch_one(&mut watch)
        .await
        .unwrap();
    assert!(terminated);
    assert!(!child.wait().unwrap().success(), "the preparation failed");
    sqlx::raw_sql("ROLLBACK")
        .execute(&mut old_snapshot)
        .await
        .unwrap();
    let valid: Option<bool> = sqlx::query_scalar(
        "SELECT i.indisvalid FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
         WHERE c.relname = $1",
    )
    .bind(&index)
    .fetch_optional(&db.pool)
    .await
    .unwrap();
    assert_eq!(valid, Some(false), "{index} is left invalid");

    assert!(run(&db.url, true), "the second preparation");
    let valid: Vec<bool> = sqlx::query_scalar(
        "SELECT i.indisvalid FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
         WHERE c.relname IN ('audit_one_activation', 'audit_canonical') ORDER BY c.relname",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(valid, [true, true]);
    let check = cli_json(&db.url, &["migrate", "--check"]);
    assert_eq!(check["activation_preflight"]["state"], "ready", "{check}");
    db.drop().await;
}
