//! End-to-end integration tests for the `morpholog` binary.
//!
//! These spawn the built binary against a real PostgreSQL database and
//! assert on stdout JSON, stderr and exit codes. Argument parsing is
//! tested in `src/cli_tests.rs`.
//!
//! The tests use `DATABASE_URL` and truncate the schema first, so they run
//! serially without crosstalk.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{database_url, reset_db};

use std::process::Command;

use serde_json::Value;
use sqlx::PgPool;

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

/// Run `morpholog` with exactly the given args and no `--database-url`,
/// for offline subcommands such as `audit verify-pack`.
/// The verdict inside a `verify-pack` report, which always carries the
/// role-rebinding finding beside it.
fn pack_verdict(stdout: &str) -> Value {
    let report: Value = serde_json::from_str(stdout)
        .unwrap_or_else(|e| panic!("a verify-pack report on stdout ({e}): {stdout:?}"));
    assert!(report.get("role_rebindings").is_some(), "{stdout}");
    report["verdict"].clone()
}

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

/// Absolute path to the double-entry-ledger example, resolved from the
/// crate manifest dir so the working directory does not matter.
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

/// Post a balanced journal entry with `propose` against the ledger example.
/// Returns the receipt's `transition_id`, for use as an as-of coordinate.
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
// `propose` against the ledger example: commit, reject, malformed args.
// The `propose` section further down covers parse failures, unknown
// transformations and invariant rejections with a temp `.morph`.
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

    // Naming the same predicate twice does not duplicate rows.
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

    // Without a programme, the claims table is the authority: a predicate
    // with no claims is an empty result, not an error, even if misspelt.
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

/// `explain` without `--json` prints prose. Pins both verdict headers; the
/// `--json` test pins the content.
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

    let (status, stdout, stderr) = run_cli(&["audit", "verify"]);
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
    let (status, stdout, _stderr) = run_cli(&["audit", "verify"]);
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

    // A writer commits while verify runs repeatedly. Verify reads from one
    // snapshot, so a commit between its reads must not look like a
    // divergence.
    let writer = std::thread::spawn(|| {
        for i in 0..12 {
            post_balanced_entry(&format!("entry_w{i:03}"), 100 + i);
        }
    });
    for _ in 0..6 {
        let (status, stdout, stderr) = run_cli(&["audit", "verify"]);
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

    let (status, stdout, _stderr) = run_cli(&["audit", "verify"]);
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

    // `checkpoint` prints the checkpoint as JSON: the external anchor.
    let (status, cp_stdout, stderr) = run_cli(&["audit", "checkpoint"]);
    assert!(status.success(), "checkpoint should succeed; {stderr}");
    let cp: Value = serde_json::from_str(&cp_stdout).expect("checkpoint output is JSON");
    assert_eq!(cp["status"], "created");
    assert_eq!(cp["tree_size"], 2, "two committed rows: {cp_stdout}");
    assert!(
        cp["root_hash"].as_str().unwrap().starts_with("sha256:"),
        "root is a self-describing hash: {cp_stdout}"
    );

    // Save it to a unique temp file and verify the tree against it.
    let mut anchor = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut anchor, cp_stdout.as_bytes()).unwrap();
    let (status, stdout, stderr) = run_cli(&[
        "audit",
        "verify",
        "--anchor-file",
        anchor.path().to_str().unwrap(),
    ]);
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
    let (status, _stdout, stderr) = run_cli(&["audit", "checkpoint"]);
    assert!(status.success(), "checkpoint should succeed; {stderr}");

    // Default verify: an unsigned checkpoint is intact (signing is opt-in).
    let (status, stdout, _stderr) = run_cli(&["audit", "verify"]);
    assert!(status.success(), "unsigned verify is intact: {stdout}");
    assert_eq!(
        serde_json::from_str::<Value>(&stdout).unwrap()["tree"]["status"],
        "intact"
    );

    // --require-signatures: the unsigned checkpoint now fails, exit non-zero.
    let (status, stdout, _stderr) = run_cli(&["audit", "verify", "--require-signatures"]);
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
async fn signature_policy_flags_compose_and_the_pin_is_refused_on_a_sparse_pack() {
    reset_db().await;
    post_balanced_entry("sp1", 100);
    let (status, _, stderr) = run_cli(&["audit", "checkpoint"]);
    assert!(status.success(), "{stderr}");
    post_balanced_entry("sp2", 100);
    let (status, _, stderr) = run_cli(&["audit", "checkpoint"]);
    assert!(status.success(), "{stderr}");

    // The two spellings of "require" do not stack.
    let (status, _, stderr) = run_cli(&[
        "audit",
        "verify",
        "--require-signatures",
        "--require-signatures-from",
        "2",
    ]);
    assert!(
        !status.success() && stderr.contains("cannot be used with"),
        "{stderr}"
    );

    // Honest unsigned history before the threshold passes; at it, fails.
    let (status, stdout, _) = run_cli(&["audit", "verify", "--require-signatures-from", "3"]);
    assert!(status.success(), "{stdout}");
    let (status, stdout, _) = run_cli(&["audit", "verify", "--require-signatures-from", "2"]);
    assert!(!status.success());
    let tree = &serde_json::from_str::<Value>(&stdout).unwrap()["tree"];
    assert_eq!(tree["status"], "signature_required", "{stdout}");
    assert_eq!(tree["tree_size"], 2, "{stdout}");

    // A pin implies requiring signatures: on an unsigned chain it reports
    // the missing signature before any key question.
    let mut keyfile = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(
        &mut keyfile,
        b"ed25519-pub:0000000000000000000000000000000000000000000000000000000000000000\n",
    )
    .unwrap();
    let key_path = keyfile.path().to_str().unwrap();
    let (status, stdout, _) = run_cli(&["audit", "verify", "--require-signing-key", key_path]);
    assert!(!status.success());
    assert_eq!(
        serde_json::from_str::<Value>(&stdout).unwrap()["tree"]["status"],
        "signature_required",
        "{stdout}"
    );

    // A window pack cannot establish key authority, so the pin is refused
    // rather than weakened to a plain signature check.
    let (status, pack_stdout, stderr) = run_cli(&["audit", "export", "--from-tree-size", "1"]);
    assert!(status.success(), "{stderr}");
    let mut packfile = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut packfile, pack_stdout.as_bytes()).unwrap();
    let (status, stdout, stderr) = run_cli_no_db(&[
        "audit",
        "verify-pack",
        packfile.path().to_str().unwrap(),
        "--require-signing-key",
        key_path,
    ]);
    assert!(!status.success(), "{stdout}");
    assert!(
        stdout.trim().is_empty(),
        "a refusal is operational, nothing on stdout: {stdout}"
    );
    assert!(
        stderr.contains("complete-prefix pack"),
        "the refusal names the remedy: {stderr}"
    );
    // The threshold alone still applies to the window's end.
    let (status, stdout, _) = run_cli_no_db(&[
        "audit",
        "verify-pack",
        packfile.path().to_str().unwrap(),
        "--require-signatures-from",
        "3",
    ]);
    assert!(status.success(), "{stdout}");
}

const KEY_GOVERNANCE_MORPH: &str = "
program key_governance

predicate AuditSigningKey(key_id: Subject, purpose: Subject, public_key: Subject)

transformation authorize(key_id, purpose, public_key):
    admit AuditSigningKey(key_id, purpose, public_key)
";

/// A fresh keypair on disk, authorised in the log under `key_id` for the
/// checkpoint purpose. Returns the private PEM path and the public key
/// file path, as `audit keygen` wrote them.
fn authorised_key(dir: &std::path::Path, key_id: &str) -> (String, String) {
    let pem = dir.join(format!("{key_id}.pem"));
    let public = dir.join(format!("{key_id}.pub"));
    let (status, public_key, stderr) = run_cli_no_db(&[
        "audit",
        "keygen",
        "--private-out",
        pem.to_str().unwrap(),
        "--public-out",
        public.to_str().unwrap(),
    ]);
    assert!(status.success(), "{stderr}");
    let fixture = common::write_fixture("key_governance", KEY_GOVERNANCE_MORPH);
    let args = serde_json::json!([
        {"type": "subject", "value": key_id},
        {"type": "subject", "value": "audit_checkpoint_v1"},
        {"type": "subject", "value": public_key.trim()},
    ]);
    let (status, stdout, stderr) = run_cli(&[
        "propose",
        fixture.path.to_str().unwrap(),
        "authorize",
        "--actor",
        "operator",
        "--args",
        &args.to_string(),
    ]);
    assert!(status.success(), "authorising {key_id}: {stdout}\n{stderr}");
    (
        pem.to_str().unwrap().to_string(),
        public.to_str().unwrap().to_string(),
    )
}

