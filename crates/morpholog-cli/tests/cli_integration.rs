//! End-to-end integration tests for the `morpholog` binary.
//!
//! Unlike the unit tests in `src/main.rs` (which exercise clap argument
//! parsing only), these tests spawn the built binary against a real
//! PostgreSQL database and assert on stdout JSON, stderr error chains,
//! and process exit codes. They cover the dispatch handlers - the
//! `match cli.command` arms in `main`, the `propose` function, the
//! `inspect_derived` function - which are the bulk of the CLI's
//! behaviour and were entirely untested before.
//!
//! The tests share one connection string from `DATABASE_URL` (same
//! convention as `morpholog-postgres` integration tests). Each test
//! truncates the schema before running so they can be executed
//! serially without crosstalk.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

use serde_json::Value;
use sqlx::PgPool;

fn morpholog_bin() -> &'static str {
    env!("CARGO_BIN_EXE_morpholog")
}

fn database_url() -> String {
    std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must be set for morpholog-cli integration tests \
         (e.g. postgres:///morpholog_dev or postgres://postgres:postgres@localhost:5432/postgres)",
    )
}

async fn reset_db() {
    let pool = PgPool::connect(&database_url())
        .await
        .expect("connect to test DB");
    sqlx::query("TRUNCATE morpholog.outbox, morpholog.claims, morpholog.audit CASCADE")
        .execute(&pool)
        .await
        .expect("truncate");
}

/// Run `morpholog` with the given subcommand args plus `--database-url`,
/// returning (status, stdout, stderr). Does not panic on non-zero exit;
/// the caller asserts on what they expect.
fn run_cli(args: &[&str]) -> (std::process::ExitStatus, String, String) {
    let url = database_url();
    let mut command = Command::new(morpholog_bin());
    command.args(args).args(["--database-url", &url]);
    let output = command.output().expect("spawn morpholog binary");
    (
        output.status,
        String::from_utf8(output.stdout).expect("stdout utf8"),
        String::from_utf8(output.stderr).expect("stderr utf8"),
    )
}

/// Issue a balanced journal entry via the CLI's `propose` subcommand.
/// Returns the `transition_id` from the receipt so subsequent tests
/// can use it as an as-of coordinate.
fn post_balanced_entry(entry_id: &str, amount: i64) -> uuid::Uuid {
    let args_json = format!(
        r#"[
            {{"type":"subject","value":"{entry_id}"}},
            {{"type":"subject","value":"2026-04-15"}},
            {{"type":"subject","value":"q1_2026"}},
            {{"type":"subject","value":"account_cash"}},
            {{"type":"subject","value":"account_revenue"}},
            {{"type":"decimal","value":"{amount}"}}
        ]"#
    );
    let (status, stdout, stderr) = run_cli(&[
        "propose",
        "double_entry_ledger",
        "post_simple_entry",
        "--actor",
        "alex",
        "--args",
        &args_json,
    ]);
    assert!(
        status.success(),
        "propose post_simple_entry should succeed; stderr: {stderr}"
    );
    let receipt: Value = serde_json::from_str(&stdout).expect("receipt is JSON");
    assert_eq!(receipt["status"], "committed");
    receipt["transition_id"]
        .as_str()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .expect("receipt carries a transition_id UUID")
}

// ============================================================
// `propose` subcommand
// ============================================================

#[tokio::test(flavor = "current_thread")]
async fn propose_commits_a_balanced_entry_and_emits_committed_receipt() {
    reset_db().await;
    let tid = post_balanced_entry("entry_001", 100);
    assert!(!tid.is_nil());
}

