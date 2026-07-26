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

mod common;

use std::process::Command;

use serde_json::Value;
use sqlx::PgPool;

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
    sqlx::query(morpholog_postgres::testing::RESET_SQL)
        .execute(&pool)
        .await
        .expect("truncate");
}

/// Run `morpholog` with the given subcommand args plus `--database-url`,
/// returning (status, stdout, stderr). Does not panic on non-zero exit;
/// the caller asserts on what they expect.
fn run_cli(args: &[&str]) -> (std::process::ExitStatus, String, String) {
    let url = database_url();
    let mut command = Command::new(common::bin());
    command.args(args).args(["--database-url", &url]);
    let output = command.output().expect("spawn morpholog binary");
    (
        output.status,
        String::from_utf8(output.stdout).expect("stdout utf8"),
        String::from_utf8(output.stderr).expect("stderr utf8"),
    )
}

/// Run `morpholog` with exactly the given args and NO `--database-url`,
/// for the offline subcommands whose contract is that they take no
/// connection (`evidence verify`).
fn run_cli_no_db(args: &[&str]) -> (std::process::ExitStatus, String, String) {
    let output = Command::new(common::bin())
        .args(args)
        .output()
        .expect("spawn morpholog binary");
    (
        output.status,
        String::from_utf8(output.stdout).expect("stdout utf8"),
        String::from_utf8(output.stderr).expect("stderr utf8"),
    )
}