/// Checkpoint the current head signed by `key_id` and return its tree
/// size, read back since key authorisations add audit rows too.
fn checkpoint_signed_by(pem: &str, key_id: &str) -> i64 {
    let (status, stdout, stderr) = run_cli(&[
        "audit",
        "checkpoint",
        "--signing-key",
        pem,
        "--key-id",
        key_id,
    ]);
    assert!(status.success(), "checkpoint signed by {key_id}: {stderr}");
    serde_json::from_str::<Value>(&stdout).unwrap()["tree_size"]
        .as_i64()
        .expect("a checkpoint reports its tree size")
}

fn export_to_file(extra: &[&str]) -> tempfile::NamedTempFile {
    let mut args = vec!["audit", "export"];
    args.extend_from_slice(extra);
    let (status, pack, stderr) = run_cli(&args);
    assert!(status.success(), "{stderr}");
    let mut file = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut file, pack.as_bytes()).unwrap();
    file
}

/// Attacker: a signer whose own key is genuinely authorised in the log.
/// The log is intact and validly signed; only the verifier's key pin on a
/// complete-prefix pack says "not by the key I trust", until the honest key
/// co-signs the same head.
#[tokio::test(flavor = "current_thread")]
async fn a_full_prefix_pack_is_pinned_offline_against_a_rogue_authorised_signer() {
    reset_db().await;
    let dir = tempfile::tempdir().unwrap();
    let (honest_pem, honest_pub) = authorised_key(dir.path(), "honest");
    let (rogue_pem, _) = authorised_key(dir.path(), "rogue");
    post_balanced_entry("pin1", 100);
    checkpoint_signed_by(&rogue_pem, "rogue");

    let pack = export_to_file(&[]);
    let pack_path = pack.path().to_str().unwrap();
    let (status, stdout, _) = run_cli_no_db(&["audit", "verify-pack", pack_path]);
    assert!(status.success(), "intact intrinsically: {stdout}");
    let (status, stdout, _) = run_cli_no_db(&[
        "audit",
        "verify-pack",
        pack_path,
        "--require-signing-key",
        &honest_pub,
    ]);
    assert!(!status.success());
    let verdict = pack_verdict(&stdout);
    assert_eq!(verdict["status"], "signing_key_required", "{stdout}");
    assert_eq!(
        verdict["public_key"],
        std::fs::read_to_string(&honest_pub).unwrap().trim(),
        "{stdout}"
    );

    // The honest key co-signs the same head; the rogue signature is not
    // held against it.
    checkpoint_signed_by(&honest_pem, "honest");
    let pack = export_to_file(&[]);
    let (status, stdout, _) = run_cli_no_db(&[
        "audit",
        "verify-pack",
        pack.path().to_str().unwrap(),
        "--require-signing-key",
        &honest_pub,
        "--require-signatures-from",
        "1",
    ]);
    assert!(status.success(), "{stdout}");
    assert_eq!(pack_verdict(&stdout)["status"], "intact");
}

/// A broken sparse pack reports as broken whatever the policy: the pin's
/// refusal never hides the pack's own verdict.
#[tokio::test(flavor = "current_thread")]
async fn the_pin_never_masks_a_sparse_packs_intrinsic_verdict() {
    reset_db().await;
    let dir = tempfile::tempdir().unwrap();
    let (honest_pem, honest_pub) = authorised_key(dir.path(), "honest");
    post_balanced_entry("mask1", 100);
    let from = checkpoint_signed_by(&honest_pem, "honest").to_string();
    post_balanced_entry("mask2", 100);
    checkpoint_signed_by(&honest_pem, "honest");

    // A window whose end signature has been corrupted.
    let (status, pack, stderr) = run_cli(&["audit", "export", "--from-tree-size", &from]);
    assert!(status.success(), "{stderr}");
    let mut pack: Value = serde_json::from_str(&pack).unwrap();
    pack["to_checkpoint"]["signatures"][0]["signature"] =
        Value::String(format!("ed25519-sig:{}", "0".repeat(128)));
    let mut broken = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut broken, pack.to_string().as_bytes()).unwrap();
    let broken_path = broken.path().to_str().unwrap();
    let mut malformed = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(
        &mut malformed,
        br#"{"manifest": {"pack_format_version": 2}}"#,
    )
    .unwrap();
    let malformed_path = malformed.path().to_str().unwrap();

    for (path, expected) in [
        (broken_path, "signature_invalid"),
        (malformed_path, "malformed"),
    ] {
        let (_, without, _) = run_cli_no_db(&["audit", "verify-pack", path]);
        let (status, with, stderr) = run_cli_no_db(&[
            "audit",
            "verify-pack",
            path,
            "--require-signing-key",
            &honest_pub,
        ]);
        assert!(!status.success());
        assert_eq!(pack_verdict(&without)["status"], expected, "{without}");
        assert_eq!(
            with, without,
            "the verdict must be byte-identical with the pin: {with}\n{stderr}"
        );
    }

    // Only an intact sparse pack reaches the refusal.
    let intact = export_to_file(&["--from-tree-size", &from]);
    let (status, stdout, stderr) = run_cli_no_db(&[
        "audit",
        "verify-pack",
        intact.path().to_str().unwrap(),
        "--require-signing-key",
        &honest_pub,
    ]);
    assert!(!status.success() && stdout.trim().is_empty(), "{stdout}");
    assert!(stderr.contains("complete-prefix pack"), "{stderr}");

    // A negative threshold has no meaning and is refused at the boundary.
    let (status, _, stderr) = run_cli(&["audit", "verify", "--require-signatures-from", "-1"]);
    assert!(!status.success() && stderr.contains("-1"), "{stderr}");
}

#[tokio::test(flavor = "current_thread")]
async fn evidence_export_then_verify_offline() {
    reset_db().await;
    post_balanced_entry("ev1", 100);
    post_balanced_entry("ev2", 200);

    // Anchor (saved outside the database), then export the pack.
    let (status, cp_stdout, stderr) = run_cli(&["audit", "checkpoint"]);
    assert!(status.success(), "checkpoint should succeed; {stderr}");
    let mut anchor = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut anchor, cp_stdout.as_bytes()).unwrap();

    let (status, pack_stdout, stderr) = run_cli(&["audit", "export"]);
    assert!(status.success(), "evidence export should succeed; {stderr}");
    let pack: Value = serde_json::from_str(&pack_stdout).expect("pack is JSON");
    assert_eq!(pack["manifest"]["tree_size"], 2, "{pack_stdout}");
    assert_eq!(pack["rows"].as_array().unwrap().len(), 2);
    let mut packfile = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut packfile, pack_stdout.as_bytes()).unwrap();
    let pack_path = packfile.path().to_str().unwrap();

    // Offline verify (NO --database-url) against the anchor: intact, exit 0.
    let (status, stdout, stderr) = run_cli_no_db(&[
        "audit",
        "verify-pack",
        pack_path,
        "--anchor-file",
        anchor.path().to_str().unwrap(),
    ]);
    assert!(
        status.success(),
        "offline verify should pass; {stderr}\n{stdout}"
    );
    assert_eq!(pack_verdict(&stdout)["status"], "intact", "got: {stdout}");

    // Edit a row in the pack file: verify must catch it and exit non-zero.
    let mut tampered_json: Value = serde_json::from_str(&pack_stdout).unwrap();
    tampered_json["rows"][0]["transformation_name"] = serde_json::json!("tampered");
    let mut tamperedfile = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut tamperedfile, tampered_json.to_string().as_bytes()).unwrap();
    let (status, stdout, _stderr) = run_cli_no_db(&[
        "audit",
        "verify-pack",
        tamperedfile.path().to_str().unwrap(),
    ]);
    assert!(
        !status.success(),
        "a tampered pack must exit non-zero: {stdout}"
    );
    assert_eq!(pack_verdict(&stdout)["status"], "tampered", "got: {stdout}");
}