#[tokio::test(flavor = "current_thread")]
async fn propose_unknown_program_errors_with_available_list_on_stderr() {
    reset_db().await;
    let (status, _stdout, stderr) = run_cli(&[
        "propose",
        "not_a_real_program",
        "post_simple_entry",
        "--actor",
        "alex",
        "--args",
        "[]",
    ]);
    assert!(!status.success(), "unknown program must exit non-zero");
    assert!(
        stderr.contains("not_a_real_program"),
        "stderr should name the unknown program: {stderr}"
    );
    assert!(
        stderr.contains("Available built-in programs"),
        "stderr should list available programs: {stderr}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn propose_unknown_transformation_errors_with_available_list_on_stderr() {
    reset_db().await;
    let (status, _stdout, stderr) = run_cli(&[
        "propose",
        "double_entry_ledger",
        "not_a_real_transformation",
        "--actor",
        "alex",
        "--args",
        "[]",
    ]);
    assert!(
        !status.success(),
        "unknown transformation must exit non-zero"
    );
    assert!(
        stderr.contains("not_a_real_transformation"),
        "stderr should name the unknown transformation: {stderr}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn propose_malformed_args_json_errors_to_stderr() {
    reset_db().await;
    let (status, _stdout, stderr) = run_cli(&[
        "propose",
        "double_entry_ledger",
        "post_simple_entry",
        "--actor",
        "alex",
        "--args",
        "not-json",
    ]);
    assert!(!status.success(), "malformed --args must exit non-zero");
    assert!(
        !stderr.is_empty(),
        "stderr should carry an error explanation"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn propose_business_rejection_exits_one_with_rejected_receipt_on_stdout() {
    reset_db().await;
    // Close q1_2026, then attempt to post into the closed period - the
    // require gate rejects.
    let (status, _stdout, _stderr) = run_cli(&[
        "propose",
        "double_entry_ledger",
        "close_period",
        "--actor",
        "alex",
        "--args",
        r#"[{"type":"subject","value":"q1_2026"}]"#,
    ]);
    assert!(status.success(), "close_period should commit");

    let (status, stdout, _stderr) = run_cli(&[
        "propose",
        "double_entry_ledger",
        "post_simple_entry",
        "--actor",
        "alex",
        "--args",
        r#"[
            {"type":"subject","value":"entry_001"},
            {"type":"subject","value":"2026-04-15"},
            {"type":"subject","value":"q1_2026"},
            {"type":"subject","value":"account_cash"},
            {"type":"subject","value":"account_revenue"},
            {"type":"decimal","value":"100"}
        ]"#,
    ]);
    assert!(
        !status.success(),
        "business rejection must exit non-zero (1)"
    );
    let receipt: Value = serde_json::from_str(&stdout).expect("rejection receipt is JSON");
    assert_eq!(receipt["status"], "rejected");
    assert!(receipt["reason"].as_str().unwrap_or("").contains("require"));
}

// ============================================================
// `inspect claims` / `inspect audit` / `inspect outbox`
// ============================================================

#[tokio::test(flavor = "current_thread")]
async fn inspect_claims_returns_admitted_claims_as_json_array() {
    reset_db().await;
    post_balanced_entry("entry_001", 100);

    let (status, stdout, stderr) = run_cli(&["inspect", "claims"]);
    assert!(status.success(), "inspect claims should succeed; {stderr}");
    let claims: Value = serde_json::from_str(&stdout).expect("stdout is JSON");
    let array = claims.as_array().expect("inspect claims returns an array");
    assert!(
        array.iter().any(|c| c["predicate"] == "JournalEntry"),
        "JournalEntry claim should be present after post: {stdout}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_claims_as_of_a_prior_transition_returns_state_at_that_point() {
    reset_db().await;
    let first_tid = post_balanced_entry("entry_001", 100);
    post_balanced_entry("entry_002", 200);

    // Current state has two entries.
    let (_status, stdout, _stderr) = run_cli(&["inspect", "claims"]);
    let claims: Value = serde_json::from_str(&stdout).unwrap();
    let now_count = claims.as_array().unwrap().len();

    // As-of the first transition: only the first entry's claims exist.
    let (status, stdout, stderr) =
        run_cli(&["inspect", "claims", "--as-of", &first_tid.to_string()]);
    assert!(
        status.success(),
        "inspect claims --as-of should succeed; {stderr}"
    );
    let claims_at: Value = serde_json::from_str(&stdout).expect("as-of stdout is JSON");
    let then_count = claims_at.as_array().unwrap().len();
    assert!(
        then_count < now_count,
        "as-of state must be smaller than current state (then={then_count}, now={now_count})"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_audit_returns_one_row_per_committed_transition() {
    reset_db().await;
    post_balanced_entry("entry_001", 100);
    post_balanced_entry("entry_002", 200);

    let (status, stdout, _stderr) = run_cli(&["inspect", "audit"]);
    assert!(status.success());
    let rows: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        rows.as_array().unwrap().len(),
        2,
        "two committed transitions, two audit rows: {stdout}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_outbox_returns_pending_intents_after_commit() {
    reset_db().await;
    post_balanced_entry("entry_001", 100);

    let (status, stdout, _stderr) = run_cli(&["inspect", "outbox"]);
    assert!(status.success());
    let rows: Value = serde_json::from_str(&stdout).unwrap();
    assert!(
        !rows.as_array().unwrap().is_empty(),
        "post_simple_entry emits a JournalEntryPosted intent: {stdout}"
    );
}

// ============================================================
// `inspect derived`
// ============================================================

#[tokio::test(flavor = "current_thread")]
async fn inspect_derived_trial_balance_reflects_admitted_postings() {
    reset_db().await;
    post_balanced_entry("entry_001", 100);

    let (status, stdout, stderr) = run_cli(&[
        "inspect",
        "derived",
        "double_entry_ledger",
        "TrialBalanceRow",
    ]);
    assert!(status.success(), "inspect derived should succeed; {stderr}");
    let rows: Value = serde_json::from_str(&stdout).expect("stdout is JSON");
    let array = rows.as_array().expect("derived returns an array");
    assert!(
        !array.is_empty(),
        "after one balanced entry, TrialBalanceRow has rows: {stdout}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_derived_unknown_program_errors_to_stderr() {
    reset_db().await;
    let (status, _stdout, stderr) = run_cli(&[
        "inspect",
        "derived",
        "not_a_real_program",
        "TrialBalanceRow",
    ]);
    assert!(
        !status.success(),
        "unknown program in inspect derived must exit non-zero"
    );
    assert!(
        stderr.contains("not_a_real_program"),
        "stderr should name the unknown program: {stderr}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_derived_unknown_derived_name_errors_to_stderr() {
    reset_db().await;
    let (status, _stdout, stderr) = run_cli(&[
        "inspect",
        "derived",
        "double_entry_ledger",
        "NotARealDerivedClaim",
    ]);
    assert!(
        !status.success(),
        "unknown derived-claim name must exit non-zero"
    );
    assert!(
        stderr.contains("NotARealDerivedClaim"),
        "stderr should name the unknown derived claim: {stderr}"
    );
}