/// Absolute path to the shipped double-entry-ledger example source, so
/// the CLI's file-path subcommands (`run`, `inspect derived`) can parse
/// it directly. Resolved from the crate manifest dir so it is robust to
/// the test process's working directory.
fn ledger_morph() -> String {
    format!(
        "{}/../../examples/03_double_entry_ledger/ledger.morph",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// The tagged-args JSON for one balanced ledger posting - the payload
/// every propose-shaped test builds.
fn ledger_args_json(entry_id: &str, date: &str, period: &str, amount: &str) -> String {
    format!(
        r#"[
            {{"type":"subject","value":"{entry_id}"}},
            {{"type":"subject","value":"{date}"}},
            {{"type":"subject","value":"{period}"}},
            {{"type":"subject","value":"account_cash"}},
            {{"type":"subject","value":"account_revenue"}},
            {{"type":"decimal","value":"{amount}"}}
        ]"#
    )
}

/// Issue a balanced journal entry via the CLI's `run` subcommand against
/// the shipped ledger example. Returns the `transition_id` from the
/// receipt so subsequent tests can use it as an as-of coordinate.
fn post_balanced_entry(entry_id: &str, amount: i64) -> uuid::Uuid {
    let args_json = ledger_args_json(entry_id, "2026-04-15", "q1_2026", &amount.to_string());
    let (status, stdout, stderr) = run_cli(&[
        "propose",
        &ledger_morph(),
        "post_simple_entry",
        "--actor",
        "alex",
        "--args",
        &args_json,
    ]);
    assert!(
        status.success(),
        "run post_simple_entry should succeed; stderr: {stderr}"
    );
    let receipt: Value = serde_json::from_str(&stdout).expect("receipt is JSON");
    assert_eq!(receipt["status"], "committed");
    receipt["transition_id"]
        .as_str()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .expect("receipt carries a transition_id UUID")
}

// ============================================================
// `run` against the shipped ledger example (commit/reject/malformed
// args). Parse failure, unknown transformation, and invariant rejection
// against a user-supplied temp `.morph` are covered in the `run` section
// further down.
// ============================================================

#[tokio::test(flavor = "current_thread")]
async fn run_malformed_args_json_errors_to_stderr() {
    reset_db().await;
    let (status, _stdout, stderr) = run_cli(&[
        "propose",
        &ledger_morph(),
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
async fn run_business_rejection_exits_one_with_rejected_receipt_on_stdout() {
    reset_db().await;
    // Close q1_2026, then attempt to post into the closed period - the
    // require gate rejects.
    let (status, _stdout, _stderr) = run_cli(&[
        "propose",
        &ledger_morph(),
        "close_period",
        "--actor",
        "alex",
        "--args",
        r#"[{"type":"subject","value":"q1_2026"}]"#,
    ]);
    assert!(status.success(), "close_period should commit");

    let (status, stdout, _stderr) = run_cli(&[
        "propose",
        &ledger_morph(),
        "post_simple_entry",
        "--actor",
        "alex",
        "--args",
        &ledger_args_json("entry_001", "2026-04-15", "q1_2026", "100"),
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
async fn inspect_claims_predicate_filter_returns_only_matching_claims() {
    reset_db().await;
    // One balanced entry admits one JournalEntry and two JournalLine
    // claims, so the filtered reads have known shapes.
    post_balanced_entry("entry_001", 100);

    let (status, stdout, stderr) = run_cli(&["inspect", "claims", "--predicate", "JournalEntry"]);
    assert!(
        status.success(),
        "filtered inspect should succeed; {stderr}"
    );
    let claims: Value = serde_json::from_str(&stdout).expect("stdout is JSON");
    let array = claims.as_array().expect("filtered claims are an array");
    assert_eq!(array.len(), 1, "one JournalEntry expected: {stdout}");
    assert!(
        array.iter().all(|c| c["predicate"] == "JournalEntry"),
        "filter must exclude every other predicate: {stdout}"
    );

    // The flag repeats: both predicates come back, nothing else does.
    let (status, stdout, _stderr) = run_cli(&[
        "inspect",
        "claims",
        "--predicate",
        "JournalEntry",
        "--predicate",
        "JournalLine",
    ]);
    assert!(status.success());
    let claims: Value = serde_json::from_str(&stdout).unwrap();
    let array = claims.as_array().unwrap();
    assert_eq!(
        array.len(),
        3,
        "one JournalEntry plus two JournalLines: {stdout}"
    );
    assert!(
        array
            .iter()
            .all(|c| c["predicate"] == "JournalEntry" || c["predicate"] == "JournalLine"),
        "repeated filter must still exclude other predicates: {stdout}"
    );

    // Naming the same predicate twice does not duplicate rows: the
    // filter is a membership test, not a per-flag scan. Pins the
    // public contract, not just today's SQL.
    let (status, stdout, _stderr) = run_cli(&[
        "inspect",
        "claims",
        "--predicate",
        "JournalEntry",
        "--predicate",
        "JournalEntry",
    ]);
    assert!(status.success());
    let claims: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        claims.as_array().unwrap().len(),
        1,
        "duplicate --predicate flags must not duplicate results: {stdout}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_claims_predicate_filter_composes_with_as_of() {
    reset_db().await;
    let first_tid = post_balanced_entry("entry_001", 100);
    post_balanced_entry("entry_002", 200);

    // Current filtered state: both entries' JournalEntry claims.
    let (_status, stdout, _stderr) = run_cli(&["inspect", "claims", "--predicate", "JournalEntry"]);
    let now: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        now.as_array().unwrap().len(),
        2,
        "two entries now: {stdout}"
    );

    // As-of the first transition, the same filter sees only the first.
    let (status, stdout, stderr) = run_cli(&[
        "inspect",
        "claims",
        "--as-of",
        &first_tid.to_string(),
        "--predicate",
        "JournalEntry",
    ]);
    assert!(
        status.success(),
        "filtered as-of inspect should succeed; {stderr}"
    );
    let then: Value = serde_json::from_str(&stdout).expect("as-of stdout is JSON");
    let array = then.as_array().unwrap();
    assert_eq!(array.len(), 1, "one entry as of the first commit: {stdout}");
    assert!(
        array.iter().all(|c| c["predicate"] == "JournalEntry"),
        "as-of filter must exclude other predicates: {stdout}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_claims_unknown_predicate_returns_empty_array() {
    reset_db().await;
    post_balanced_entry("entry_001", 100);

    // The claims table is the authority, not a programme's vocabulary:
    // a predicate with no admitted claims is an empty result, not an
    // error. (A typo'd name is indistinguishable from a true zero, by
    // design - `inspect claims` takes no `.morph` file to check against.)
    let (status, stdout, stderr) =
        run_cli(&["inspect", "claims", "--predicate", "NoSuchPredicate"]);
    assert!(
        status.success(),
        "unknown predicate should still exit zero; {stderr}"
    );
    let claims: Value = serde_json::from_str(&stdout).expect("stdout is JSON");
    assert_eq!(
        claims.as_array().expect("an array").len(),
        0,
        "unknown predicate yields an empty array: {stdout}"
    );
}

/// `explain` without `--json` renders claim-shaped prose - the default
/// surface a human reads, previously untested. Pins both verdict
/// headers; the structured content is pinned by the `--json` test.
#[tokio::test(flavor = "current_thread")]
async fn explain_without_json_renders_prose_for_both_verdicts() {
    reset_db().await;
    let entry_args = &ledger_args_json("e1", "2026-04-15", "q1_2026", "100");

    // Open period: the same proposal is admissible.
    let (status, stdout, stderr) = run_cli(&[
        "explain",
        &ledger_morph(),
        "post_simple_entry",
        "--actor",
        "alex",
        "--args",
        entry_args,
    ]);
    assert!(status.success(), "explain is read-only; {stderr}");
    assert!(
        stdout.starts_with("Admissible: post_simple_entry("),
        "admissible prose header expected; got:\n{stdout}"
    );

    // Close the period: the same proposal is now refused at the gate,
    // and the prose names it.
    let (status, _o, _e) = run_cli(&[
        "propose",
        &ledger_morph(),
        "close_period",
        "--actor",
        "alex",
        "--args",
        r#"[{"type":"subject","value":"q1_2026"}]"#,
    ]);
    assert!(status.success());
    let (status, stdout, _stderr) = run_cli(&[
        "explain",
        &ledger_morph(),
        "post_simple_entry",
        "--actor",
        "alex",
        "--args",
        entry_args,
    ]);
    assert!(
        status.success(),
        "a rejected verdict still exits zero - explaining is answering, not acting"
    );
    assert!(
        stdout.starts_with("Rejected: post_simple_entry("),
        "rejected prose header expected; got:\n{stdout}"
    );
    assert!(
        stdout.contains("Gate not satisfied:") && stdout.contains("not PeriodClosed(period)"),
        "the failed gate is named in prose; got:\n{stdout}"
    );
}

// ============================================================
// `verify` - replay-vs-claims consistency
// ============================================================

#[tokio::test(flavor = "current_thread")]
async fn verify_is_consistent_after_normal_commits() {
    reset_db().await;
    post_balanced_entry("entry_001", 100);
    post_balanced_entry("entry_002", 200);

    let (status, stdout, stderr) = run_cli(&["verify"]);
    assert!(status.success(), "verify should exit zero; {stderr}");
    let outcome: Value = serde_json::from_str(&stdout).expect("verify output is JSON");
    assert_eq!(outcome["replay"]["status"], "consistent", "got: {stdout}");
    assert_eq!(
        outcome["replay"]["transitions"], 2,
        "two commits replayed: {stdout}"
    );
    assert_eq!(
        outcome["tree"]["status"], "intact",
        "no checkpoints is still an intact tree: {stdout}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn verify_on_empty_database_is_consistent() {
    reset_db().await;
    let (status, stdout, _stderr) = run_cli(&["verify"]);
    assert!(status.success(), "empty database is trivially consistent");
    let outcome: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(outcome["replay"]["status"], "consistent");
    assert_eq!(outcome["replay"]["transitions"], 0);
    assert_eq!(outcome["tree"]["status"], "intact");
}

#[tokio::test(flavor = "current_thread")]
async fn verify_stays_consistent_under_concurrent_commits() {
    reset_db().await;
    post_balanced_entry("entry_000", 10);

    // A writer hammering commits while verify runs repeatedly. Before
    // verify read everything from one REPEATABLE READ snapshot, a
    // commit landing between its reads could manufacture a false
    // divergence on a perfectly healthy database - under that bug,
    // this test flakes; under the snapshot contract, it cannot fail.
    let writer = std::thread::spawn(|| {
        for i in 0..12 {
            post_balanced_entry(&format!("entry_w{i:03}"), 100 + i);
        }
    });
    for _ in 0..6 {
        let (status, stdout, stderr) = run_cli(&["verify"]);
        assert!(
            status.success(),
            "verify must not report false divergence under concurrent \
             commits; stdout:\n{stdout}\nstderr:\n{stderr}"
        );
    }
    writer.join().expect("writer thread panicked");
}

#[tokio::test(flavor = "current_thread")]
async fn verify_detects_an_out_of_band_edit() {
    reset_db().await;
    post_balanced_entry("entry_001", 100);

    // Tamper with the claims table behind the runtime's back: rewrite
    // the JournalEntry's arguments with raw SQL. The audit log still
    // describes the original, so replay and current state diverge in
    // both directions - the tampered claim is unjustified, the
    // original is missing.
    let pool = PgPool::connect(&database_url()).await.unwrap();
    sqlx::query(
        "UPDATE morpholog.claims
         SET arguments = '[{\"type\":\"subject\",\"value\":\"tampered\"}]'
         WHERE predicate_name = 'JournalEntry'",
    )
    .execute(&pool)
    .await
    .expect("out-of-band UPDATE");

    let (status, stdout, _stderr) = run_cli(&["verify"]);
    assert!(!status.success(), "divergence must exit non-zero");
    let outcome: Value = serde_json::from_str(&stdout).expect("verify output is JSON");
    assert_eq!(outcome["replay"]["status"], "divergent", "got: {stdout}");
    let unjustified = outcome["replay"]["only_in_claims_table"]
        .as_array()
        .unwrap();
    let missing = outcome["replay"]["only_in_replay"].as_array().unwrap();
    assert!(
        unjustified
            .iter()
            .any(|c| c["args"][0]["value"] == "tampered"),
        "the tampered claim must be reported as unjustified: {stdout}"
    );
    assert!(
        missing
            .iter()
            .any(|c| c["predicate"] == "JournalEntry" && c["args"][0]["value"] == "entry_001"),
        "the original claim must be reported as missing: {stdout}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn checkpoint_then_verify_against_the_anchor() {
    reset_db().await;
    post_balanced_entry("c1", 100);
    post_balanced_entry("c2", 200);

    // `checkpoint` prints the checkpoint as JSON - the external anchor.
    let (status, cp_stdout, stderr) = run_cli(&["checkpoint"]);
    assert!(status.success(), "checkpoint should succeed; {stderr}");
    let cp: Value = serde_json::from_str(&cp_stdout).expect("checkpoint output is JSON");
    assert_eq!(cp["status"], "created");
    assert_eq!(cp["tree_size"], 2, "two committed rows: {cp_stdout}");
    assert!(
        cp["root_hash"].as_str().unwrap().starts_with("sha256:"),
        "root is a self-describing hash: {cp_stdout}"
    );

    // Save it and verify the tree against it.
    // A unique temp file, auto-cleaned, so concurrent runs cannot collide.
    let mut anchor = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut anchor, cp_stdout.as_bytes()).unwrap();
    let (status, stdout, stderr) =
        run_cli(&["verify", "--anchor-file", anchor.path().to_str().unwrap()]);
    assert!(
        status.success(),
        "verify against a fresh anchor should pass; {stderr}\n{stdout}"
    );
    let outcome: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(outcome["tree"]["status"], "intact", "got: {stdout}");
    assert_eq!(outcome["tree"]["checkpoints"], 1);
}

#[tokio::test(flavor = "current_thread")]
async fn verify_require_signatures_fails_an_unsigned_checkpoint() {
    reset_db().await;
    post_balanced_entry("rs1", 100);
    let (status, _stdout, stderr) = run_cli(&["checkpoint"]);
    assert!(status.success(), "checkpoint should succeed; {stderr}");

    // Default verify: an unsigned checkpoint is intact (signing is opt-in).
    let (status, stdout, _stderr) = run_cli(&["verify"]);
    assert!(status.success(), "unsigned verify is intact: {stdout}");
    assert_eq!(
        serde_json::from_str::<Value>(&stdout).unwrap()["tree"]["status"],
        "intact"
    );

    // --require-signatures: the unsigned checkpoint now fails, exit non-zero.
    let (status, stdout, _stderr) = run_cli(&["verify", "--require-signatures"]);
    assert!(
        !status.success(),
        "require-signatures must fail an unsigned checkpoint: {stdout}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&stdout).unwrap()["tree"]["status"],
        "signature_required",
        "got: {stdout}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn evidence_export_then_verify_offline() {
    reset_db().await;
    post_balanced_entry("ev1", 100);
    post_balanced_entry("ev2", 200);

    // Anchor (saved outside the database), then export the pack.
    let (status, cp_stdout, stderr) = run_cli(&["checkpoint"]);
    assert!(status.success(), "checkpoint should succeed; {stderr}");
    let mut anchor = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut anchor, cp_stdout.as_bytes()).unwrap();

    let (status, pack_stdout, stderr) = run_cli(&["evidence", "export"]);
    assert!(status.success(), "evidence export should succeed; {stderr}");
    let pack: Value = serde_json::from_str(&pack_stdout).expect("pack is JSON");
    assert_eq!(pack["manifest"]["tree_size"], 2, "{pack_stdout}");
    assert_eq!(pack["rows"].as_array().unwrap().len(), 2);
    let mut packfile = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut packfile, pack_stdout.as_bytes()).unwrap();
    let pack_path = packfile.path().to_str().unwrap();

    // Offline verify (NO --database-url) against the anchor: intact, exit 0.
    let (status, stdout, stderr) = run_cli_no_db(&[
        "evidence",
        "verify",
        pack_path,
        "--anchor-file",
        anchor.path().to_str().unwrap(),
    ]);
    assert!(
        status.success(),
        "offline verify should pass; {stderr}\n{stdout}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&stdout).unwrap()["status"],
        "intact",
        "got: {stdout}"
    );

    // Edit a row in the pack file: verify must catch it and exit non-zero.
    let mut tampered_json: Value = serde_json::from_str(&pack_stdout).unwrap();
    tampered_json["rows"][0]["transformation_name"] = serde_json::json!("tampered");
    let mut tamperedfile = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut tamperedfile, tampered_json.to_string().as_bytes()).unwrap();
    let (status, stdout, _stderr) =
        run_cli_no_db(&["evidence", "verify", tamperedfile.path().to_str().unwrap()]);
    assert!(
        !status.success(),
        "a tampered pack must exit non-zero: {stdout}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&stdout).unwrap()["status"],
        "tampered",
        "got: {stdout}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn evidence_selective_export_then_verify_offline() {
    reset_db().await;
    let shown_a = post_balanced_entry("sd1", 100);
    post_balanced_entry("sd_hidden", 200);
    let shown_b = post_balanced_entry("sd3", 300);

    let (status, cp_stdout, stderr) = run_cli(&["checkpoint"]);
    assert!(status.success(), "checkpoint should succeed; {stderr}");
    let mut anchor = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut anchor, cp_stdout.as_bytes()).unwrap();

    // Disclose two of the three committed transitions.
    let (status, pack_stdout, stderr) = run_cli(&[
        "evidence",
        "export",
        "--transition",
        &shown_a.to_string(),
        "--transition",
        &shown_b.to_string(),
    ]);
    assert!(
        status.success(),
        "selective export should succeed; {stderr}"
    );
    let pack: Value = serde_json::from_str(&pack_stdout).expect("pack is JSON");
    assert_eq!(pack["manifest"]["pack_kind"], "selective", "{pack_stdout}");
    assert_eq!(pack["rows"].as_array().unwrap().len(), 2);

    // The reveal-nothing property over the real wire bytes: nothing of the
    // undisclosed transition survives - not its entry subject, accounts,
    // claims, or intent payloads.
    assert!(pack_stdout.contains("sd1"));
    assert!(
        !pack_stdout.contains("sd_hidden"),
        "undisclosed business payload leaked into the pack"
    );

    let mut packfile = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut packfile, pack_stdout.as_bytes()).unwrap();
    let pack_path = packfile.path().to_str().unwrap();

    // Offline verify (NO --database-url) against the anchor: intact, exit 0.
    let (status, stdout, stderr) = run_cli_no_db(&[
        "evidence",
        "verify",
        pack_path,
        "--anchor-file",
        anchor.path().to_str().unwrap(),
    ]);
    assert!(
        status.success(),
        "offline verify should pass; {stderr}\n{stdout}"
    );
    let verdict: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(verdict["status"], "intact", "got: {stdout}");
    assert_eq!(verdict["rows_disclosed"], 2, "got: {stdout}");

    // Edit a disclosed row: verify names its position and exits non-zero.
    let mut tampered_json: Value = serde_json::from_str(&pack_stdout).unwrap();
    tampered_json["rows"][0]["transformation_name"] = serde_json::json!("tampered");
    let mut tamperedfile = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut tamperedfile, tampered_json.to_string().as_bytes()).unwrap();
    let (status, stdout, _stderr) =
        run_cli_no_db(&["evidence", "verify", tamperedfile.path().to_str().unwrap()]);
    assert!(
        !status.success(),
        "a tampered selective pack must exit non-zero: {stdout}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&stdout).unwrap()["status"],
        "row_not_included",
        "got: {stdout}"
    );

    // Compliance mode: the covering checkpoint is unsigned, so
    // --require-signatures fails the otherwise-intact pack.
    let (status, stdout, _stderr) =
        run_cli_no_db(&["evidence", "verify", pack_path, "--require-signatures"]);
    assert!(!status.success(), "unsigned must fail the policy: {stdout}");
    assert_eq!(
        serde_json::from_str::<Value>(&stdout).unwrap()["status"],
        "signature_required",
        "got: {stdout}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn evidence_verify_names_an_unknown_future_pack_version() {
    // A v4 pack must be named as too new, never misread as a malformed v1.
    let mut packfile = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(
        &mut packfile,
        br#"{"manifest": {"pack_format_version": 4}}"#,
    )
    .unwrap();
    let (status, stdout, _stderr) =
        run_cli_no_db(&["evidence", "verify", packfile.path().to_str().unwrap()]);
    assert!(!status.success());
    let verdict: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(verdict["status"], "malformed_pack", "got: {stdout}");
    assert!(
        verdict["detail"].as_str().unwrap().contains("newer"),
        "got: {stdout}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn evidence_verify_on_a_readable_but_invalid_pack_is_a_malformed_verdict() {
    // A file that reads but is not a valid pack is a decided verdict on
    // stdout (`malformed_pack`, exit one), not an operational failure on
    // stderr - the offline verifier still answers. No database needed.
    let mut packfile = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut packfile, br#"{"not": "a pack"}"#).unwrap();
    let (status, stdout, stderr) =
        run_cli_no_db(&["evidence", "verify", packfile.path().to_str().unwrap()]);
    assert!(
        !status.success(),
        "an invalid pack must exit non-zero: {stdout}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&stdout).unwrap_or_else(|_| panic!(
            "verdict on stdout, got stdout={stdout:?} stderr={stderr:?}"
        ))["status"],
        "malformed_pack",
        "got: {stdout}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn evaluate_scores_a_candidate_against_history() {
    reset_db().await;
    post_balanced_entry("ev1", 100);
    post_balanced_entry("ev2", 200);

    // A candidate that forbids journal entries - history violates it.
    let candidate = "program candidate\n\n\
         predicate JournalEntry(entry_id: Subject, posting_date: Subject, period: Subject)\n\n\
         invariant no_entries:\n    not (exists e: JournalEntry(e, _, _))\n";
    let mut f = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut f, candidate.as_bytes()).unwrap();

    let (status, stdout, stderr) = run_cli(&["evaluate", f.path().to_str().unwrap()]);
    assert!(
        status.success(),
        "evaluate should succeed; {stderr}\n{stdout}"
    );
    let report: Value = serde_json::from_str(&stdout).expect("report is JSON");
    assert_eq!(report["score_format_version"], 1);
    assert_eq!(report["semantics"], "fresh_state_violation_v1");
    assert!(
        report["program_hash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"),
        "{stdout}"
    );
    let inv = &report["invariants"][0];
    assert_eq!(inv["invariant"], "no_entries");
    // The first entry introduces the violation; the second inherits it.
    assert_eq!(inv["would_refuse"], 1, "got: {stdout}");
}

#[tokio::test(flavor = "current_thread")]
async fn evaluate_train_until_reports_per_slice_scores() {
    reset_db().await;
    let boundary = post_balanced_entry("ev1", 100);
    post_balanced_entry("ev2", 200);

    let candidate = "program candidate\n\n\
         predicate JournalEntry(entry_id: Subject, posting_date: Subject, period: Subject)\n\n\
         invariant no_entries:\n    not (exists e: JournalEntry(e, _, _))\n";
    let mut f = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut f, candidate.as_bytes()).unwrap();

    let (status, stdout, stderr) = run_cli(&[
        "evaluate",
        f.path().to_str().unwrap(),
        "--train-until",
        &boundary.to_string(),
    ]);
    assert!(status.success(), "{stderr}\n{stdout}");
    let report: Value = serde_json::from_str(&stdout).expect("report is JSON");
    let split = &report["split"];
    assert_eq!(
        split["boundary"]["resolved_transition_id"],
        boundary.to_string(),
        "got: {stdout}"
    );
    // The first entry introduces the violation inside the train slice;
    // the test slice inherits it and reports clean.
    assert_eq!(split["train"]["transitions_replayed"], 1);
    assert_eq!(split["test"]["transitions_replayed"], 1);
    assert_eq!(split["train"]["invariants"][0]["would_refuse"], 1);
    assert_eq!(split["test"]["invariants"][0]["would_refuse"], 0);
}

#[tokio::test(flavor = "current_thread")]
async fn evaluate_train_until_conflicts_with_packs() {
    let output = Command::new(common::bin())
        .args([
            "evaluate",
            "whatever.morph",
            "--packs",
            "somewhere",
            "--train-until",
            "2026-07-01T00:00:00Z",
        ])
        .output()
        .expect("spawn morpholog binary");
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr utf8");
    assert!(
        stderr.contains("--train-until") && stderr.contains("--packs"),
        "expected the clap conflict naming both flags, got: {stderr}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn evaluate_rejects_a_pre_candidate_before_connecting() {
    // A transition-relational candidate (uses pre(...)): v1 cannot score it.
    let candidate = "program candidate\n\n\
         predicate Flag(x: Subject)\n\n\
         invariant uses_pre:\n    Flag(a) implies pre(Flag(a))\n";
    let mut f = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut f, candidate.as_bytes()).unwrap();

    // A deliberately unreachable database: if the CLI connected before
    // checking, the failure would be a connection error, not the pre
    // rejection. The rejection must come first.
    let output = Command::new(common::bin())
        .args([
            "evaluate",
            f.path().to_str().unwrap(),
            "--database-url",
            "postgres://nonexistent.invalid:1/nope",
        ])
        .output()
        .expect("spawn morpholog binary");
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr utf8");
    assert!(
        stderr.contains("pre(...)"),
        "expected the pre(...) rejection, got: {stderr}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn evaluate_against_a_pack_needs_no_database() {
    reset_db().await;
    post_balanced_entry("ev1", 100);
    post_balanced_entry("ev2", 200);

    // Checkpoint + export a pack over the history (these need the DB).
    let (s, _, e) = run_cli(&["checkpoint"]);
    assert!(s.success(), "checkpoint: {e}");
    let (s, pack_stdout, e) = run_cli(&["evidence", "export"]);
    assert!(s.success(), "export: {e}");
    let mut packfile = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut packfile, pack_stdout.as_bytes()).unwrap();

    let candidate = "program candidate\n\n\
         predicate JournalEntry(entry_id: Subject, posting_date: Subject, period: Subject)\n\n\
         invariant no_entries:\n    not (exists e: JournalEntry(e, _, _))\n";
    let mut candfile = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut candfile, candidate.as_bytes()).unwrap();

    // Score against the pack with NO --database-url - the offline promise.
    let (status, stdout, stderr) = run_cli_no_db(&[
        "evaluate",
        candfile.path().to_str().unwrap(),
        "--pack",
        packfile.path().to_str().unwrap(),
    ]);
    assert!(
        status.success(),
        "pack-mode evaluate should pass with no DB; {stderr}\n{stdout}"
    );
    let report: Value = serde_json::from_str(&stdout).expect("report is JSON");
    assert_eq!(report["semantics"], "fresh_state_violation_v1");
    assert_eq!(report["invariants"][0]["would_refuse"], 1, "got: {stdout}");

    // A tampered pack is refused, not scored.
    let mut tampered: Value = serde_json::from_str(&pack_stdout).unwrap();
    tampered["rows"][0]["transformation_name"] = serde_json::json!("tampered");
    let mut tamperedfile = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut tamperedfile, tampered.to_string().as_bytes()).unwrap();
    let (status, _stdout, stderr) = run_cli_no_db(&[
        "evaluate",
        candfile.path().to_str().unwrap(),
        "--pack",
        tamperedfile.path().to_str().unwrap(),
    ]);
    assert!(!status.success(), "a tampered pack must be refused");
    assert!(stderr.contains("does not verify"), "got: {stderr}");
}

const CANDIDATE_NO_ENTRIES: &str = "program candidate\n\n\
     predicate JournalEntry(entry_id: Subject, posting_date: Subject, period: Subject)\n\n\
     invariant no_entries:\n    not (exists e: JournalEntry(e, _, _))\n";

/// Build a one-firm-year pack and write it to `dir/<name>.json`.
async fn write_case_pack(dir: &std::path::Path, name: &str, amount: i64) {
    reset_db().await;
    post_balanced_entry(name, amount);
    let (s, _, e) = run_cli(&["checkpoint"]);
    assert!(s.success(), "checkpoint {name}: {e}");
    let (s, pack, e) = run_cli(&["evidence", "export"]);
    assert!(s.success(), "export {name}: {e}");
    std::fs::write(dir.join(format!("{name}.json")), pack).unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn evaluate_packs_batches_offline_sorted_by_file_name() {
    let dir = tempfile::tempdir().unwrap();
    // Build three packs in non-sorted creation order.
    write_case_pack(dir.path(), "c", 100).await;
    write_case_pack(dir.path(), "a", 200).await;
    write_case_pack(dir.path(), "b", 300).await;

    let mut candfile = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut candfile, CANDIDATE_NO_ENTRIES.as_bytes()).unwrap();

    // No --database-url: batch scoring is offline.
    let (status, stdout, stderr) = run_cli_no_db(&[
        "evaluate",
        candfile.path().to_str().unwrap(),
        "--packs",
        dir.path().to_str().unwrap(),
    ]);
    assert!(
        status.success(),
        "batch evaluate should pass with no DB; {stderr}\n{stdout}"
    );
    let report: Value = serde_json::from_str(&stdout).expect("report is JSON");
    assert_eq!(report["semantics"], "fresh_state_violation_v1");
    let cases = report["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 3);
    // Deterministic, by file name, regardless of creation order.
    assert_eq!(cases[0]["pack"], "a.json");
    assert_eq!(cases[1]["pack"], "b.json");
    assert_eq!(cases[2]["pack"], "c.json");
    assert_eq!(cases[0]["status"], "scored");
    assert_eq!(cases[0]["invariants"][0]["would_refuse"], 1, "{stdout}");
}

#[tokio::test(flavor = "current_thread")]
async fn evaluate_packs_aborts_on_an_unparseable_file() {
    let dir = tempfile::tempdir().unwrap();
    write_case_pack(dir.path(), "a", 100).await;
    // A junk file in the controlled packs directory aborts the batch.
    std::fs::write(dir.path().join("b.json"), "{ not a pack }").unwrap();

    let mut candfile = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut candfile, CANDIDATE_NO_ENTRIES.as_bytes()).unwrap();

    let (status, _stdout, stderr) = run_cli_no_db(&[
        "evaluate",
        candfile.path().to_str().unwrap(),
        "--packs",
        dir.path().to_str().unwrap(),
    ]);
    assert!(!status.success(), "an unparseable pack file must abort");
    assert!(stderr.contains("parsing pack file"), "got: {stderr}");
}

#[tokio::test(flavor = "current_thread")]
async fn evaluate_packs_with_anchor_file_is_a_usage_error() {
    // --anchor-file requires --pack (single), so pairing it with --packs is
    // a clap error, before any work.
    let (status, _stdout, stderr) = run_cli_no_db(&[
        "evaluate",
        "/nonexistent/candidate.morph",
        "--packs",
        "/nonexistent/dir",
        "--anchor-file",
        "/nonexistent/anchor.json",
    ]);
    assert!(!status.success(), "anchors are single-pack only");
    assert!(
        stderr.contains("--pack") || stderr.contains("anchor"),
        "got: {stderr}"
    );
}

// ============================================================
// `--as-of` with a timestamp
// ============================================================

#[tokio::test(flavor = "current_thread")]
async fn inspect_claims_as_of_timestamp_resolves_to_the_prior_state() {
    reset_db().await;
    post_balanced_entry("entry_001", 100);
    post_balanced_entry("entry_002", 200);

    // The first commit's exact timestamp, from the audit log. Using it
    // verbatim also pins that the boundary is inclusive: at the very
    // instant of a commit, that commit's state is what you get.
    let (_s, stdout, _e) = run_cli(&["inspect", "audit"]);
    let rows = ndjson(&stdout);
    let first_committed_at = rows[0]["committed_at"]
        .as_str()
        .expect("audit row carries committed_at")
        .to_string();

    let (status, stdout, stderr) = run_cli(&[
        "inspect",
        "claims",
        "--as-of",
        &first_committed_at,
        "--predicate",
        "JournalEntry",
    ]);
    assert!(status.success(), "timestamp as-of should succeed; {stderr}");
    let claims: Value = serde_json::from_str(&stdout).unwrap();
    let array = claims.as_array().unwrap();
    assert_eq!(
        array.len(),
        1,
        "only the first entry exists at the first commit's instant: {stdout}"
    );
    assert_eq!(array[0]["args"][0]["value"], "entry_001");
}

#[tokio::test(flavor = "current_thread")]
async fn as_of_timestamp_before_all_commits_errors() {
    reset_db().await;
    post_balanced_entry("entry_001", 100);

    let (status, _stdout, stderr) =
        run_cli(&["inspect", "claims", "--as-of", "1970-01-01T00:00:00Z"]);
    assert!(
        !status.success(),
        "a timestamp before every commit must error"
    );
    assert!(
        stderr.contains("no transition committed at or before"),
        "the error should name the condition; got:\n{stderr}"
    );
}

/// Parse NDJSON output: one JSON value per non-empty line.
fn ndjson(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("each line is one JSON value"))
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_audit_streams_one_ndjson_line_per_committed_transition() {
    reset_db().await;
    post_balanced_entry("entry_001", 100);
    post_balanced_entry("entry_002", 200);

    let (status, stdout, _stderr) = run_cli(&["inspect", "audit"]);
    assert!(status.success());
    let rows = ndjson(&stdout);
    assert_eq!(
        rows.len(),
        2,
        "two committed transitions, two lines: {stdout}"
    );
    // Every line is a full audit row: the tagged arrays and the
    // scalar fields ride together.
    for row in &rows {
        assert!(row["transition_id"].is_string());
        assert!(row["asserted_claims"].is_array());
        assert!(row["committed_at"].is_string());
        assert_eq!(row["actor"]["type"], "subject");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_audit_after_resumes_strictly_after_the_cursor() {
    reset_db().await;
    let t1 = post_balanced_entry("entry_001", 100);
    post_balanced_entry("entry_002", 200);
    post_balanced_entry("entry_003", 300);

    // Resuming from the first transition yields exactly the later
    // two, in order; the cursor row itself is excluded.
    let (status, stdout, _stderr) = run_cli(&["inspect", "audit", "--after", &t1.to_string()]);
    assert!(status.success());
    let rows = ndjson(&stdout);
    assert_eq!(rows.len(), 2, "strictly after the cursor: {stdout}");

    // Resuming from the last line's id is an empty tail, exit 0 -
    // the poll loop's steady state.
    let last_id = rows[1]["transition_id"].as_str().unwrap().to_string();
    let (status, stdout, _stderr) = run_cli(&["inspect", "audit", "--after", &last_id]);
    assert!(status.success(), "an empty tail is not an error");
    assert!(stdout.trim().is_empty(), "empty tail, empty stdout");

    // An unknown cursor is an error naming the id - never a silent
    // restart from zero.
    let unknown = uuid::Uuid::now_v7().to_string();
    let (status, _stdout, stderr) = run_cli(&["inspect", "audit", "--after", &unknown]);
    assert!(!status.success());
    assert!(
        stderr.contains(&unknown),
        "the error names the unknown id; got: {stderr}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_audit_named_decodes_claims_and_leaves_arguments_tagged() {
    reset_db().await;
    post_balanced_entry("entry_001", 100);

    let (status, stdout, _stderr) = run_cli(&["inspect", "audit", "--named", &ledger_morph()]);
    assert!(status.success());
    let rows = ndjson(&stdout);
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    // Claims decode by declared field name...
    let asserted = row["asserted_claims"].as_array().unwrap();
    assert!(!asserted.is_empty());
    assert!(
        asserted.iter().all(|c| c["args"].is_object()),
        "named claims carry field-keyed args: {row}"
    );
    // ...while transformation arguments stay tagged - a different
    // vocabulary, deliberately not half-decoded.
    let arguments = row["arguments"].as_array().unwrap();
    assert!(
        arguments.iter().all(|a| a["type"].is_string()),
        "arguments stay tagged: {row}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_audit_named_skew_is_a_hard_error_naming_both_sides() {
    reset_db().await;
    post_balanced_entry("entry_001", 100);

    // A programme that does not declare the ledger vocabulary: the
    // named decode must refuse with the skew error, not skip rows.
    let other = std::env::temp_dir().join("audit_named_skew.morph");
    std::fs::write(
        &other,
        "program unrelated

predicate Solo(only_id: Subject)

         transformation touch(only_id):
    admit Solo(only_id)
",
    )
    .unwrap();
    let (status, _stdout, stderr) =
        run_cli(&["inspect", "audit", "--named", other.to_str().unwrap()]);
    assert!(!status.success(), "skew must be a hard error");
    assert!(
        stderr.contains("skew"),
        "the error names the skew; got: {stderr}"
    );
}

// The managed-Postgres opt-in reaches both watermark consumers. The
// local/CI test role is a superuser, so the catalog census is
// trivially satisfiable here; the census's own teeth are tested in
// morpholog-postgres, and the genuinely-hidden-session path only
// exists on a managed host.
#[tokio::test(flavor = "current_thread")]
async fn inspect_audit_writer_role_assertion_streams_the_tail() {
    reset_db().await;
    post_balanced_entry("entry_001", 100);
    let me = common::session_user(&database_url()).await;

    let (status, stdout, stderr) = run_cli(&["inspect", "audit", "--writer-role", &me]);
    assert!(status.success(), "asserted tail should stream; {stderr}");
    let lines: Vec<&str> = stdout.lines().filter(|l| !l.trim().is_empty()).collect();
    assert_eq!(
        lines.len(),
        1,
        "one committed transition, one line: {stdout}"
    );
    let row: Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(row["transformation_name"], "post_simple_entry");
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_audit_unknown_writer_role_is_refused_as_a_typo() {
    reset_db().await;
    let (status, _stdout, stderr) =
        run_cli(&["inspect", "audit", "--writer-role", "no_such_role_cli_209"]);
    assert!(!status.success(), "an unknown asserted role must refuse");
    assert!(
        stderr.contains("no_such_role_cli_209"),
        "the refusal names the role: {stderr}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn checkpoint_accepts_the_writer_assertion() {
    reset_db().await;
    post_balanced_entry("entry_001", 100);
    let me = common::session_user(&database_url()).await;

    let (status, stdout, stderr) = run_cli(&["checkpoint", "--writer-role", &me]);
    assert!(
        status.success(),
        "asserted checkpoint should commit; {stderr}"
    );
    let outcome: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(outcome["status"], "created", "{stdout}");
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_rejections_lists_refusals_and_an_empty_log_is_empty() {
    reset_db().await;

    // An empty rejection log lists empty and exits zero - listing is
    // answering, not enforcing.
    let (status, stdout, _stderr) = run_cli(&["inspect", "rejections"]);
    assert!(status.success());
    let rows: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 0, "empty log: {stdout}");

    // Close the period, then post into it: the require gate refuses
    // and the refusal is on the record.
    let (status, ..) = run_cli(&[
        "propose",
        &ledger_morph(),
        "close_period",
        "--actor",
        "alex",
        "--args",
        r#"[{"type":"subject","value":"q1_2026"}]"#,
    ]);
    assert!(status.success(), "close_period should commit");
    let (status, ..) = run_cli(&[
        "propose",
        &ledger_morph(),
        "post_simple_entry",
        "--actor",
        "alex",
        "--args",
        &ledger_args_json("entry_001", "2026-04-15", "q1_2026", "100"),
    ]);
    assert!(!status.success(), "posting into a closed period rejects");

    let (status, stdout, _stderr) = run_cli(&["inspect", "rejections"]);
    assert!(status.success());
    let rows: Value = serde_json::from_str(&stdout).unwrap();
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 1, "one refusal, one row: {stdout}");
    let row = &rows[0];
    assert_eq!(row["transformation_name"], "post_simple_entry");
    assert_eq!(row["kind"], "require");
    assert_eq!(
        row["actor"],
        serde_json::json!({"type": "subject", "value": "alex"})
    );
    assert!(row["rule"].is_string());
    assert!(
        row["reason"].as_str().unwrap().contains("require failed"),
        "the exact envelope reason string is recorded: {row}"
    );
    assert!(
        row.get("invariant_version").is_none(),
        "gate kinds carry no invariant version and the field is omitted"
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

    let ledger = ledger_morph();
    let (status, stdout, stderr) = run_cli(&["inspect", "derived", &ledger, "TrialBalanceRow"]);
    assert!(status.success(), "inspect derived should succeed; {stderr}");
    let rows: Value = serde_json::from_str(&stdout).expect("stdout is JSON");
    let array = rows.as_array().expect("derived returns an array");
    assert!(
        !array.is_empty(),
        "after one balanced entry, TrialBalanceRow has rows: {stdout}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_derived_unknown_file_errors_to_stderr() {
    reset_db().await;
    let (status, _stdout, stderr) = run_cli(&[
        "inspect",
        "derived",
        "/no/such/program.morph",
        "TrialBalanceRow",
    ]);
    assert!(
        !status.success(),
        "a missing source file must exit non-zero"
    );
    assert!(
        !stderr.is_empty(),
        "stderr should carry an error explanation: {stderr}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_derived_unknown_derived_name_errors_to_stderr() {
    reset_db().await;
    let ledger = ledger_morph();
    let (status, _stdout, stderr) =
        run_cli(&["inspect", "derived", &ledger, "NotARealDerivedClaim"]);
    assert!(
        !status.success(),
        "unknown derived-claim name must exit non-zero"
    );
    assert!(
        stderr.contains("NotARealDerivedClaim"),
        "stderr should name the unknown derived claim: {stderr}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_derived_named_decodes_rows_and_default_stays_tagged() {
    reset_db().await;
    post_balanced_entry("entry_001", 100);
    let ledger = ledger_morph();

    let (status, stdout, stderr) =
        run_cli(&["inspect", "derived", &ledger, "TrialBalanceRow", "--named"]);
    assert!(status.success(), "--named should succeed; {stderr}");
    let rows: Value = serde_json::from_str(&stdout).unwrap();
    let row = &rows.as_array().expect("named output is an array")[0];
    assert_eq!(row["predicate"], "TrialBalanceRow");
    let args = row["args"].as_object().expect("named args are an object");
    assert!(
        args.contains_key("account") && args.contains_key("balance"),
        "args are keyed by declared field name: {row}"
    );

    // The acceptance side: adding --named must not move the default,
    // which stays the tagged array `inspect claims` also prints.
    let (status, stdout, _stderr) = run_cli(&["inspect", "derived", &ledger, "TrialBalanceRow"]);
    assert!(status.success());
    let rows: Value = serde_json::from_str(&stdout).unwrap();
    let row = &rows.as_array().expect("default output is an array")[0];
    assert!(
        row["args"].as_array().expect("default args are tagged")[0]
            .get("type")
            .is_some(),
        "default rows keep the tagged positional encoding: {row}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_derived_named_composes_with_as_of() {
    reset_db().await;
    let first = post_balanced_entry("entry_001", 100);
    post_balanced_entry("entry_002", 50);
    let ledger = ledger_morph();

    let (status, stdout, stderr) = run_cli(&[
        "inspect",
        "derived",
        &ledger,
        "TrialBalanceRow",
        "--named",
        "--as-of",
        &first.to_string(),
    ]);
    assert!(status.success(), "--named --as-of should succeed; {stderr}");
    let rows: Value = serde_json::from_str(&stdout).unwrap();
    let cash_balance = rows
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["args"]["account"] == "account_cash")
        .expect("cash row present as of the first entry")["args"]["balance"]
        .clone();
    assert_eq!(
        cash_balance, "100",
        "as of the first transition only entry_001 is visible: {rows}"
    );
}

// ============================================================
// `refresh derived`
// ============================================================

#[tokio::test(flavor = "current_thread")]
async fn refresh_derived_emits_the_typed_report() {
    reset_db().await;
    post_balanced_entry("entry_001", 100);
    let ledger = ledger_morph();

    let (status, stdout, stderr) = run_cli(&["refresh", "derived", &ledger]);
    assert!(status.success(), "refresh derived should succeed; {stderr}");
    let report: Value = serde_json::from_str(&stdout).expect("stdout is the typed report");
    // The ledger fixture declares exactly one derived predicate, which
    // is what licenses comparing the report's total row count against
    // one `inspect derived` read below.
    assert_eq!(report["derived_predicate_count"], 1, "{report}");
    assert!(
        report["model_hash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"),
        "{report}"
    );
    assert!(
        report["source_snapshot_transition_id"].is_string()
            && report["source_snapshot_committed_at"].is_string(),
        "a populated ledger carries the snapshot pair together: {report}"
    );
    assert!(
        stderr.contains("refreshed"),
        "the human summary stays on stderr: {stderr}"
    );

    let (status, stdout, _stderr) = run_cli(&["inspect", "derived", &ledger, "TrialBalanceRow"]);
    assert!(status.success());
    let rows: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        report["derived_claim_count"].as_u64().unwrap(),
        rows.as_array().unwrap().len() as u64,
        "the report's count matches the sole derived predicate's rows"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn refresh_derived_report_without_transitions_omits_snapshot_pair() {
    reset_db().await;
    let ledger = ledger_morph();

    let (status, stdout, stderr) = run_cli(&["refresh", "derived", &ledger]);
    assert!(
        status.success(),
        "refresh on an empty ledger succeeds; {stderr}"
    );
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["derived_claim_count"], 0, "{report}");
    assert!(
        report.get("source_snapshot_transition_id").is_none()
            && report.get("source_snapshot_committed_at").is_none(),
        "no committed transitions: the snapshot pair is absent together: {report}"
    );
}

// ============================================================
// `morpholog run` subcommand
//
// Parses a user-supplied `.morph` file by path, validates it, and
// proposes a transformation from it - the CLI's commit path.
// ============================================================

/// Write a minimal balanced-ledger programme to a temp .morph file
/// and return the path. The programme is deliberately a subset of
/// the built-in double-entry ledger - one transformation, one
/// invariant - so the run subcommand has something simple to admit
/// against and the test does not depend on the full example's
/// invariant suite.
fn write_temp_ledger_morph() -> std::path::PathBuf {
    let body = r#"
program temp_ledger

predicate JournalEntry(entry_id: Subject, posting_date: Subject, period: Subject)
predicate JournalLine(entry_id: Subject, account: Subject, debit_amount: Decimal, credit_amount: Decimal)

intent JournalEntryPosted(entry_id: Subject)

invariant balanced_posted_entry:
    JournalEntry(entry, _, _) implies (sum(d | JournalLine(entry, _, d, _)) = sum(c | JournalLine(entry, _, _, c)))

transformation post_simple_entry(entry_id, posting_date, period, debit_account, credit_account, amount):
    admit JournalEntry(entry_id, posting_date, period)
    admit JournalLine(entry_id, debit_account, amount, 0)
    admit JournalLine(entry_id, credit_account, 0, amount)
    emit JournalEntryPosted(entry_id)
"#;
    let dir = std::env::temp_dir().join(format!("morpholog_run_test_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("ledger.morph");
    std::fs::write(&path, body).expect("write temp .morph");
    path
}

#[tokio::test(flavor = "current_thread")]
async fn run_commits_a_balanced_entry_from_user_supplied_morph_file() {
    reset_db().await;
    let path = write_temp_ledger_morph();
    let args_json = &ledger_args_json("entry_001", "2026-04-15", "q1_2026", "100");
    let (status, stdout, stderr) = run_cli(&[
        "propose",
        path.to_str().unwrap(),
        "post_simple_entry",
        "--actor",
        "alex",
        "--args",
        args_json,
    ]);
    assert!(
        status.success(),
        "run should succeed; stderr: {stderr}; stdout: {stdout}"
    );
    let receipt: Value = serde_json::from_str(&stdout).expect("receipt is JSON");
    assert_eq!(receipt["status"], "committed");
}

/// `run --args-named` happy path against the temp ledger. Same
/// transformation as the tagged-form test above, but with the
/// embedder-facing codec: bare values keyed by parameter name. The
/// CLI consults `transformation_param_kinds` to coerce each value
/// against its declared kind. All Subject values are UUIDs because
/// the schema commits to `format: "uuid"` and the codec enforces
/// it.
#[tokio::test(flavor = "current_thread")]
async fn run_args_named_commits_with_the_friendly_codec() {
    reset_db().await;
    let path = write_temp_ledger_morph();
    let args_named = r#"{
        "entry_id":"018f0000-0000-7000-8000-000000000001",
        "posting_date":"018f0000-0000-7000-8000-000000000002",
        "period":"018f0000-0000-7000-8000-000000000003",
        "debit_account":"018f0000-0000-7000-8000-000000000004",
        "credit_account":"018f0000-0000-7000-8000-000000000005",
        "amount":"250"
    }"#;
    let (status, stdout, stderr) = run_cli(&[
        "propose",
        path.to_str().unwrap(),
        "post_simple_entry",
        "--actor",
        "alex",
        "--args-named",
        args_named,
    ]);
    assert!(
        status.success(),
        "--args-named happy path should commit; stderr: {stderr}; stdout: {stdout}"
    );
    let receipt: Value = serde_json::from_str(&stdout).expect("receipt is JSON");
    assert_eq!(receipt["status"], "committed");
}

/// Propose `post_simple_entry` from an already-written programme with
/// the named codec and require a hard error whose stderr carries
/// every needle. The shared scaffold of the `--args-named` error
/// family; takes the path so a multi-case test resets and writes once.
fn propose_named_expect_stderr_at(path: &std::path::Path, args_named: &str, needles: &[&str]) {
    let (status, _stdout, stderr) = run_cli(&[
        "propose",
        path.to_str().unwrap(),
        "post_simple_entry",
        "--actor",
        "alex",
        "--args-named",
        args_named,
    ]);
    assert!(!status.success(), "expected a hard error; stderr: {stderr}");
    for needle in needles {
        assert!(
            stderr.contains(needle),
            "stderr should contain `{needle}`; got: {stderr}"
        );
    }
}

/// One-off convenience over [`propose_named_expect_stderr_at`]:
/// reset the database and write the temp ledger for a single case.
async fn propose_named_expect_stderr(args_named: &str, needles: &[&str]) {
    reset_db().await;
    let path = write_temp_ledger_morph();
    propose_named_expect_stderr_at(&path, args_named, needles);
}

/// Missing a declared parameter in the `--args-named` object is a
/// hard error before any database work. The error names the missing
/// parameter and points at `morpholog schema` for the accepted
/// shape.
#[tokio::test(flavor = "current_thread")]
async fn run_args_named_missing_required_errors_with_schema_hint() {
    propose_named_expect_stderr(
        r#"{
            "entry_id":"018f0000-0000-7000-8000-000000000011",
            "posting_date":"018f0000-0000-7000-8000-000000000012",
            "period":"018f0000-0000-7000-8000-000000000013",
            "debit_account":"018f0000-0000-7000-8000-000000000014",
            "credit_account":"018f0000-0000-7000-8000-000000000015"
        }"#,
        &["missing required parameter `amount`", "morpholog schema"],
    )
    .await;
}

/// An unknown key in `--args-named` is a hard error. The error lists
/// the parameters that ARE accepted (here `amount` and `entry_id`),
/// so a typo surfaces clearly rather than as "missing required"
/// (which would point at the wrong target).
#[tokio::test(flavor = "current_thread")]
async fn run_args_named_unknown_key_errors_with_expected_names() {
    propose_named_expect_stderr(
        r#"{
            "entry_id":"018f0000-0000-7000-8000-000000000021",
            "posting_date":"018f0000-0000-7000-8000-000000000022",
            "period":"018f0000-0000-7000-8000-000000000023",
            "debit_account":"018f0000-0000-7000-8000-000000000024",
            "credit_account":"018f0000-0000-7000-8000-000000000025",
            "amount":"100",
            "amaount":"100"
        }"#,
        &["unknown parameter(s) `amaount`", "amount", "entry_id"],
    )
    .await;
}

/// `explain --args-named --json` parses the embedder-facing codec
/// the same way `run` does and produces an `Explanation` envelope.
/// Both verbs share the decode path through `commands::args` and
/// the in-crate Clap tests pin the surface, but a binary-level
/// smoke test catches the wiring (explain's pre-state load + the
/// in-memory explain call) under the named codec, not just the
/// tagged one.
#[tokio::test(flavor = "current_thread")]
async fn explain_args_named_returns_explanation_envelope() {
    reset_db().await;
    let path = write_temp_ledger_morph();
    let args_named = r#"{
        "entry_id":"018f0000-0000-7000-8000-000000000061",
        "posting_date":"018f0000-0000-7000-8000-000000000062",
        "period":"018f0000-0000-7000-8000-000000000063",
        "debit_account":"018f0000-0000-7000-8000-000000000064",
        "credit_account":"018f0000-0000-7000-8000-000000000065",
        "amount":"100"
    }"#;
    let (status, stdout, stderr) = run_cli(&[
        "explain",
        path.to_str().unwrap(),
        "post_simple_entry",
        "--actor",
        "alex",
        "--args-named",
        args_named,
        "--json",
    ]);
    // Explain is read-only and always exits zero on a parsed-and-
    // validated programme; the verdict (admissible or rejected)
    // lives inside the JSON envelope.
    assert!(
        status.success(),
        "explain should always exit zero on a valid programme; stderr: {stderr}"
    );
    let explanation: Value =
        serde_json::from_str(&stdout).expect("explain --json stdout must be JSON");
    assert!(
        explanation.get("verdict").is_some(),
        "Explanation envelope must carry a `verdict` field; got: {stdout}"
    );
}

/// `Subject` is Morpholog's only primitive noun: it carries both
/// minted entity identifiers (UUIDv7 by runtime convention) and
/// domain symbols (commodity codes, period names, account
/// codes, direction enums). The `--args-named` codec accepts
/// any string for Subject parameters, mirroring the kernel's
/// opaque-subject model. This test pins that natural-symbol
/// Subjects work end-to-end - exactly the shape the embedder
/// integration doc's `commodity:"oil"` / `direction:"buy"`
/// examples rely on, and the shape that an earlier UUID-only
/// validation broke.
#[tokio::test(flavor = "current_thread")]
async fn run_args_named_accepts_symbolic_subject_values() {
    reset_db().await;
    let path = write_temp_ledger_morph();
    let args_named = r#"{
        "entry_id":"entry_42",
        "posting_date":"2026-04-15",
        "period":"q1_2026",
        "debit_account":"account_cash",
        "credit_account":"account_revenue",
        "amount":"100"
    }"#;
    let (status, stdout, stderr) = run_cli(&[
        "propose",
        path.to_str().unwrap(),
        "post_simple_entry",
        "--actor",
        "alex",
        "--args-named",
        args_named,
    ]);
    assert!(
        status.success(),
        "symbolic Subject values must work in --args-named; \
         stderr: {stderr}; stdout: {stdout}"
    );
    let receipt: Value = serde_json::from_str(&stdout).expect("receipt is JSON");
    assert_eq!(receipt["status"], "committed");
}

/// Decimal strings that fail the schema's pattern must also fail
/// the `--args-named` codec, or the embedder validates request
/// bodies against a stricter contract than the CLI actually
/// enforces. The schema pattern is `^-?(0|[1-9]\d*)(\.\d+)?$`;
/// `Decimal::from_str` alone is more lenient. This test pins the
/// alignment.
#[tokio::test(flavor = "current_thread")]
async fn run_args_named_decimal_outside_schema_pattern_errors() {
    reset_db().await;
    let path = write_temp_ledger_morph();
    for bad in ["+1", "00.12", "1.", ".5"] {
        let args_named = format!(
            r#"{{
                "entry_id":"018f0000-0000-7000-8000-000000000041",
                "posting_date":"018f0000-0000-7000-8000-000000000042",
                "period":"018f0000-0000-7000-8000-000000000043",
                "debit_account":"018f0000-0000-7000-8000-000000000044",
                "credit_account":"018f0000-0000-7000-8000-000000000045",
                "amount":"{bad}"
            }}"#
        );
        propose_named_expect_stderr_at(&path, &args_named, &["does not match the schema pattern"]);
    }
}

/// Wrong JSON type errors with the expected kind label so the
/// embedder can see WHICH parameter went wrong and WHAT kind it
/// should be.
#[tokio::test(flavor = "current_thread")]
async fn run_args_named_wrong_type_errors_with_kind_label() {
    propose_named_expect_stderr(
        r#"{
            "entry_id":"018f0000-0000-7000-8000-000000000051",
            "posting_date":"018f0000-0000-7000-8000-000000000052",
            "period":"018f0000-0000-7000-8000-000000000053",
            "debit_account":"018f0000-0000-7000-8000-000000000054",
            "credit_account":"018f0000-0000-7000-8000-000000000055",
            "amount": true
        }"#,
        &["`amount` is Decimal but received boolean"],
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn run_errors_with_available_list_on_unknown_transformation() {
    reset_db().await;
    let path = write_temp_ledger_morph();
    let args_json = r#"[]"#;
    let (status, _stdout, stderr) = run_cli(&[
        "propose",
        path.to_str().unwrap(),
        "no_such_transformation",
        "--actor",
        "alex",
        "--args",
        args_json,
    ]);
    assert!(
        !status.success(),
        "unknown transformation must exit non-zero"
    );
    assert!(
        stderr.contains("no_such_transformation"),
        "stderr should name the missing transformation; got: {stderr}"
    );
    assert!(
        stderr.contains("post_simple_entry"),
        "stderr should list the available transformations; got: {stderr}"
    );
}

/// Write a temp .morph whose programme intentionally exposes a
/// `post_unbalanced_entry` transformation that can produce an
/// invariant-violating candidate state. Lets us prove that `run`
/// preserves the same committed-vs-rejected semantics as `propose`
/// when a kernel invariant rejects the candidate.
fn write_temp_ledger_morph_with_unbalanced_path() -> std::path::PathBuf {
    let body = r#"
program temp_ledger_unbalanced

predicate JournalEntry(entry_id: Subject, posting_date: Subject, period: Subject)
predicate JournalLine(entry_id: Subject, account: Subject, debit_amount: Decimal, credit_amount: Decimal)

intent JournalEntryPosted(entry_id: Subject)

invariant balanced_posted_entry:
    JournalEntry(entry, _, _) implies (sum(d | JournalLine(entry, _, d, _)) = sum(c | JournalLine(entry, _, _, c)))

transformation post_unbalanced_entry(entry_id, posting_date, period, debit_account, debit_amount, credit_account, credit_amount):
    admit JournalEntry(entry_id, posting_date, period)
    admit JournalLine(entry_id, debit_account, debit_amount, 0)
    admit JournalLine(entry_id, credit_account, 0, credit_amount)
    emit JournalEntryPosted(entry_id)
"#;
    let dir =
        std::env::temp_dir().join(format!("morpholog_run_unbalanced_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("ledger.morph");
    std::fs::write(&path, body).expect("write temp .morph");
    path
}

#[tokio::test(flavor = "current_thread")]
async fn run_rejects_unbalanced_entry_via_invariant() {
    reset_db().await;
    let path = write_temp_ledger_morph_with_unbalanced_path();
    // 100 debit, 90 credit - the candidate state has an unbalanced
    // JournalEntry, so balanced_posted_entry must reject it.
    let args_json = r#"[
        {"type":"subject","value":"unbal_001"},
        {"type":"subject","value":"2026-04-15"},
        {"type":"subject","value":"q1_2026"},
        {"type":"subject","value":"account_cash"},
        {"type":"decimal","value":"100"},
        {"type":"subject","value":"account_revenue"},
        {"type":"decimal","value":"90"}
    ]"#;
    let (status, stdout, stderr) = run_cli(&[
        "propose",
        path.to_str().unwrap(),
        "post_unbalanced_entry",
        "--actor",
        "alex",
        "--args",
        args_json,
    ]);
    // Same exit-code semantics as `propose`: rejected business
    // outcome is exit 1 with the receipt on stdout, not stderr.
    assert!(
        !status.success(),
        "unbalanced entry must be rejected; stderr: {stderr}"
    );
    let receipt: Value = serde_json::from_str(&stdout).expect("rejection receipt is JSON");
    assert_eq!(receipt["status"], "rejected");
    assert!(
        receipt["reason"]
            .as_str()
            .unwrap_or("")
            .contains("balanced_posted_entry"),
        "rejection reason should name the failing invariant; got: {}",
        receipt["reason"]
    );
    // The courtesy location line: stderr points at the violated
    // rule's declaration in the source (the invariant sits at 9:1 in
    // the temp programme). Stderr only - the stdout envelope above
    // already parsed as the same pinned receipt shape.
    assert!(
        stderr.contains("rule at") && stderr.contains(":9:1 (balanced_posted_entry)"),
        "stderr should locate the violated rule; got: {stderr}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn run_rejects_parse_failure_in_user_morph() {
    reset_db().await;
    // Write a deliberately malformed .morph.
    let dir = std::env::temp_dir().join(format!("morpholog_run_bad_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("bad.morph");
    std::fs::write(&path, "program is_invalid syntax here\n").expect("write bad .morph");

    let (status, _stdout, stderr) = run_cli(&[
        "propose",
        path.to_str().unwrap(),
        "anything",
        "--actor",
        "alex",
        "--args",
        "[]",
    ]);
    assert!(!status.success(), "parse failure must exit non-zero");
    assert!(
        !stderr.is_empty(),
        "parse failure should write diagnostics to stderr"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn run_with_trace_emits_structured_trace_alongside_outcome() {
    reset_db().await;
    let path = write_temp_ledger_morph();
    let args_json = &ledger_args_json("entry_002", "2026-04-15", "q1_2026", "50");
    let (status, stdout, _stderr) = run_cli(&[
        "propose",
        path.to_str().unwrap(),
        "post_simple_entry",
        "--actor",
        "alex",
        "--args",
        args_json,
        "--trace",
    ]);
    assert!(status.success(), "trace happy path should succeed");
    let json: Value = serde_json::from_str(&stdout).expect("trace output is JSON");
    assert!(
        json["result"].is_object(),
        "trace output must wrap the result"
    );
    assert!(
        json["trace"].is_array(),
        "trace output must carry a trace array"
    );
    assert_eq!(json["result"]["status"], "committed");
}

// ============================================================
// `morpholog outbox` subcommands (claim / complete / release)
//
// These exercise the lease protocol end-to-end against a real
// outbox row created by `propose post_simple_entry`. Each test
// resets the database and admits one journal entry so that a
// JournalEntryPosted intent lands in the outbox.
// ============================================================

/// Seed: propose one balanced entry so the outbox has a row to claim.
/// Returns the intent_type that the worked example emits.
fn seed_one_outbox_row() -> &'static str {
    let _tid = post_balanced_entry("seed_entry", 1_000);
    "JournalEntryPosted"
}

#[tokio::test(flavor = "current_thread")]
async fn outbox_claim_returns_null_when_outbox_is_empty() {
    reset_db().await;
    let (status, stdout, stderr) =
        run_cli(&["outbox", "claim", "--intent-type", "JournalEntryPosted"]);
    assert!(
        status.success(),
        "empty-outbox claim must exit 0; stderr: {stderr}"
    );
    let json: Value = serde_json::from_str(&stdout).expect("claim output is JSON");
    assert!(
        json["row"].is_null(),
        "empty outbox should return {{\"row\": null}}; got: {json}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn outbox_claim_claims_a_pending_row_and_reports_worker_id() {
    reset_db().await;
    let intent_type = seed_one_outbox_row();
    let (status, stdout, stderr) = run_cli(&["outbox", "claim", "--intent-type", intent_type]);
    assert!(status.success(), "claim should succeed; stderr: {stderr}");
    let json: Value = serde_json::from_str(&stdout).expect("claim output is JSON");
    let row = &json["row"];
    assert!(!row.is_null(), "outbox had a row; claim should not be null");
    assert_eq!(row["intent_type"], intent_type);
    assert_eq!(row["status"], "in_progress");
    assert!(
        row["locked_by"].is_string(),
        "locked_by should carry the generated worker_id"
    );
    assert!(
        row["lock_expires_at"].is_string(),
        "lock_expires_at should be set on a claimed row"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn outbox_claim_with_supplied_worker_id_uses_that_id() {
    reset_db().await;
    let intent_type = seed_one_outbox_row();
    let (status, stdout, _stderr) = run_cli(&[
        "outbox",
        "claim",
        "--intent-type",
        intent_type,
        "--worker-id",
        "my-python-worker-7",
    ]);
    assert!(status.success());
    let json: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json["row"]["locked_by"], "my-python-worker-7");
}

#[tokio::test(flavor = "current_thread")]
async fn outbox_complete_delivered_marks_row_delivered() {
    reset_db().await;
    let intent_type = seed_one_outbox_row();
    let claim_out: Value =
        serde_json::from_str(&run_cli(&["outbox", "claim", "--intent-type", intent_type]).1)
            .unwrap();
    let intent_id = claim_out["row"]["intent_id"].as_str().unwrap().to_string();
    let worker_id = claim_out["row"]["locked_by"].as_str().unwrap().to_string();

    let (status, stdout, stderr) = run_cli(&[
        "outbox",
        "complete",
        &intent_id,
        "--worker-id",
        &worker_id,
        "--outcome",
        "delivered",
    ]);
    assert!(
        status.success(),
        "delivered complete should exit 0; stderr: {stderr}"
    );
    let json: Value = serde_json::from_str(&stdout).expect("complete output is JSON");
    assert_eq!(json, serde_json::json!({"status": "applied"}));
}

#[tokio::test(flavor = "current_thread")]
async fn outbox_complete_transient_reschedules_row_to_pending() {
    reset_db().await;
    let intent_type = seed_one_outbox_row();
    let claim_out: Value =
        serde_json::from_str(&run_cli(&["outbox", "claim", "--intent-type", intent_type]).1)
            .unwrap();
    let intent_id = claim_out["row"]["intent_id"].as_str().unwrap().to_string();
    let worker_id = claim_out["row"]["locked_by"].as_str().unwrap().to_string();

    let (status, _stdout, stderr) = run_cli(&[
        "outbox",
        "complete",
        &intent_id,
        "--worker-id",
        &worker_id,
        "--outcome",
        "transient",
        "--retry-after-seconds",
        "60",
    ]);
    assert!(
        status.success(),
        "transient complete should exit 0; stderr: {stderr}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn outbox_complete_transient_requires_retry_after_seconds() {
    reset_db().await;
    let (status, _stdout, stderr) = run_cli(&[
        "outbox",
        "complete",
        &uuid::Uuid::now_v7().to_string(),
        "--worker-id",
        "any",
        "--outcome",
        "transient",
    ]);
    assert!(
        !status.success(),
        "missing --retry-after-seconds must error"
    );
    assert!(
        stderr.contains("retry-after-seconds"),
        "error should name the missing flag; got: {stderr}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn outbox_complete_failed_records_reason() {
    reset_db().await;
    let intent_type = seed_one_outbox_row();
    let claim_out: Value =
        serde_json::from_str(&run_cli(&["outbox", "claim", "--intent-type", intent_type]).1)
            .unwrap();
    let intent_id = claim_out["row"]["intent_id"].as_str().unwrap().to_string();
    let worker_id = claim_out["row"]["locked_by"].as_str().unwrap().to_string();

    let (status, _stdout, stderr) = run_cli(&[
        "outbox",
        "complete",
        &intent_id,
        "--worker-id",
        &worker_id,
        "--outcome",
        "failed",
        "--reason",
        "downstream returned 4xx",
    ]);
    assert!(
        status.success(),
        "failed complete should exit 0; stderr: {stderr}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn outbox_complete_with_wrong_worker_id_exits_one_with_lease_lost() {
    reset_db().await;
    let intent_type = seed_one_outbox_row();
    let claim_out: Value =
        serde_json::from_str(&run_cli(&["outbox", "claim", "--intent-type", intent_type]).1)
            .unwrap();
    let intent_id = claim_out["row"]["intent_id"].as_str().unwrap().to_string();

    let (status, stdout, _stderr) = run_cli(&[
        "outbox",
        "complete",
        &intent_id,
        "--worker-id",
        "not-the-lease-holder",
        "--outcome",
        "delivered",
    ]);
    assert!(!status.success(), "wrong worker_id must exit non-zero");
    let json: Value = serde_json::from_str(&stdout).expect("LeaseLost output is JSON");
    assert_eq!(json, serde_json::json!({"status": "lease_lost"}));
}

#[tokio::test(flavor = "current_thread")]
async fn outbox_release_puts_a_claimed_row_back_to_pending() {
    reset_db().await;
    let intent_type = seed_one_outbox_row();
    let claim_out: Value =
        serde_json::from_str(&run_cli(&["outbox", "claim", "--intent-type", intent_type]).1)
            .unwrap();
    let intent_id = claim_out["row"]["intent_id"].as_str().unwrap().to_string();
    let worker_id = claim_out["row"]["locked_by"].as_str().unwrap().to_string();

    let (status, stdout, stderr) =
        run_cli(&["outbox", "release", &intent_id, "--worker-id", &worker_id]);
    assert!(status.success(), "release should exit 0; stderr: {stderr}");
    let json: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json, serde_json::json!({"status": "applied"}));

    // And the row is claimable again.
    let (status, stdout, _stderr) = run_cli(&["outbox", "claim", "--intent-type", intent_type]);
    assert!(status.success());
    let json: Value = serde_json::from_str(&stdout).unwrap();
    assert!(
        !json["row"].is_null(),
        "released row should be reclaimable; got: {json}"
    );
}

// ============================================================
// `inspect outbox` filters
//
// `--status pending` is the default (matches the operational
// "what is waiting" question); the new filters expose the rest.
// ============================================================

/// Helper for the filter tests: seed the outbox with two pending
/// rows so the filter assertions can distinguish "all" from
/// "delivered" / "failed".
fn seed_two_pending_outbox_rows() {
    let _ = post_balanced_entry("filter_seed_a", 100);
    let _ = post_balanced_entry("filter_seed_b", 200);
}

/// One pending row and one delivered row, so the status filters can
/// tell pending / delivered / all apart.
fn seed_one_pending_and_one_delivered_row() {
    seed_two_pending_outbox_rows();
    let claim_out: Value = serde_json::from_str(
        &run_cli(&["outbox", "claim", "--intent-type", "JournalEntryPosted"]).1,
    )
    .unwrap();
    let intent_id = claim_out["row"]["intent_id"].as_str().unwrap().to_string();
    let worker_id = claim_out["row"]["locked_by"].as_str().unwrap().to_string();
    let (s, _, _) = run_cli(&[
        "outbox",
        "complete",
        &intent_id,
        "--worker-id",
        &worker_id,
        "--outcome",
        "delivered",
    ]);
    assert!(s.success());
}

/// No `--status` defaults to pending; `all` and `delivered` expose
/// the rest. One pending + one delivered row; each filter's view is
/// pinned as the exact (sorted) status list it must return.
#[tokio::test(flavor = "current_thread")]
async fn inspect_outbox_status_filters_partition_the_rows() {
    reset_db().await;
    seed_one_pending_and_one_delivered_row();
    let cases: [(&[&str], &[&str]); 3] = [
        (&[], &["pending"]),
        (&["--status", "all"], &["delivered", "pending"]),
        (&["--status", "delivered"], &["delivered"]),
    ];
    for (flags, expected_statuses) in cases {
        let mut argv = vec!["inspect", "outbox"];
        argv.extend_from_slice(flags);
        let (status, stdout, _stderr) = run_cli(&argv);
        assert!(status.success());
        let rows: Value = serde_json::from_str(&stdout).unwrap();
        let arr = rows.as_array().expect("inspect outbox emits a JSON array");
        let mut statuses: Vec<&str> = arr
            .iter()
            .map(|row| row["status"].as_str().expect("status is a string"))
            .collect();
        statuses.sort_unstable();
        assert_eq!(statuses, expected_statuses, "flags {flags:?}; got {arr:?}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_outbox_intent_type_filter_narrows_results() {
    reset_db().await;
    seed_two_pending_outbox_rows();
    let (status, stdout, _stderr) = run_cli(&[
        "inspect",
        "outbox",
        "--status",
        "all",
        "--intent-type",
        "DoesNotExist",
    ]);
    assert!(status.success(), "unknown intent_type is not an error");
    let rows: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 0);

    let (status, stdout, _stderr) =
        run_cli(&["inspect", "outbox", "--intent-type", "JournalEntryPosted"]);
    assert!(status.success());
    let rows: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 2);
}

// ============================================================
// End-to-end compute loop
//
// The product test: prove that a non-Rust consumer can drive the
// whole input/commit/outbox loop using only the `morpholog` binary.
// This pins the contract the docs describe as "round-trip compute".
// ============================================================

#[tokio::test(flavor = "current_thread")]
async fn compute_loop_end_to_end_via_cli_binary_only() {
    reset_db().await;

    // 1. INPUT BOUNDARY: a Python-shaped consumer writes its own
    //    `.morph` file and invokes `morpholog run` to admit a
    //    transformation against PostgreSQL - no Rust, no baked-in
    //    programmes, just a file path.
    let path = write_temp_ledger_morph();
    let args_json = &ledger_args_json("e2e_entry", "2026-05-01", "q2_2026", "500");
    let (status, stdout, stderr) = run_cli(&[
        "propose",
        path.to_str().unwrap(),
        "post_simple_entry",
        "--actor",
        "python_worker",
        "--args",
        args_json,
    ]);
    assert!(status.success(), "run should succeed; stderr: {stderr}");
    let receipt: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(receipt["status"], "committed");

    // 2. OUTPUT BOUNDARY: the consumer claims the resulting outbox
    //    row. The intent type matches what the transformation emits.
    let (status, stdout, _stderr) =
        run_cli(&["outbox", "claim", "--intent-type", "JournalEntryPosted"]);
    assert!(status.success());
    let claim: Value = serde_json::from_str(&stdout).unwrap();
    let row = &claim["row"];
    assert!(!row.is_null(), "the run above should have enqueued one row");
    let intent_id = row["intent_id"].as_str().unwrap().to_string();
    let worker_id = row["locked_by"].as_str().unwrap().to_string();
    assert_eq!(row["intent_type"], "JournalEntryPosted");
    assert_eq!(row["status"], "in_progress");

    // 3. COMPUTE PHASE: the consumer does whatever external work
    //    the intent represents (here a no-op stand-in - the test's
    //    point is that the kernel does not care what happens
    //    between claim and complete).

    // 4. CONSUMER MARKS DELIVERED.
    let (status, stdout, stderr) = run_cli(&[
        "outbox",
        "complete",
        &intent_id,
        "--worker-id",
        &worker_id,
        "--outcome",
        "delivered",
    ]);
    assert!(
        status.success(),
        "complete should succeed; stderr: {stderr}"
    );
    let upd: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(upd, serde_json::json!({"status": "applied"}));

    // 5. INSPECT: the same consumer (or an auditor) verifies the row
    //    is in the delivered slice.
    let (status, stdout, _stderr) = run_cli(&["inspect", "outbox", "--status", "delivered"]);
    assert!(status.success());
    let delivered: Value = serde_json::from_str(&stdout).unwrap();
    let arr = delivered.as_array().unwrap();
    assert_eq!(arr.len(), 1, "exactly one delivered row");
    assert_eq!(arr[0]["intent_id"], intent_id);

    // 6. AND IT IS NO LONGER PENDING.
    let (status, stdout, _stderr) = run_cli(&["inspect", "outbox"]);
    assert!(status.success());
    let pending: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        pending.as_array().unwrap().len(),
        0,
        "no rows should remain pending"
    );
}

// ============================================================
// File-path subcommands validate before acting
// ============================================================

/// Write a temp `.morph` that parses but fails `Program::validate()`:
/// the invariant references an undeclared predicate `Bar`. Now that the
/// CLI parses arbitrary files, the file-path subcommands must hold them
/// to the vocabulary contract.
fn write_temp_invalid_morph() -> std::path::PathBuf {
    let body = r#"
program temp_invalid

predicate Foo(x: Subject)

invariant references_undeclared:
    Bar(x) implies Foo(x)
"#;
    let dir = std::env::temp_dir().join(format!("morpholog_invalid_test_{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let path = dir.join("invalid.morph");
    std::fs::write(&path, body).expect("write temp .morph");
    path
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_derived_validates_before_touching_the_database() {
    // A parseable-but-invalid programme (undeclared predicate) is refused
    // with validation diagnostics before the derived lookup or any
    // database connection - the same gate `run` applies.
    let path = write_temp_invalid_morph();
    let (status, _stdout, stderr) =
        run_cli(&["inspect", "derived", path.to_str().unwrap(), "AnyDerived"]);
    assert!(
        !status.success(),
        "inspect derived on an invalid programme must exit non-zero"
    );
    assert!(
        stderr.contains("Bar"),
        "stderr should name the undeclared predicate: {stderr}"
    );
}

// ============================================================
// `init` - schema provisioning from the embedded schema
// ============================================================

#[tokio::test(flavor = "current_thread")]
async fn init_provisions_then_refuses_then_skips() {
    // This test owns the whole schema lifecycle: drop it, provision it
    // through the binary, prove the provisioned schema actually works,
    // then pin both already-initialised behaviours. Safe in this
    // serial suite - every other test only TRUNCATEs.
    let pool = PgPool::connect(&database_url()).await.unwrap();
    sqlx::raw_sql("DROP SCHEMA IF EXISTS morpholog CASCADE")
        .execute(&pool)
        .await
        .expect("drop schema");

    let (status, stdout, stderr) = run_cli(&["init"]);
    assert!(status.success(), "init should provision; stderr:\n{stderr}");
    let v: Value = serde_json::from_str(&stdout).expect("init output is JSON");
    assert_eq!(v["status"], "initialised");

    // The provisioned schema is the real one: a governed commit works.
    post_balanced_entry("entry_001", 100);

    // Re-running refuses, with the remedy named.
    let (status, _stdout, stderr) = run_cli(&["init"]);
    assert!(!status.success(), "second init must refuse");
    assert!(
        stderr.contains("--skip-if-exists"),
        "the refusal names the entrypoint escape hatch: {stderr}"
    );

    // The escape hatch: report and exit zero.
    let (status, stdout, _stderr) = run_cli(&["init", "--skip-if-exists"]);
    assert!(status.success(), "skip-if-exists exits zero");
    let v: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["status"], "already-initialised");

    // --reset is destructive, so the acknowledgement is the contract:
    // refused without it, and refused the other way round too, so a
    // stray ack in a script cannot lie in wait for a later --reset.
    let (status, _stdout, stderr) = run_cli(&["init", "--reset"]);
    assert!(
        !status.success(),
        "--reset without the acknowledgement must refuse"
    );
    assert!(
        stderr.contains("--i-know-this-deletes-data") && stderr.contains(&database_url()),
        "the refusal names the flag AND the target it would have destroyed: {stderr}"
    );
    let (status, _stdout, stderr) = run_cli(&["init", "--i-know-this-deletes-data"]);
    assert!(!status.success(), "the acknowledgement alone must refuse");
    assert!(stderr.contains("only meaningful with --reset"), "{stderr}");

    // Data first, so the reset is proven to have really dropped it
    // rather than to have quietly no-oped on an empty schema.
    post_balanced_entry("entry_before_reset", 250);
    let (status, stdout, stderr) = run_cli(&["init", "--reset", "--i-know-this-deletes-data"]);
    assert!(
        status.success(),
        "acknowledged reset provisions; stderr:\n{stderr}"
    );
    let v: Value = serde_json::from_str(&stdout).expect("reset output is JSON");
    assert_eq!(
        v["status"], "initialised",
        "a reset re-provisions from scratch"
    );
    assert!(
        stderr.contains("dropped the pre-existing"),
        "the report distinguishes dropping from finding nothing: {stderr}"
    );
    // The re-provisioned schema is usable, and empty.
    post_balanced_entry("entry_after_reset", 100);
    let pool = PgPool::connect(&database_url()).await.unwrap();
    let entries: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM morpholog.claims WHERE predicate_name = 'JournalEntry'",
    )
    .fetch_one(&pool)
    .await
    .expect("count claims");
    assert_eq!(
        entries, 1,
        "only the post-reset entry survives - the reset really dropped the data"
    );

    // On a database with no schema, the same command reports honestly
    // rather than implying it removed something.
    sqlx::raw_sql("DROP SCHEMA IF EXISTS morpholog CASCADE")
        .execute(&pool)
        .await
        .expect("drop schema");
    let (status, _stdout, stderr) = run_cli(&["init", "--reset", "--i-know-this-deletes-data"]);
    assert!(status.success());
    assert!(
        stderr.contains("found no"),
        "with nothing to drop the report says so: {stderr}"
    );
}

// ============================================================
// `run --explain-on-reject` - same-snapshot diagnosis
// ============================================================

#[tokio::test(flavor = "current_thread")]
async fn run_explain_on_reject_attaches_the_same_snapshot_explanation() {
    reset_db().await;
    // Close the period, then propose into it with the flag: the
    // rejection envelope carries the explanation computed against the
    // exact pre-state the gate evaluated.
    let (status, _o, _e) = run_cli(&[
        "propose",
        &ledger_morph(),
        "close_period",
        "--actor",
        "alex",
        "--args",
        r#"[{"type":"subject","value":"q1_2026"}]"#,
    ]);
    assert!(status.success());

    let (status, stdout, _stderr) = run_cli(&[
        "propose",
        &ledger_morph(),
        "post_simple_entry",
        "--actor",
        "alex",
        "--explain-on-reject",
        "--args",
        &ledger_args_json("entry_001", "2026-04-15", "q1_2026", "100"),
    ]);
    assert!(!status.success(), "rejection still exits one");
    let v: Value = serde_json::from_str(&stdout).expect("envelope is JSON");
    assert_eq!(v["status"], "rejected");
    assert!(v["reason"].as_str().unwrap().contains("require"));
    let explanation = serde_json::to_string(&v["explanation"]);
    assert!(
        explanation.unwrap().contains("PeriodClosed"),
        "the explanation names the failed gate: {stdout}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn run_explain_on_reject_leaves_committed_envelopes_unchanged() {
    reset_db().await;
    let (status, stdout, stderr) = run_cli(&[
        "propose",
        &ledger_morph(),
        "close_period",
        "--actor",
        "alex",
        "--explain-on-reject",
        "--args",
        r#"[{"type":"subject","value":"q1_2026"}]"#,
    ]);
    assert!(status.success(), "commit path unaffected; {stderr}");
    let v: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(v["status"], "committed");
    assert!(
        v.get("explanation").is_none(),
        "an admitted change carries no admissibility diagnosis: {stdout}"
    );
}

// ============================================================
// `inspect claims --named` - vocabulary-decoded reads
// ============================================================

#[tokio::test(flavor = "current_thread")]
async fn inspect_claims_named_decodes_args_by_declared_field_name() {
    reset_db().await;
    post_balanced_entry("entry_001", 100);

    let ledger = ledger_morph();
    let (status, stdout, stderr) = run_cli(&[
        "inspect",
        "claims",
        "--predicate",
        "JournalLine",
        "--named",
        &ledger,
    ]);
    assert!(status.success(), "named read should succeed; {stderr}");
    let rows: Value = serde_json::from_str(&stdout).expect("stdout is JSON");
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2, "two journal lines: {stdout}");
    let debit = rows
        .iter()
        .find(|r| r["args"]["debit_amount"] == "100")
        .expect("the debit line, decoded by field name");
    assert_eq!(debit["predicate"], "JournalLine");
    assert_eq!(debit["args"]["entry_id"], "entry_001");
    assert_eq!(
        debit["args"]["credit_amount"], "0",
        "decimals stay strings - the named codec's exactness rule, mirrored"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_claims_named_hard_errors_on_programme_database_skew() {
    reset_db().await;
    post_balanced_entry("entry_001", 100);

    // A programme whose vocabulary does not declare the claims in the
    // database: the named read refuses by name, never silently skips.
    let mut other = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(
        &mut other,
        b"program other\npredicate Unrelated(x: Subject)\n",
    )
    .unwrap();
    let (status, _stdout, stderr) = run_cli(&[
        "inspect",
        "claims",
        "--named",
        other.path().to_str().unwrap(),
    ]);
    assert!(!status.success(), "skew must be a hard error");
    assert!(
        stderr.contains("not declared") && stderr.contains("skew"),
        "the error names the condition: {stderr}"
    );

    // Same vocabulary, wrong arity: also skew, naming both arities.
    let mut wrong_arity = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(
        &mut wrong_arity,
        b"program other\n\
          predicate JournalEntry(entry_id: Subject, posting_date: Subject)\n\
          predicate JournalLine(entry_id: Subject, account: Subject, debit_amount: Decimal, credit_amount: Decimal)\n\
          predicate PeriodClosed(period: Subject)\n\
          predicate Supersedes(new_entry_id: Subject, prior_entry_id: Subject)\n\
          predicate TrialBalanceRow(account: Subject, balance: Decimal)\n",
    )
    .unwrap();
    let (status, _stdout, stderr) = run_cli(&[
        "inspect",
        "claims",
        "--predicate",
        "JournalEntry",
        "--named",
        wrong_arity.path().to_str().unwrap(),
    ]);
    assert!(!status.success(), "arity skew must be a hard error");
    assert!(
        stderr.contains("arity 3") && stderr.contains("arity 2"),
        "the error names both arities: {stderr}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn quantity_params_flow_bare_through_the_named_codec_end_to_end() {
    reset_db().await;

    // A minimal unit-tagged model: settlements in USD.
    let mut model = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(
        &mut model,
        b"program quantities\n\
          predicate Settled(settlement: Subject, amount: Decimal[USD])\n\
          transformation settle(settlement, amount):\n    \
              admit Settled(settlement, amount)\n",
    )
    .unwrap();
    let path = model.path().to_str().unwrap();

    // The schema carries the unit as the machine-readable extension
    // AND in the human-readable description (form generators ignore
    // custom extensions), while the wire shape stays the bare decimal
    // pattern - the declaration is the single source of truth.
    // `schema` is static (no database flag), so it bypasses run_cli.
    let output = Command::new(common::bin())
        .args(["schema", path, "settle"])
        .output()
        .expect("spawn morpholog binary");
    let (status, stdout, stderr) = (
        output.status,
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    );
    assert!(status.success(), "schema failed: {stderr}");
    let schema: Value = serde_json::from_str(&stdout).unwrap();
    let amount = &schema["properties"]["amount"];
    assert_eq!(amount["x-morpholog-unit"], "USD");
    assert_eq!(amount["type"], "string");
    assert!(
        amount["description"].as_str().unwrap().contains("USD"),
        "unit in description: {amount}"
    );

    // Named codec in: the bare amount, no unit on the wire.
    let (status, _stdout, stderr) = run_cli(&[
        "propose",
        path,
        "settle",
        "--actor",
        "ops",
        "--args-named",
        r#"{"settlement":"s1","amount":"137500.00"}"#,
    ]);
    assert!(status.success(), "named-codec run failed: {stderr}");

    // Named read out: the same bare amount, decoded by field name.
    let (status, stdout, stderr) = run_cli(&[
        "inspect",
        "claims",
        "--predicate",
        "Settled",
        "--named",
        path,
    ]);
    assert!(status.success(), "named read failed: {stderr}");
    let rows: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(rows[0]["args"]["amount"], "137500.00", "{stdout}");

    // Tagged codec in: self-describing, the unit rides the value.
    let (status, _stdout, stderr) = run_cli(&[
        "propose",
        path,
        "settle",
        "--actor",
        "ops",
        "--args",
        r#"[{"type":"subject","value":"s2"},{"type":"quantity","value":{"amount":"1","unit":"USD"}}]"#,
    ]);
    assert!(status.success(), "tagged-codec run failed: {stderr}");
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_claims_named_errors_on_undeclared_requested_predicate() {
    reset_db().await;
    post_balanced_entry("entry_001", 100);

    // The bare read keeps claims-table-as-authority: an unknown
    // requested predicate matches nothing and yields an empty result.
    let (status, stdout, stderr) = run_cli(&["inspect", "claims", "--predicate", "JornalLine"]);
    assert!(status.success(), "bare read tolerates the typo; {stderr}");
    let rows: Value = serde_json::from_str(&stdout).expect("stdout is JSON");
    assert_eq!(rows.as_array().unwrap().len(), 0, "typo matches nothing");

    // With `--named`, the programme is the authority for the request
    // too: the same typo is a hard error naming the declared
    // vocabulary, raised before any database read.
    let ledger = ledger_morph();
    let (status, _stdout, stderr) = run_cli(&[
        "inspect",
        "claims",
        "--predicate",
        "JornalLine",
        "--named",
        &ledger,
    ]);
    assert!(
        !status.success(),
        "a typoed requested predicate must be a hard error under --named"
    );
    assert!(
        stderr.contains("JornalLine") && stderr.contains("not declared"),
        "the error names the typo: {stderr}"
    );
    assert!(
        stderr.contains("JournalLine"),
        "the error lists the declared vocabulary: {stderr}"
    );
}

// ============================================================
// run --batch: rows in, a receipt per row, each its own commit.
// ============================================================

/// `run --batch` with NDJSON rows on a temp file. Returns
/// (status, receipts-parsed-from-stdout, stderr).
fn run_batch(rows: &str, extra: &[&str]) -> (std::process::ExitStatus, Vec<Value>, String) {
    let f = tempfile::NamedTempFile::new().expect("temp batch file");
    std::fs::write(f.path(), rows).expect("write batch rows");
    let path = f.path().to_str().expect("utf8 path").to_string();
    let ledger = ledger_morph();
    let mut args = vec!["propose", ledger.as_str(), "--batch", path.as_str()];
    args.extend_from_slice(extra);
    let (status, stdout, stderr) = run_cli(&args);
    let receipts = stdout
        .lines()
        .map(|l| serde_json::from_str(l).expect("receipt is one JSON object per line"))
        .collect();
    (status, receipts, stderr)
}

fn ledger_row(transformation: &str, actor: &str, named: Value) -> String {
    serde_json::json!({
        "transformation": transformation,
        "actor": actor,
        "args_named": named,
    })
    .to_string()
}

// The batch contract in one run: commits, a blank line skipped, a
// malformed row turned into an error receipt, a lawful rejection, and
// a row AFTER the failures still committing - with exit 0, because
// every row produced a receipt.
#[tokio::test]
async fn batch_rows_are_independent_and_every_row_gets_a_receipt() {
    reset_db().await;
    let rows = [
        ledger_row(
            "post_simple_entry",
            "jordan",
            serde_json::json!({
                "entry_id": "b1", "posting_date": "d1", "period": "p1",
                "debit_account": "cash", "credit_account": "rev", "amount": "100"
            }),
        ),
        String::new(),
        ledger_row("close_period", "maria", serde_json::json!({"period": "p1"})),
        "this is not json".to_string(),
        // Posting into the closed period: a lawful rejection.
        ledger_row(
            "post_simple_entry",
            "jordan",
            serde_json::json!({
                "entry_id": "b2", "posting_date": "d2", "period": "p1",
                "debit_account": "cash", "credit_account": "rev", "amount": "50"
            }),
        ),
        // A later row still commits after the error and the rejection.
        ledger_row(
            "post_simple_entry",
            "nina",
            serde_json::json!({
                "entry_id": "b3", "posting_date": "d3", "period": "p2",
                "debit_account": "cash", "credit_account": "rev", "amount": "75"
            }),
        ),
    ]
    .join("\n");

    let (status, receipts, stderr) = run_batch(&rows, &[]);
    assert!(
        status.success(),
        "receipts for every row mean exit 0: {stderr}"
    );
    assert_eq!(receipts.len(), 5, "blank line yields no receipt");

    let statuses: Vec<&str> = receipts
        .iter()
        .map(|r| r["status"].as_str().expect("status"))
        .collect();
    assert_eq!(
        statuses,
        vec!["committed", "committed", "error", "rejected", "committed"]
    );
    // `row` is the 1-based input line number, so receipts map back to
    // the file even with blank lines skipped.
    let rows_field: Vec<u64> = receipts
        .iter()
        .map(|r| r["row"].as_u64().expect("row"))
        .collect();
    assert_eq!(rows_field, vec![1, 3, 4, 5, 6]);
    // Per-row actors land in the receipts (and so in the audit rows).
    assert_eq!(receipts[1]["actor"]["value"], "maria");
    assert_eq!(receipts[4]["actor"]["value"], "nina");
    assert!(
        stderr.contains("5 rows - 3 committed, 1 rejected, 1 errors"),
        "summary on stderr: {stderr}"
    );
    // The single-run rule-location courtesy line stays out of batch
    // mode: receipts are the machine contract, and stderr carries
    // only the summary.
    assert!(
        !stderr.contains("rule at"),
        "no rule-location lines in batch mode; got: {stderr}"
    );

    // The batch's one lawful rejection landed in the rejection log -
    // batch rows record through the same single site as single runs.
    let (status, stdout, _stderr) = run_cli(&["inspect", "rejections"]);
    assert!(status.success());
    let logged: Value = serde_json::from_str(&stdout).unwrap();
    let logged = logged.as_array().unwrap();
    assert_eq!(logged.len(), 1, "one rejected row, one log row: {stdout}");
    assert_eq!(logged[0]["transformation_name"], "post_simple_entry");
    assert_eq!(logged[0]["actor"]["value"], "jordan");
}

// --explain-on-reject composes per row: the rejected row's receipt
// carries the same structured explanation `explain --json` produces.
#[tokio::test]
async fn batch_rejected_rows_carry_explanations_when_asked() {
    reset_db().await;
    let rows = [
        ledger_row("close_period", "maria", serde_json::json!({"period": "p9"})),
        ledger_row(
            "post_simple_entry",
            "jordan",
            serde_json::json!({
                "entry_id": "x1", "posting_date": "d1", "period": "p9",
                "debit_account": "cash", "credit_account": "rev", "amount": "10"
            }),
        ),
    ]
    .join("\n");
    let (status, receipts, _stderr) = run_batch(&rows, &["--explain-on-reject"]);
    assert!(status.success());
    assert_eq!(receipts[1]["status"], "rejected");
    assert!(
        receipts[1]["explanation"].is_object(),
        "the rejected row explains itself: {}",
        receipts[1]
    );
    assert!(
        receipts[0].get("explanation").is_none(),
        "committed rows are unchanged"
    );
}

// Operational failure - an unreadable batch path - is the non-zero
// case, distinct from per-row outcomes.
#[tokio::test]
async fn batch_with_unreadable_input_exits_nonzero() {
    let (status, _stdout, stderr) = run_cli(&[
        "propose",
        &ledger_morph(),
        "--batch",
        "/nonexistent/rows.ndjson",
    ]);
    assert!(!status.success());
    assert!(stderr.contains("failed to read batch rows"), "{stderr}");
}

// --trace is single-run diagnostics; clap refuses the combination.
#[tokio::test]
async fn batch_conflicts_with_trace() {
    let (status, _stdout, stderr) =
        run_cli(&["propose", &ledger_morph(), "--batch", "-", "--trace"]);
    assert!(!status.success());
    assert!(
        stderr.contains("cannot be used with"),
        "clap names the conflict: {stderr}"
    );
}

// Batch rows carry their own args; a top-level args flag would be
// silently ignored, so clap refuses both codecs' flags with --batch.
#[tokio::test]
async fn batch_conflicts_with_both_args_flags() {
    for flag in [["--args", "[]"], ["--args-named", "{}"]] {
        let (status, _stdout, stderr) =
            run_cli(&["propose", &ledger_morph(), "--batch", "-", flag[0], flag[1]]);
        assert!(!status.success(), "{} must conflict with --batch", flag[0]);
        assert!(
            stderr.contains("cannot be used with"),
            "clap names the conflict for {}: {stderr}",
            flag[0]
        );
    }
}

// ============================================================
// `inspect coverage` - which rules have ever actually done work.
// ============================================================

// Prose mode names the verdicts and carries the legend that says what
// committed history structurally cannot show; the exit code is zero
// regardless of findings (coverage answers a question - the `explain`
// stance), and never-fired rules are the point, not a failure.
#[tokio::test(flavor = "current_thread")]
async fn inspect_coverage_prose_reports_fired_and_never_fired() {
    reset_db().await;
    post_balanced_entry("entry_001", 100);
    post_balanced_entry("entry_002", 250);

    let (status, stdout, stderr) = run_cli(&["inspect", "coverage", &ledger_morph()]);
    assert!(status.success(), "coverage always exits zero; {stderr}");
    assert!(
        stdout.contains("balanced_posted_entry - fired in 2 transition(s)"),
        "the balance rule fired twice; got:\n{stdout}"
    );
    assert!(
        stdout.contains("NEVER FIRED"),
        "a two-transition history leaves rules never-fired; got:\n{stdout}"
    );
    assert!(
        stdout.contains("close_period - never used"),
        "declared-but-unused transformations are named; got:\n{stdout}"
    );
    assert!(
        stdout.contains("a floor, not a census"),
        "the legend states the rejection log's at-most-once bound; got:\n{stdout}"
    );
    assert!(
        stdout.contains("0 recorded rejection(s)"),
        "the header counts the rejection log; got:\n{stdout}"
    );
}

// The --json form: the exact field set the report promises, pinned.
#[tokio::test(flavor = "current_thread")]
async fn inspect_coverage_json_carries_the_pinned_field_set() {
    reset_db().await;
    let tid = post_balanced_entry("entry_001", 100);

    let (status, stdout, stderr) = run_cli(&["inspect", "coverage", &ledger_morph(), "--json"]);
    assert!(status.success(), "coverage always exits zero; {stderr}");
    let report: Value = serde_json::from_str(&stdout).expect("coverage --json is JSON");
    assert_eq!(report["transitions_replayed"], 1);
    assert_eq!(report["rejections_replayed"], 0);
    assert!(report["program"].is_string());

    let invariants = report["invariants"].as_array().expect("invariants array");
    let balanced = invariants
        .iter()
        .find(|i| i["invariant"] == "balanced_posted_entry")
        .expect("balance rule in report");
    assert_eq!(balanced["verdict"], "fired");
    assert_eq!(balanced["transitions_fired"], 1);
    assert_eq!(balanced["first_fired"], tid.to_string());
    assert_eq!(balanced["last_fired"], tid.to_string());
    assert!(
        invariants.iter().any(|i| i["verdict"] == "never_fired"),
        "verdicts use snake_case and never-fired rules appear: {report}"
    );

    let transformations = report["transformations"]
        .as_array()
        .expect("transformations array");
    let posting = transformations
        .iter()
        .find(|t| t["transformation"] == "post_simple_entry")
        .expect("posting transformation in report");
    assert_eq!(posting["transitions"], 1);
    assert!(
        transformations.iter().any(|t| t["transitions"] == 0),
        "declared-but-unused transformations appear at zero: {report}"
    );
}