#[tokio::test(flavor = "current_thread")]
async fn evidence_selective_export_then_verify_offline() {
    reset_db().await;
    let shown_a = post_balanced_entry("sd1", 100);
    post_balanced_entry("sd_hidden", 200);
    let shown_b = post_balanced_entry("sd3", 300);

    let (status, cp_stdout, stderr) = run_cli(&["audit", "checkpoint"]);
    assert!(status.success(), "checkpoint should succeed; {stderr}");
    let mut anchor = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut anchor, cp_stdout.as_bytes()).unwrap();

    // Disclose two of the three committed transitions.
    let (status, pack_stdout, stderr) = run_cli(&[
        "audit",
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

    // Nothing of the undisclosed transition appears in the bytes: not its
    // entry subject, accounts, claims or intent payloads.
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
        "audit",
        "verify-pack",
        pack_path,
        "--anchor-file",
        anchor.path().to_str().unwrap(),
    ]);
    assert!(
        status.success(),
        "offline verify should pass; {stderr}\n{stdout}"
    );
    let verdict = pack_verdict(&stdout);
    assert_eq!(verdict["status"], "intact", "got: {stdout}");
    assert_eq!(verdict["rows_disclosed"], 2, "got: {stdout}");

    // Edit a disclosed row: verify names its position and exits non-zero.
    let mut tampered_json: Value = serde_json::from_str(&pack_stdout).unwrap();
    tampered_json["rows"][0]["transformation_name"] = serde_json::json!("tampered");
    let mut tamperedfile = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut tamperedfile, tampered_json.to_string().as_bytes()).unwrap();
    let (status, stdout, _stderr) = run_cli_no_db(&[
        "audit",
        "verify-pack",
        tamperedfile.path().to_str().unwrap(),
    ]);
    assert!(
        !status.success(),
        "a tampered selective pack must exit non-zero: {stdout}"
    );
    assert_eq!(
        pack_verdict(&stdout)["status"],
        "row_not_included",
        "got: {stdout}"
    );

    // Compliance mode: the covering checkpoint is unsigned, so
    // --require-signatures fails the otherwise-intact pack.
    let (status, stdout, _stderr) =
        run_cli_no_db(&["audit", "verify-pack", pack_path, "--require-signatures"]);
    assert!(!status.success(), "unsigned must fail the policy: {stdout}");
    assert_eq!(
        pack_verdict(&stdout)["status"],
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
        run_cli_no_db(&["audit", "verify-pack", packfile.path().to_str().unwrap()]);
    assert!(!status.success());
    let verdict = pack_verdict(&stdout);
    assert_eq!(verdict["status"], "malformed_pack", "got: {stdout}");
    assert!(
        verdict["detail"].as_str().unwrap().contains("newer"),
        "got: {stdout}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn evidence_verify_on_a_readable_but_invalid_pack_is_a_malformed_verdict() {
    // A file that is not a valid pack gets a verdict on stdout
    // (`malformed_pack`, exit 1), not an operational error on stderr.
    let mut packfile = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut packfile, br#"{"not": "a pack"}"#).unwrap();
    let (status, stdout, stderr) =
        run_cli_no_db(&["audit", "verify-pack", packfile.path().to_str().unwrap()]);
    assert!(
        !status.success(),
        "an invalid pack must exit non-zero: {stdout}"
    );
    assert_eq!(
        pack_verdict(&stdout)["status"],
        "malformed_pack",
        "got stdout={stdout:?} stderr={stderr:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn evidence_verify_refuses_a_pack_whose_hash_is_not_a_digest() {
    // A hash that is not `sha256:<64 lowercase hex>` is refused on read as
    // `malformed_pack`, naming the value. Uppercase hex decodes to the
    // same bytes but is not the record's spelling.
    let mut packfile = tempfile::NamedTempFile::new().unwrap();
    let uppercase = format!("sha256:{}", "AB".repeat(32));
    std::io::Write::write_all(
        &mut packfile,
        format!(r#"{{"manifest": {{"pack_format_version": 1, "root_hash": "{uppercase}"}}}}"#)
            .as_bytes(),
    )
    .unwrap();
    let (status, stdout, _stderr) =
        run_cli_no_db(&["audit", "verify-pack", packfile.path().to_str().unwrap()]);
    assert!(!status.success(), "got: {stdout}");
    let verdict = pack_verdict(&stdout);
    assert_eq!(verdict["status"], "malformed_pack", "got: {stdout}");
    let detail = verdict["detail"].as_str().unwrap();
    assert!(
        detail.contains("not a sha256:<hex> digest") && detail.contains(&uppercase),
        "the verdict names the value it refused: {detail}"
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
    assert_eq!(report["score_format_version"], 2);
    assert_eq!(report["semantics"], "case_bound_admission_v2");
    assert!(
        report["program_hash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"),
        "{stdout}"
    );
    let inv = &report["invariants"][0];
    assert_eq!(inv["invariant"], "no_entries");
    // Each posting adds its own forbidden entry, so both would have been
    // refused; an older violation does not hide a new one.
    assert_eq!(inv["would_refuse"], 2, "got: {stdout}");
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
    // Each slice's posting introduces its own forbidden entry, so each
    // slice charges its own.
    assert_eq!(split["train"]["transitions_replayed"], 1);
    assert_eq!(split["test"]["transitions_replayed"], 1);
    assert_eq!(split["train"]["invariants"][0]["would_refuse"], 1);
    assert_eq!(split["test"]["invariants"][0]["would_refuse"], 1);
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
    // A candidate using pre(...) cannot be scored.
    let candidate = "program candidate\n\n\
         predicate Flag(x: Subject)\n\n\
         invariant uses_pre:\n    Flag(a) implies pre(Flag(a))\n";
    let mut f = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut f, candidate.as_bytes()).unwrap();

    // An unreachable database: the pre(...) refusal must come before any
    // connection attempt.
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
    let (s, _, e) = run_cli(&["audit", "checkpoint"]);
    assert!(s.success(), "checkpoint: {e}");
    let (s, pack_stdout, e) = run_cli(&["audit", "export"]);
    assert!(s.success(), "export: {e}");
    let mut packfile = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut packfile, pack_stdout.as_bytes()).unwrap();

    let candidate = "program candidate\n\n\
         predicate JournalEntry(entry_id: Subject, posting_date: Subject, period: Subject)\n\n\
         invariant no_entries:\n    not (exists e: JournalEntry(e, _, _))\n";
    let mut candfile = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut candfile, candidate.as_bytes()).unwrap();

    // Score against the pack with no --database-url.
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
    assert_eq!(report["semantics"], "case_bound_admission_v2");
    assert_eq!(report["invariants"][0]["would_refuse"], 2, "got: {stdout}");

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
    let (s, _, e) = run_cli(&["audit", "checkpoint"]);
    assert!(s.success(), "checkpoint {name}: {e}");
    let (s, pack, e) = run_cli(&["audit", "export"]);
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
    assert_eq!(report["semantics"], "case_bound_admission_v2");
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
    // --anchor-file needs --pack, so with --packs it is a clap error.
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

    // The first commit's exact timestamp. The boundary is inclusive: at
    // that instant, the commit's state is what you get.
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
    // Every line is a full audit row, naming its own parameters, one per
    // argument, as the programme declares them.
    for row in &rows {
        assert!(row["transition_id"].is_string());
        assert!(row["asserted_claims"].is_array());
        assert!(row["committed_at"].is_string());
        assert_eq!(row["actor"]["type"], "subject");
        assert_eq!(
            row["parameters"],
            serde_json::json!([
                "entry_id",
                "posting_date",
                "period",
                "debit_account",
                "credit_account",
                "amount"
            ]),
            "{row}"
        );
        assert_eq!(
            row["parameters"].as_array().unwrap().len(),
            row["arguments"].as_array().unwrap().len()
        );
    }
}

/// The act that wrote a row is retired; the row still names its
/// parameters, read bare with no programme at all.
#[tokio::test(flavor = "current_thread")]
async fn a_retired_acts_rows_still_name_their_parameters() {
    reset_db().await;
    let before = common::write_fixture(
        "cases_v1",
        "program cases\n\npredicate Case(case_id: Subject, region: Subject, severity: Decimal)\n\ntransformation open_case(case_id, region, severity):\n    admit Case(case_id, region, severity)\n",
    );
    let after = common::write_fixture(
        "cases_v2",
        "program cases\n\npredicate Case(case_id: Subject, region: Subject, severity: Decimal)\n\ntransformation open(case_id, region, severity, channel):\n    admit Case(case_id, region, severity)\n",
    );
    let (status, _, stderr) = run_cli(&[
        "propose",
        before.path.to_str().unwrap(),
        "open_case",
        "--actor",
        "alex",
        "--args-named",
        r#"{"case_id":"C-17","region":"north","severity":"3"}"#,
    ]);
    assert!(status.success(), "{stderr}");

    // Bare: the names come from the row, not the programme.
    let (status, stdout, _) = run_cli(&["inspect", "audit"]);
    assert!(status.success());
    let rows = ndjson(&stdout);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["transformation_name"], "open_case");
    assert_eq!(
        rows[0]["parameters"],
        serde_json::json!(["case_id", "region", "severity"]),
        "{}",
        rows[0]
    );
    assert_eq!(rows[0]["arguments"][0]["value"], "C-17");

    // Under the later programme, which dropped the act but still declares
    // the claim: the claims decode and the names stay.
    let (status, stdout, stderr) =
        run_cli(&["inspect", "audit", "--named", after.path.to_str().unwrap()]);
    assert!(status.success(), "{stderr}");
    let rows = ndjson(&stdout);
    assert_eq!(
        rows[0]["asserted_claims"][0]["args"]["case_id"], "C-17",
        "{}",
        rows[0]
    );
    assert_eq!(
        rows[0]["parameters"],
        serde_json::json!(["case_id", "region", "severity"])
    );
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_audit_after_resumes_strictly_after_the_cursor() {
    reset_db().await;
    let t1 = post_balanced_entry("entry_001", 100);
    post_balanced_entry("entry_002", 200);
    post_balanced_entry("entry_003", 300);

    // Resuming from the first transition yields the later two, in order,
    // without the cursor row.
    let (status, stdout, _stderr) = run_cli(&["inspect", "audit", "--after", &t1.to_string()]);
    assert!(status.success());
    let rows = ndjson(&stdout);
    assert_eq!(rows.len(), 2, "strictly after the cursor: {stdout}");

    // Resuming from the last id is an empty tail, exit 0: a poll loop's
    // steady state.
    let last_id = rows[1]["transition_id"].as_str().unwrap().to_string();
    let (status, stdout, _stderr) = run_cli(&["inspect", "audit", "--after", &last_id]);
    assert!(status.success(), "an empty tail is not an error");
    assert!(stdout.trim().is_empty(), "empty tail, empty stdout");

    // An unknown cursor is an error naming the id, never a restart from
    // zero.
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
    // ...while transformation arguments stay tagged.
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

    // A programme without the ledger's predicates: the named decode must
    // refuse, not skip rows.
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

// `--writer-role` reaches both commands that use the watermark. The test
// role is a superuser, so the role check passes trivially here; it is
// tested properly in morpholog-postgres.
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

    let (status, stdout, stderr) = run_cli(&["audit", "checkpoint", "--writer-role", &me]);
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

    // An empty rejection log lists empty and exits zero.
    let (status, stdout, _stderr) = run_cli(&["inspect", "rejections"]);
    assert!(status.success());
    let rows: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(rows.as_array().unwrap().len(), 0, "empty log: {stdout}");

    // Close the period, then post into it: the gate refuses and the
    // refusal is logged.
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

    // --named must not change the default, the tagged array
    // `inspect claims` also prints.
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
    // The ledger declares exactly one derived predicate, so the report's
    // total can be compared with one `inspect derived` read.
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
// `morpholog propose` with a temp `.morph` file
// ============================================================

/// Write a minimal balanced-ledger programme to a temp .morph file and
/// return the path. One transformation, one invariant: independent of the
/// full ledger example.
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

/// `propose --args-named` happy path against the temp ledger: the same
/// transformation as the tagged test above, with bare values keyed by
/// parameter name and decoded by declared kind.
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

/// Propose `post_simple_entry` with the named codec and expect an error
/// whose stderr contains every needle. Takes the path so a multi-case test
/// resets and writes once.
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

/// A missing parameter in `--args-named` is an error before any database
/// work, naming it and pointing at `morpholog schema`.
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

/// An unknown key in `--args-named` is an error listing the accepted
/// parameters, so a typo is not reported as "missing required".
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

/// `explain --args-named --json` decodes as `propose` does and prints an
/// `Explanation`, checking explain's wiring under the named codec.
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
    // Explain exits zero for a valid programme; the verdict is in the JSON.
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

/// `--args-named` accepts any string as a Subject: minted ids and domain
/// symbols (period names, account codes) alike, as the embedder docs'
/// `commodity:"oil"` examples rely on.
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

/// Decimal strings that fail the schema's `^-?(0|[1-9]\d*)(\.\d+)?$` must
/// also fail `--args-named`, so the CLI enforces what the schema says.
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

/// A wrong JSON type names the parameter and the kind it should be.
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

/// Write a temp .morph with a `post_unbalanced_entry` transformation that
/// can break an invariant, to test rejection by an invariant.
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
    // A rejection exits 1 with the receipt on stdout, not stderr.
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
    // Stderr points at the violated invariant's declaration (9:1 in the
    // temp programme). Stdout is unchanged.
    assert!(
        stderr.contains("rule at") && stderr.contains(":9:1 (balanced_posted_entry)"),
        "stderr should locate the violated rule; got: {stderr}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn run_rejects_parse_failure_in_user_morph() {
    reset_db().await;
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
// The lease protocol end to end. Each test admits one journal entry, so a
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
// `--status pending` is the default: "what is waiting?".
// ============================================================

/// Seed the outbox with two pending rows.
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

/// No `--status` means pending; `all` and `delivered` show the rest. Each
/// filter's exact (sorted) status list is pinned.
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
// A non-Rust consumer can drive the whole propose, commit and outbox loop
// with only the `morpholog` binary.
// ============================================================

#[tokio::test(flavor = "current_thread")]
async fn compute_loop_end_to_end_via_cli_binary_only() {
    reset_db().await;

    // 1. The consumer writes its own `.morph` and runs `morpholog propose`.
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

    // 2. It claims the resulting outbox row.
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

    // 3. It does the external work (a no-op here).

    // 4. It marks the row delivered.
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

    // 5. The row shows as delivered.
    let (status, stdout, _stderr) = run_cli(&["inspect", "outbox", "--status", "delivered"]);
    assert!(status.success());
    let delivered: Value = serde_json::from_str(&stdout).unwrap();
    let arr = delivered.as_array().unwrap();
    assert_eq!(arr.len(), 1, "exactly one delivered row");
    assert_eq!(arr[0]["intent_id"], intent_id);

    // 6. And no longer as pending.
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

/// Write a temp `.morph` that parses but fails `Program::validate()`: the
/// invariant uses an undeclared predicate `Bar`.
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
    // An invalid programme is refused with diagnostics before the derived
    // lookup or any database connection, as `propose` does.
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
    // Drop the schema, provision it through the binary, prove it works,
    // then pin both already-initialised behaviours. Safe here because the
    // suite is serial and other tests only truncate.
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

    // --reset needs the acknowledgement, and the acknowledgement needs
    // --reset, so a stray one in a script cannot wait for a later reset.
    let (status, _stdout, stderr) = run_cli(&["init", "--reset"]);
    assert!(
        !status.success(),
        "--reset without the acknowledgement must refuse"
    );
    assert!(
        stderr.contains("--i-know-this-deletes-data")
            && stderr.contains(&morpholog_postgres::redact_database_url(&database_url())),
        "the refusal names the flag AND the target it would have destroyed: {stderr}"
    );
    let (status, _stdout, stderr) = run_cli(&["init", "--i-know-this-deletes-data"]);
    assert!(!status.success(), "the acknowledgement alone must refuse");
    assert!(stderr.contains("only meaningful with --reset"), "{stderr}");

    // Add data first, so the reset visibly drops something.
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

    // With no schema, it says there was nothing to drop.
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
// `propose --explain-on-reject` - same-snapshot diagnosis
// ============================================================

#[tokio::test(flavor = "current_thread")]
async fn run_explain_on_reject_attaches_the_same_snapshot_explanation() {
    reset_db().await;
    // Close the period, then propose into it: the rejection carries an
    // explanation computed against the exact state the gate saw.
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

/// A `--where` clause is refused before any database work when there is
/// nothing to resolve its fields against: no named read, or more than one
/// predicate. The session test mirrors this one.
#[tokio::test(flavor = "current_thread")]
async fn inspect_claims_where_is_refused_without_a_declaration_to_read_it_against() {
    reset_db().await;
    let ledger = ledger_morph();
    let (status, _stdout, stderr) = run_cli(&[
        "inspect",
        "claims",
        "--predicate",
        "JournalLine",
        "--where",
        "entry_id=entry_001",
    ]);
    assert!(!status.success(), "no named read: {stderr}");
    assert!(stderr.contains("needs the named read"), "got: {stderr}");

    let (status, _stdout, stderr) = run_cli(&[
        "inspect",
        "claims",
        "--predicate",
        "JournalLine",
        "--predicate",
        "JournalEntry",
        "--where",
        "entry_id=entry_001",
        "--named",
        &ledger,
    ]);
    assert!(!status.success(), "two predicates: {stderr}");
    assert!(
        stderr.contains("needs exactly one predicate"),
        "got: {stderr}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_claims_named_hard_errors_on_programme_database_skew() {
    reset_db().await;
    post_balanced_entry("entry_001", 100);

    // A programme that does not declare the stored claims: the named read
    // refuses by name, never skips.
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

    // The schema carries the unit in an extension and in the description
    // (form generators ignore extensions); the wire value stays a bare
    // decimal. `schema` takes no database flag, so skip run_cli.
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

    // Tagged codec in: the unit travels with the value.
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

    // Without --named, an unknown predicate just matches nothing.
    let (status, stdout, stderr) = run_cli(&["inspect", "claims", "--predicate", "JornalLine"]);
    assert!(status.success(), "bare read tolerates the typo; {stderr}");
    let rows: Value = serde_json::from_str(&stdout).expect("stdout is JSON");
    assert_eq!(rows.as_array().unwrap().len(), 0, "typo matches nothing");

    // With `--named`, the same typo is an error naming the declared
    // predicates, before any database read.
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
// propose --batch: rows in, a receipt per row, each its own commit.
// ============================================================

/// `propose --batch` with NDJSON rows in a temp file. Returns
/// (status, parsed receipts, stderr).
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

// The batch contract in one run: commits, a skipped blank line, an error
// receipt for a malformed row, a rejection, and a later row still
// committing. Exit 0, because every row got a receipt.
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
        // Posting into the closed period: a rejection.
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
    assert_eq!(
        receipts[2]["code"], "invalid_request",
        "a malformed row's receipt carries the stable code: {}",
        receipts[2]
    );
    // `row` is the 1-based input line number, so receipts map back to
    // the file even with blank lines skipped.
    let rows_field: Vec<u64> = receipts
        .iter()
        .map(|r| r["row"].as_u64().expect("row"))
        .collect();
    assert_eq!(rows_field, vec![1, 3, 4, 5, 6]);
    // Per-row actors land in the receipts and the audit rows.
    assert_eq!(receipts[1]["actor"]["value"], "maria");
    assert_eq!(receipts[4]["actor"]["value"], "nina");
    assert!(
        stderr.contains("5 rows - 3 committed, 1 rejected, 1 errors"),
        "summary on stderr: {stderr}"
    );
    // No rule-location line in batch mode: stderr carries only the
    // summary.
    assert!(
        !stderr.contains("rule at"),
        "no rule-location lines in batch mode; got: {stderr}"
    );

    // The batch's rejection is in the rejection log, as for single runs.
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

// An operational failure, such as an unreadable batch file, exits
// non-zero.
#[tokio::test]
async fn batch_with_unreadable_input_exits_nonzero() {
    let (status, stdout, stderr) = run_cli(&[
        "propose",
        &ledger_morph(),
        "--batch",
        "/nonexistent/rows.ndjson",
    ]);
    assert!(!status.success());
    assert!(stderr.contains("failed to read batch rows"), "{stderr}");
    // One error object for the whole batch, with no `row`: no row was
    // attempted, and the binary says so. One line, framed like a receipt,
    // because a batch is read line by line.
    assert_eq!(stdout.lines().count(), 1, "one NDJSON line: {stdout:?}");
    let error: Value = serde_json::from_str(&stdout).expect("one error object on stdout");
    assert_eq!(error["code"], "not_committed", "{stdout}");
    assert!(error.get("row").is_none(), "{stdout}");
}

/// Every failure before the adapter call prints a coded error object, so a
/// caller never has to infer "nothing committed" from silence.
#[tokio::test]
async fn a_one_shot_failure_before_the_proposal_is_a_coded_error_object() {
    let dir = tempfile::tempdir().unwrap();
    let broken = dir.path().join("broken.morph");
    std::fs::write(&broken, "program broken\ninvariant x:\n").unwrap();
    let cases: Vec<(&str, Vec<String>, &str)> = vec![
        (
            "unknown transformation",
            vec![
                "propose".into(),
                ledger_morph(),
                "no_such_act".into(),
                "--actor".into(),
                "alex".into(),
                "--args".into(),
                "[]".into(),
            ],
            "unknown_transformation",
        ),
        (
            "malformed arguments",
            vec![
                "propose".into(),
                ledger_morph(),
                "post_simple_entry".into(),
                "--actor".into(),
                "alex".into(),
                "--args".into(),
                "not json".into(),
            ],
            "invalid_arguments",
        ),
        (
            "a programme that does not parse",
            vec![
                "propose".into(),
                broken.display().to_string(),
                "x".into(),
                "--actor".into(),
                "alex".into(),
                "--args".into(),
                "[]".into(),
            ],
            "not_committed",
        ),
        (
            "transact, a programme that does not parse",
            vec![
                "transact".into(),
                broken.display().to_string(),
                "--acts".into(),
                "/nonexistent".into(),
            ],
            "not_committed",
        ),
    ];
    for (what, args, code) in cases {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let (status, stdout, stderr) = run_cli(&args);
        assert_eq!(status.code(), Some(1), "{what}: {stderr}");
        let error: Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("{what}: no error object on stdout ({e}): {stdout:?}"));
        assert_eq!(error["status"], "error", "{what}: {stdout}");
        assert_eq!(error["code"], code, "{what}: {stdout}");
        assert!(
            !stderr.trim().is_empty(),
            "{what}: a person still gets prose"
        );
    }

    // A connection that fails is a known non-commit too.
    let output = Command::new(common::bin())
        .args([
            "propose",
            &ledger_morph(),
            "post_simple_entry",
            "--actor",
            "alex",
            "--args",
            &ledger_args_json("e1", "2026-04-15", "q1_2026", "100"),
            "--database-url",
            "postgres:///morpholog_no_such_database",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let error: Value = serde_json::from_slice(&output.stdout).expect("an error object");
    assert_eq!(error["code"], "not_committed");
}

/// A batch receipt reaches the caller as soon as its row is done, not when
/// the process exits: a caller that kills a long batch still holds every
/// receipt for the rows that finished.
#[tokio::test(flavor = "current_thread")]
async fn batch_receipts_are_visible_before_the_batch_ends() {
    reset_db().await;
    let rows: String = (0..400)
        .map(|i| posting_row(None, &format!("flush_{i}")) + "\n")
        .collect();
    let mut child = Command::new(common::bin())
        .args([
            "propose",
            &ledger_morph(),
            "--batch",
            "-",
            "--database-url",
            &database_url(),
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), rows.as_bytes()).unwrap();
    drop(child.stdin.take());
    let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut first = String::new();
    std::io::BufRead::read_line(&mut stdout, &mut first).unwrap();
    let still_running = child.try_wait().unwrap().is_none();
    child.kill().unwrap();
    child.wait().unwrap();
    let receipt: Value = serde_json::from_str(&first).expect("a whole receipt line");
    assert_eq!(receipt["row"], 1, "{first}");
    assert!(
        still_running,
        "the first receipt should arrive while later rows are still running"
    );
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

// Batch rows carry their own args, so clap refuses top-level args flags
// with --batch rather than ignore them.
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

// Prose names the verdicts, with a legend saying what committed history
// cannot show. Exit is zero whatever it finds: never-fired rules are the
// answer, not a failure.
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

/// `migrate --check` fails a database ahead of this binary, not just one
/// behind it. Nothing is pending for an ahead database, so a check of
/// `pending` alone would pass it. Tested at the CLI, where a deploy gate
/// reads the answer.
#[tokio::test]
async fn migrate_check_fails_a_database_ahead_of_the_binary() {
    let pool = PgPool::connect(&database_url())
        .await
        .expect("connect to test DB");
    let future = morpholog_postgres::head_version() + 1;

    // Current to begin with, or the assertions below prove nothing.
    let (before, _, _) = run_cli(&["migrate", "--check"]);
    assert!(before.success(), "the test database must start current");

    sqlx::query(
        "INSERT INTO morpholog.schema_migrations (version, name)
         VALUES ($1, 'from_a_newer_morpholog') ON CONFLICT DO NOTHING",
    )
    .bind(future)
    .execute(&pool)
    .await
    .expect("record a version from the future");

    let (status, stdout, _) = run_cli(&["migrate", "--check"]);
    let (apply_status, _, _) = run_cli(&["migrate"]);

    // Clean up before asserting, so a failure cannot leave the shared
    // database looking migrated by a newer binary.
    sqlx::query("DELETE FROM morpholog.schema_migrations WHERE version = $1")
        .bind(future)
        .execute(&pool)
        .await
        .expect("remove it again");

    let report: Value = serde_json::from_str(&stdout).expect("the report is still on stdout");
    assert_eq!(
        report["pending"].as_array().map(Vec::len),
        Some(0),
        "nothing is pending - which is why gating on that field green-lit this"
    );
    assert!(
        !status.success(),
        "a database ahead of this binary must fail the readiness check"
    );
    assert!(
        !apply_status.success(),
        "migrating a database ahead of this binary must be refused"
    );
}

/// In a batch, an unauthorised assertion gets a coded receipt and the run
/// carries on. The grant is a claim compared with `session_user` as text,
/// so naming another login makes this connection unauthorised.
const POLICY_BATCH_MORPH: &str = "program policy_batch

predicate ActorAssertionRestricted(actor: Subject)
predicate ActorAssertionAuthority(actor: Subject, login_role: Subject)
predicate Noted(id: Subject)

transformation arm(person, login_role):
    admit ActorAssertionRestricted(person)
    admit ActorAssertionAuthority(person, login_role)

transformation note(id):
    admit Noted(id)
";

#[tokio::test(flavor = "current_thread")]
async fn batch_gives_an_unauthorised_row_a_coded_receipt_and_keeps_going() {
    reset_db().await;
    let fixture = common::write_fixture("policy_batch", POLICY_BATCH_MORPH);
    let rows = format!(
        "{}\n{}\n{}\n",
        serde_json::json!({
            "transformation": "arm", "actor": "bootstrap",
            "args_named": {"person": "restricted_actor", "login_role": "somebody_else"}
        }),
        serde_json::json!({
            "transformation": "note", "actor": "restricted_actor",
            "args_named": {"id": "n1"}
        }),
        serde_json::json!({
            "transformation": "note", "actor": "anyone_else",
            "args_named": {"id": "n2"}
        }),
    );
    let f = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(f.path(), &rows).unwrap();
    let (status, stdout, stderr) = run_cli(&[
        "propose",
        fixture.path.to_str().unwrap(),
        "--batch",
        f.path().to_str().unwrap(),
    ]);
    let receipts: Vec<Value> = stdout
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(receipts.len(), 3, "stdout={stdout}\nstderr={stderr}");
    assert_eq!(receipts[0]["status"], "committed", "{stdout}");
    assert_eq!(receipts[1]["status"], "error", "{stdout}");
    assert_eq!(receipts[1]["row"], 2, "{stdout}");
    // A per-row receipt, not an abort, with the session's code; the prose
    // names both parties.
    assert_eq!(
        receipts[1]["code"], "actor_assertion_unauthorised",
        "{stdout}"
    );
    let reason = receipts[1]["error"].as_str().unwrap_or_default();
    assert!(
        reason.contains("restricted_actor") && reason.contains("not authorised"),
        "the refusal should name the actor it refused: {stdout}"
    );
    assert_eq!(
        receipts[2]["status"], "committed",
        "a refused row must not stop the run: {stdout}"
    );
    assert!(
        status.success(),
        "every row produced a receipt, so exit 0; {stderr}"
    );
}

// ============================================================
// What `main` prints on failure.
// ============================================================

/// An ordinary failure prints as `Result`'s own `Termination` would:
/// `Error: ` then the context chain.
#[tokio::test(flavor = "current_thread")]
async fn an_ordinary_error_keeps_the_termination_rendering() {
    let (status, stdout, stderr) =
        run_cli_no_db(&["check", "/nonexistent/definitely-not-here.morph"]);
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(stdout.is_empty(), "stdout stays script-silent: {stdout}");
    assert!(
        stderr.starts_with("Error: "),
        "the prefix std printed: {stderr}"
    );
    assert!(
        stderr.contains("read source file"),
        "the context the command attached: {stderr}"
    );
    assert!(
        stderr.contains("Caused by:"),
        "and the cause chain beneath it: {stderr}"
    );
}

/// A failure the command already rendered gets no second message from
/// `main`.
#[tokio::test(flavor = "current_thread")]
async fn an_already_reported_failure_gets_no_second_message() {
    let fixture = common::write_fixture(
        "already_reported",
        "program p\npredicate P(x: Subject)\ntransformation t(x):\n    admit Q(x)\n",
    );
    let (status, _stdout, stderr) = run_cli_no_db(&["check", fixture.path.to_str().unwrap()]);
    assert_eq!(status.code(), Some(1), "{stderr}");
    assert!(
        stderr.contains("undeclared predicate"),
        "the command's own diagnostic is what the user reads: {stderr}"
    );
    assert!(
        !stderr.contains("Error: "),
        "main must add nothing on top of a rendered diagnostic: {stderr}"
    );
    assert!(
        !stderr.contains("reported its own diagnostics"),
        "the sentinel's Display is an implementation detail, never output: {stderr}"
    );
}

/// The recorded DigiCert token over the frozen sample head, base64 as a
/// checkpoint stores it. It vouches for that head and no other.
fn recorded_witness_json() -> Value {
    use base64::Engine as _;
    let tsr = std::fs::read(format!(
        "{}/../morpholog-witness/tests/fixtures/rfc3161/genesis_digicert.tsr",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    serde_json::json!({
        "scheme": "rfc3161",
        "proof": base64::engine::general_purpose::STANDARD.encode(tsr),
        "submitted_to": "http://timestamp.digicert.com",
    })
}

#[tokio::test(flavor = "current_thread")]
async fn a_witness_that_does_not_vouch_for_its_checkpoint_fails_the_live_verify() {
    // Attacker capability: attaches a genuine timestamp token, obtained
    // over some other head, to a checkpoint it never covered.
    reset_db().await;
    post_balanced_entry("w1", 100);
    let (status, cp_stdout, stderr) = run_cli(&["audit", "checkpoint"]);
    assert!(status.success(), "{stderr}");
    let cp: Value = serde_json::from_str(&cp_stdout).unwrap();

    // Before any witness: the report has no witness axis at all.
    let (status, stdout, _) = run_cli(&["audit", "verify"]);
    assert!(status.success(), "{stdout}");
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert!(report.get("witnesses").is_none(), "{stdout}");

    let pool = PgPool::connect(&database_url()).await.unwrap();
    let witness: morpholog_postgres::Witness =
        serde_json::from_value(recorded_witness_json()).unwrap();
    morpholog_postgres::attach_witness(
        &pool,
        cp["tree_size"].as_i64().unwrap(),
        &cp["checkpoint_hash"]
            .as_str()
            .unwrap()
            .parse::<morpholog_postgres::Digest>()
            .unwrap(),
        witness,
    )
    .await
    .unwrap();

    let (status, stdout, _) = run_cli(&["audit", "verify"]);
    assert!(!status.success(), "an invalid witness fails: {stdout}");
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        report["tree"]["status"], "intact",
        "the tree itself is fine: {stdout}"
    );
    let verdict = &report["witnesses"]["checkpoints"][0]["witnesses"][0];
    assert_eq!(verdict["status"], "invalid", "{stdout}");
    assert_eq!(verdict["submitted_to"], "http://timestamp.digicert.com");
    assert!(
        verdict.get("attested_at").is_none(),
        "no time from a token that does not apply"
    );
    assert!(report["witnesses"].get("earliest_attested_at").is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn verify_pack_reports_witnesses_only_when_asked() {
    reset_db().await;
    post_balanced_entry("wp1", 100);
    let (status, _, stderr) = run_cli(&["audit", "checkpoint"]);
    assert!(status.success(), "{stderr}");
    let (status, pack_stdout, stderr) = run_cli(&["audit", "export"]);
    assert!(status.success(), "{stderr}");

    // The same pack with the recorded token grafted onto its checkpoint.
    let mut pack: Value = serde_json::from_str(&pack_stdout).unwrap();
    pack["checkpoints"][0]["witnesses"] = Value::Array(vec![recorded_witness_json()]);
    let mut packfile = tempfile::NamedTempFile::new().unwrap();
    std::io::Write::write_all(&mut packfile, pack.to_string().as_bytes()).unwrap();
    let path = packfile.path().to_str().unwrap();

    // Not asked: the report, with no witnesses in it.
    let (status, stdout, _) = run_cli_no_db(&["audit", "verify-pack", path]);
    assert!(status.success(), "{stdout}");
    assert_eq!(pack_verdict(&stdout)["status"], "intact", "{stdout}");
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert!(report.get("witnesses").is_none(), "{stdout}");

    // Asked: the wrapper, and the grafted token is judged invalid.
    let (status, stdout, _) = run_cli_no_db(&["audit", "verify-pack", path, "--witnesses"]);
    assert!(!status.success(), "{stdout}");
    let wrapped: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(wrapped["verdict"]["status"], "intact", "{stdout}");
    assert_eq!(
        wrapped["witnesses"]["checkpoints"][0]["witnesses"][0]["status"], "invalid",
        "{stdout}"
    );

    // A trust-anchor file implies asking; an unreadable one is operational.
    let (status, stdout, stderr) = run_cli_no_db(&[
        "audit",
        "verify-pack",
        path,
        "--trusted-tsa-file",
        "/nonexistent/tsa.pem",
    ]);
    assert!(!status.success() && stdout.trim().is_empty(), "{stdout}");
    assert!(stderr.contains("trusted TSA file"), "{stderr}");
}

/// A one-shot timestamp authority on localhost that answers with canned
/// bytes and hands back the request it saw. It drives the whole submission
/// path except a real signature.
fn canned_tsa(reply: Vec<u8>) -> (String, std::thread::JoinHandle<String>) {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/tsr", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = vec![0_u8; 8192];
        let n = stream.read(&mut buf).unwrap();
        let head = String::from_utf8_lossy(&buf[..n]).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/timestamp-reply\r\n\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            reply.len()
        );
        stream.write_all(response.as_bytes()).unwrap();
        stream.write_all(&reply).unwrap();
        head
    });
    (url, handle)
}

fn recorded_tsr() -> Vec<u8> {
    std::fs::read(format!(
        "{}/../morpholog-witness/tests/fixtures/rfc3161/genesis_digicert.tsr",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

#[tokio::test(flavor = "current_thread")]
async fn a_response_over_another_head_is_refused_and_the_checkpoint_still_stands() {
    // Attacker capability: an authority (or the path to it) answers with
    // a genuine token over some other head. Nothing of it is stored; the
    // checkpoint is recorded and printed regardless.
    reset_db().await;
    post_balanced_entry("ws1", 100);

    let (url, served) = canned_tsa(recorded_tsr());
    let target = format!("rfc3161:{url}");
    let (status, stdout, stderr) = run_cli(&["audit", "checkpoint", "--witness", &target]);
    assert!(!status.success(), "a failed submission exits one: {stderr}");
    let cp: Value = serde_json::from_str(&stdout).expect("the checkpoint is still printed");
    assert_eq!(cp["status"], "created", "{stdout}");
    assert!(cp.get("witnesses").is_none(), "nothing stored: {stdout}");
    assert!(stderr.contains("does not vouch for this head"), "{stderr}");
    assert!(
        stderr.contains("audit witness --tree-size 1"),
        "the retry is named: {stderr}"
    );
    let request_head = served.join().unwrap();
    assert!(
        request_head.starts_with("POST /tsr HTTP/1.1"),
        "{request_head}"
    );
    assert!(
        request_head.contains("content-type: application/timestamp-query"),
        "{request_head}"
    );

    // The retry path refuses the same answer the same way, storing
    // nothing, and still prints the checkpoint as it stands.
    let (url, _served) = canned_tsa(recorded_tsr());
    let target = format!("rfc3161:{url}");
    let (status, stdout, stderr) =
        run_cli(&["audit", "witness", "--tree-size", "1", "--witness", &target]);
    assert!(!status.success(), "{stderr}");
    let cp: Value = serde_json::from_str(&stdout).expect("the checkpoint is printed");
    assert_eq!(cp["tree_size"], 1);
    assert!(cp.get("witnesses").is_none(), "{stdout}");
    assert!(stderr.contains("nothing stored"), "{stderr}");
    assert!(stderr.contains(&format!("--witness {target}")), "{stderr}");

    // Every authority named is attempted, and the retry names exactly
    // the ones that failed.
    let (url_a, served_a) = canned_tsa(recorded_tsr());
    let (url_b, served_b) = canned_tsa(recorded_tsr());
    let (target_a, target_b) = (format!("rfc3161:{url_a}"), format!("rfc3161:{url_b}"));
    let (status, _, stderr) = run_cli(&[
        "audit",
        "witness",
        "--tree-size",
        "1",
        "--witness",
        &target_a,
        "--witness",
        &target_b,
    ]);
    assert!(!status.success());
    served_a.join().unwrap();
    served_b.join().unwrap();
    assert!(
        stderr.contains(&format!("--witness {target_a} --witness {target_b}")),
        "{stderr}"
    );
    let (status, stdout, _) = run_cli(&["audit", "verify"]);
    assert!(status.success(), "{stdout}");
    assert!(
        serde_json::from_str::<Value>(&stdout)
            .unwrap()
            .get("witnesses")
            .is_none(),
        "{stdout}"
    );

    // An unknown tree size is operational, not a verdict.
    let (status, stdout, stderr) =
        run_cli(&["audit", "witness", "--tree-size", "7", "--witness", &target]);
    assert!(!status.success() && stdout.trim().is_empty());
    assert!(
        stderr.contains("no checkpoint is recorded at tree size 7"),
        "{stderr}"
    );

    // No new rows: nothing is submitted, the head is re-printed, the
    // note names the command that witnesses it.
    let (status, stdout, stderr) = run_cli(&["audit", "checkpoint", "--witness", &target]);
    assert!(status.success(), "{stderr}");
    assert_eq!(
        serde_json::from_str::<Value>(&stdout).unwrap()["status"],
        "no_new_rows"
    );
    assert!(stderr.contains("audit witness --tree-size 1"), "{stderr}");
}

/// The success path needs a real authority's signature, so it runs only
/// when `MORPHOLOG_NETWORK_TESTS` is set: DigiCert's public RFC 3161
/// service witnesses a fresh head, and the live verifier judges it
/// `verified` against DigiCert's published chain.
#[tokio::test(flavor = "current_thread")]
async fn a_public_authority_witnesses_a_fresh_head_end_to_end() {
    if std::env::var_os("MORPHOLOG_NETWORK_TESTS").is_none() {
        eprintln!("skipped: set MORPHOLOG_NETWORK_TESTS=1 to reach the public authority");
        return;
    }
    reset_db().await;
    post_balanced_entry("wn1", 100);
    let (status, stdout, stderr) = run_cli(&[
        "audit",
        "checkpoint",
        "--witness",
        "rfc3161:http://timestamp.digicert.com",
    ]);
    assert!(status.success(), "{stderr}");
    let cp: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(cp["witnesses"][0]["scheme"], "rfc3161", "{stdout}");
    assert_eq!(
        cp["witnesses"][0]["submitted_to"],
        "http://timestamp.digicert.com"
    );

    let chain = format!(
        "{}/../morpholog-witness/tests/fixtures/rfc3161/digicert_chain.pem",
        env!("CARGO_MANIFEST_DIR")
    );
    let (status, stdout, _) = run_cli(&["audit", "verify", "--trusted-tsa-file", &chain]);
    assert!(status.success(), "{stdout}");
    let report: Value = serde_json::from_str(&stdout).unwrap();
    let verdict = &report["witnesses"]["checkpoints"][0]["witnesses"][0];
    assert_eq!(verdict["status"], "verified", "{stdout}");
    assert!(
        report["witnesses"]["earliest_attested_at"].is_string(),
        "{stdout}"
    );
}

/// A ledger posting as a batch or session row, by entry id.
fn posting_row(op: Option<&str>, entry_id: &str) -> String {
    let mut row = serde_json::json!({
        "transformation": "post_simple_entry",
        "actor": "alex",
        "args": serde_json::from_str::<Value>(&ledger_args_json(entry_id, "2026-04-15", "q1_2026", "100")).unwrap(),
    });
    if let Some(op) = op {
        row["op"] = Value::String(op.to_string());
    }
    row.to_string()
}

/// Attacker capability: none. The database refuses one row's delta
/// write (a selective CHECK on the audit table); the runtime must say
/// on every surface that nothing was committed, and carry on with the
/// rows after it under the same constraint.
#[tokio::test(flavor = "current_thread")]
async fn a_refused_delta_write_is_a_known_non_commit_on_every_surface() {
    reset_db().await;
    let pool = PgPool::connect(&database_url()).await.unwrap();
    sqlx::raw_sql(
        "ALTER TABLE morpholog.audit DROP CONSTRAINT IF EXISTS probe;
         ALTER TABLE morpholog.audit ADD CONSTRAINT probe
             CHECK (arguments::text NOT LIKE '%poison%')",
    )
    .execute(&pool)
    .await
    .unwrap();

    // Run everything and remove the constraint before any assertion can
    // fail, since the suite shares this database.
    let one_shot = run_cli(&[
        "propose",
        &ledger_morph(),
        "post_simple_entry",
        "--actor",
        "alex",
        "--args",
        &ledger_args_json("poison_1", "2026-04-15", "q1_2026", "100"),
    ]);
    let batch_input = format!(
        "{}\n{}\n",
        posting_row(None, "poison_2"),
        posting_row(None, "fine_2")
    );
    let batch = {
        let mut child = Command::new(common::bin())
            .args([
                "propose",
                &ledger_morph(),
                "--batch",
                "-",
                "--database-url",
                &database_url(),
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(child.stdin.as_mut().unwrap(), batch_input.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    };
    let session = {
        let mut child = Command::new(common::bin())
            .args([
                "session",
                &ledger_morph(),
                "--database-url",
                &database_url(),
            ])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let input = format!(
            "{}\n{}\n{{\"op\":\"claims\"}}\n",
            posting_row(Some("propose"), "poison_3"),
            posting_row(Some("propose"), "fine_3")
        );
        std::io::Write::write_all(child.stdin.as_mut().unwrap(), input.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    };
    sqlx::raw_sql("ALTER TABLE morpholog.audit DROP CONSTRAINT probe")
        .execute(&pool)
        .await
        .unwrap();

    // One-shot: the coded error object on stdout, exit 1 (not 3), and
    // the same prose on stderr.
    let (status, stdout, stderr) = one_shot;
    assert_eq!(status.code(), Some(1), "{stderr}");
    let error: Value = serde_json::from_str(&stdout).expect("a coded error object on stdout");
    assert_eq!(error["status"], "error", "{stdout}");
    assert_eq!(error["code"], "not_committed", "{stdout}");
    assert!(
        stderr.contains("the proposal was not committed"),
        "{stderr}"
    );
    assert!(!stderr.contains("outcome is unknown"), "{stderr}");

    // Batch: a receipt for the refused row, the next row commits, exit 0.
    assert!(
        batch.status.success(),
        "{}",
        String::from_utf8_lossy(&batch.stderr)
    );
    let receipts: Vec<Value> = String::from_utf8(batch.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(receipts.len(), 2, "{receipts:?}");
    assert_eq!(receipts[0]["status"], "error");
    assert_eq!(receipts[0]["code"], "not_committed", "{}", receipts[0]);
    assert_eq!(receipts[0]["row"], 1);
    assert_eq!(receipts[1]["status"], "committed", "{}", receipts[1]);

    // Session: the same receipt, then a commit, then a read - in step.
    assert!(
        session.status.success(),
        "{}",
        String::from_utf8_lossy(&session.stderr)
    );
    let lines: Vec<Value> = String::from_utf8(session.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 4, "ready plus three answers: {lines:?}");
    assert_eq!(lines[1]["code"], "not_committed", "{}", lines[1]);
    assert_eq!(lines[1]["row"], 1);
    assert_eq!(lines[2]["status"], "committed", "{}", lines[2]);
    assert!(lines[3].is_array(), "the read still answers: {}", lines[3]);
}

/// A register with a gate: the transact tests' refusing act.
const TRANSACT_FIXTURE: &str = "\
program transact_fixture

predicate Account(id: Subject)
predicate Balance(account: Subject, figure: Decimal)
    unique by (account)

transformation open(id):
    admit Account(id)

transformation post(account, figure):
    require Account(account)
    admit Balance(account, figure)
";

fn transact(file: &std::path::Path, acts: &str) -> std::process::Output {
    let mut child = Command::new(common::bin())
        .args(["transact"])
        .arg(file)
        .args(["--acts", "-", "--database-url", &database_url()])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(child.stdin.as_mut().unwrap(), acts.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn act_row(transformation: &str, named: Value) -> String {
    serde_json::json!({"transformation": transformation, "actor": "teller", "args_named": named})
        .to_string()
}

/// Attacker capability: none. The one-shot `transact` prints the one
/// decision with the exit code `propose` would give it, and nothing of
/// a refused batch reaches the record.
#[tokio::test(flavor = "current_thread")]
async fn transact_prints_the_one_decision_and_writes_all_or_nothing() {
    reset_db().await;
    let fixture = common::write_fixture("transact_fixture", TRANSACT_FIXTURE);
    let pool = PgPool::connect(&database_url()).await.unwrap();

    // Refused at act 2: act 1's account was staged and rolled back.
    let refused = transact(
        &fixture.path,
        &format!(
            "{}\n{}\n",
            act_row("open", serde_json::json!({"id": "a1"})),
            act_row(
                "post",
                serde_json::json!({"account": "ghost", "figure": "5"})
            )
        ),
    );
    assert_eq!(refused.status.code(), Some(1));
    let out: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(out["status"], "rejected", "{out}");
    assert_eq!(out["act"], 2);
    assert!(out.get("row").is_none(), "no row on the one-shot: {out}");
    let claims: i64 = sqlx::query_scalar("SELECT count(*) FROM morpholog.claims")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(claims, 0, "nothing of the refused batch was written");

    // Committed: one receipt per act, in order, each with its own id.
    let committed = transact(
        &fixture.path,
        &format!(
            "{}\n\n{}\n",
            act_row("open", serde_json::json!({"id": "a1"})),
            act_row(
                "post",
                serde_json::json!({"account": "a1", "figure": "100"})
            )
        ),
    );
    assert!(
        committed.status.success(),
        "{}",
        String::from_utf8_lossy(&committed.stderr)
    );
    let out: Value = serde_json::from_slice(&committed.stdout).unwrap();
    assert_eq!(out["status"], "committed", "{out}");
    let acts = out["acts"].as_array().unwrap();
    assert_eq!(acts.len(), 2);
    assert_eq!(acts[0]["row"], 1);
    assert_eq!(acts[1]["row"], 2);
    assert_ne!(acts[0]["transition_id"], acts[1]["transition_id"]);

    // A malformed act is one invalid request, named by position, and
    // nothing ran; so is an empty batch.
    let malformed = transact(
        &fixture.path,
        &format!(
            "{}\n{}\n",
            act_row("open", serde_json::json!({"id": "a2"})),
            act_row("post", serde_json::json!({"account": "a2"}))
        ),
    );
    assert_eq!(malformed.status.code(), Some(1));
    let out: Value = serde_json::from_slice(&malformed.stdout).unwrap();
    assert_eq!(out["status"], "error", "{out}");
    assert_eq!(out["code"], "invalid_arguments");
    assert!(out["error"].as_str().unwrap().contains("act 2"), "{out}");
    let empty = transact(&fixture.path, "\n");
    let out: Value = serde_json::from_slice(&empty.stdout).unwrap();
    assert_eq!(out["code"], "invalid_request", "{out}");
    // A misspelt field is a refusal, never a silently ignored key.
    let stray = transact(
        &fixture.path,
        &serde_json::json!({
            "transformation": "open", "actor": "teller", "args_named": {"id": "a3"}, "actr": "x"
        })
        .to_string(),
    );
    let out: Value = serde_json::from_slice(&stray.stdout).unwrap();
    assert_eq!(out["code"], "invalid_request", "{out}");
    assert!(out["error"].as_str().unwrap().contains("act 1"), "{out}");
    let claims: i64 = sqlx::query_scalar("SELECT count(*) FROM morpholog.claims")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(claims, 2, "only the committed batch's claims exist");

    // A known non-commit of the whole batch is the coded error object at
    // exit 1, and nothing of it was written.
    sqlx::raw_sql(
        "ALTER TABLE morpholog.audit ADD CONSTRAINT probe CHECK (arguments::text NOT LIKE '%poison%')",
    )
    .execute(&pool)
    .await
    .unwrap();
    let not_committed = transact(
        &fixture.path,
        &format!(
            "{}\n{}\n",
            act_row("open", serde_json::json!({"id": "fine"})),
            act_row("open", serde_json::json!({"id": "poison"}))
        ),
    );
    let claims_after: i64 = sqlx::query_scalar("SELECT count(*) FROM morpholog.claims")
        .fetch_one(&pool)
        .await
        .unwrap();
    sqlx::raw_sql("ALTER TABLE morpholog.audit DROP CONSTRAINT probe")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(not_committed.status.code(), Some(1));
    let out: Value = serde_json::from_slice(&not_committed.stdout).unwrap();
    assert_eq!(out["code"], "not_committed", "{out}");
    assert_eq!(
        claims_after, 2,
        "the first act did not survive the second's refusal"
    );
}
