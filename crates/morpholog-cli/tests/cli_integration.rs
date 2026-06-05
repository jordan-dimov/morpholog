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

/// Issue a balanced journal entry via the CLI's `run` subcommand against
/// the shipped ledger example. Returns the `transition_id` from the
/// receipt so subsequent tests can use it as an as-of coordinate.
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
        "run",
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
        "run",
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
        "run",
        &ledger_morph(),
        "close_period",
        "--actor",
        "alex",
        "--args",
        r#"[{"type":"subject","value":"q1_2026"}]"#,
    ]);
    assert!(status.success(), "close_period should commit");

    let (status, stdout, _stderr) = run_cli(&[
        "run",
        &ledger_morph(),
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
    let args_json = r#"[
        {"type":"subject","value":"entry_001"},
        {"type":"subject","value":"2026-04-15"},
        {"type":"subject","value":"q1_2026"},
        {"type":"subject","value":"account_cash"},
        {"type":"subject","value":"account_revenue"},
        {"type":"decimal","value":"100"}
    ]"#;
    let (status, stdout, stderr) = run_cli(&[
        "run",
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
        "run",
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

/// Missing a declared parameter in the `--args-named` object is a
/// hard error before any database work. The error names the missing
/// parameter and points at `morpholog schema` for the accepted
/// shape.
#[tokio::test(flavor = "current_thread")]
async fn run_args_named_missing_required_errors_with_schema_hint() {
    reset_db().await;
    let path = write_temp_ledger_morph();
    let args_named = r#"{
        "entry_id":"018f0000-0000-7000-8000-000000000011",
        "posting_date":"018f0000-0000-7000-8000-000000000012",
        "period":"018f0000-0000-7000-8000-000000000013",
        "debit_account":"018f0000-0000-7000-8000-000000000014",
        "credit_account":"018f0000-0000-7000-8000-000000000015"
    }"#;
    let (status, _stdout, stderr) = run_cli(&[
        "run",
        path.to_str().unwrap(),
        "post_simple_entry",
        "--actor",
        "alex",
        "--args-named",
        args_named,
    ]);
    assert!(!status.success(), "missing required parameter must error");
    assert!(
        stderr.contains("missing required parameter `amount`"),
        "stderr should name the missing parameter; got: {stderr}"
    );
    assert!(
        stderr.contains("morpholog schema"),
        "error should point at the schema subcommand; got: {stderr}"
    );
}

/// An unknown key in `--args-named` is a hard error. The error lists
/// the parameters that ARE accepted, so a typo surfaces clearly
/// rather than as "missing required" (which would point at the
/// wrong target).
#[tokio::test(flavor = "current_thread")]
async fn run_args_named_unknown_key_errors_with_expected_names() {
    reset_db().await;
    let path = write_temp_ledger_morph();
    let args_named = r#"{
        "entry_id":"018f0000-0000-7000-8000-000000000021",
        "posting_date":"018f0000-0000-7000-8000-000000000022",
        "period":"018f0000-0000-7000-8000-000000000023",
        "debit_account":"018f0000-0000-7000-8000-000000000024",
        "credit_account":"018f0000-0000-7000-8000-000000000025",
        "amount":"100",
        "amaount":"100"
    }"#;
    let (status, _stdout, stderr) = run_cli(&[
        "run",
        path.to_str().unwrap(),
        "post_simple_entry",
        "--actor",
        "alex",
        "--args-named",
        args_named,
    ]);
    assert!(!status.success(), "unknown key must error");
    assert!(
        stderr.contains("unknown parameter(s) `amaount`"),
        "stderr should name the unknown key; got: {stderr}"
    );
    assert!(
        stderr.contains("amount") && stderr.contains("entry_id"),
        "stderr should list the expected parameter names; got: {stderr}"
    );
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
        "run",
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
        let (status, _stdout, stderr) = run_cli(&[
            "run",
            path.to_str().unwrap(),
            "post_simple_entry",
            "--actor",
            "alex",
            "--args-named",
            &args_named,
        ]);
        assert!(
            !status.success(),
            "decimal `{bad}` is outside the schema pattern and must be rejected"
        );
        assert!(
            stderr.contains("does not match the schema pattern"),
            "stderr should name the schema-pattern mismatch for `{bad}`; got: {stderr}"
        );
    }
}

/// Wrong JSON type errors with the expected kind label so the
/// embedder can see WHICH parameter went wrong and WHAT kind it
/// should be.
#[tokio::test(flavor = "current_thread")]
async fn run_args_named_wrong_type_errors_with_kind_label() {
    reset_db().await;
    let path = write_temp_ledger_morph();
    let args_named = r#"{
        "entry_id":"018f0000-0000-7000-8000-000000000051",
        "posting_date":"018f0000-0000-7000-8000-000000000052",
        "period":"018f0000-0000-7000-8000-000000000053",
        "debit_account":"018f0000-0000-7000-8000-000000000054",
        "credit_account":"018f0000-0000-7000-8000-000000000055",
        "amount": true
    }"#;
    let (status, _stdout, stderr) = run_cli(&[
        "run",
        path.to_str().unwrap(),
        "post_simple_entry",
        "--actor",
        "alex",
        "--args-named",
        args_named,
    ]);
    assert!(!status.success(), "wrong JSON type must error");
    assert!(
        stderr.contains("`amount` is Decimal but received boolean"),
        "stderr should name the parameter, the expected kind, and the actual JSON type; got: {stderr}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn run_errors_with_available_list_on_unknown_transformation() {
    reset_db().await;
    let path = write_temp_ledger_morph();
    let args_json = r#"[]"#;
    let (status, _stdout, stderr) = run_cli(&[
        "run",
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
        "run",
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
        "run",
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
    let args_json = r#"[
        {"type":"subject","value":"entry_002"},
        {"type":"subject","value":"2026-04-15"},
        {"type":"subject","value":"q1_2026"},
        {"type":"subject","value":"account_cash"},
        {"type":"subject","value":"account_revenue"},
        {"type":"decimal","value":"50"}
    ]"#;
    let (status, stdout, _stderr) = run_cli(&[
        "run",
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

#[tokio::test(flavor = "current_thread")]
async fn inspect_outbox_defaults_to_pending() {
    reset_db().await;
    seed_two_pending_outbox_rows();
    // Drive one row through to `delivered` so we can confirm it
    // does NOT appear in the default (pending) listing.
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

    let (status, stdout, _stderr) = run_cli(&["inspect", "outbox"]);
    assert!(status.success());
    let rows: Value = serde_json::from_str(&stdout).unwrap();
    let arr = rows.as_array().expect("inspect outbox emits a JSON array");
    assert_eq!(
        arr.len(),
        1,
        "default should show the remaining pending row only"
    );
    assert_eq!(arr[0]["status"], "pending");
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_outbox_status_all_shows_every_row() {
    reset_db().await;
    seed_two_pending_outbox_rows();
    // Drive one row to `delivered`.
    let claim_out: Value = serde_json::from_str(
        &run_cli(&["outbox", "claim", "--intent-type", "JournalEntryPosted"]).1,
    )
    .unwrap();
    let intent_id = claim_out["row"]["intent_id"].as_str().unwrap().to_string();
    let worker_id = claim_out["row"]["locked_by"].as_str().unwrap().to_string();
    run_cli(&[
        "outbox",
        "complete",
        &intent_id,
        "--worker-id",
        &worker_id,
        "--outcome",
        "delivered",
    ]);

    let (status, stdout, _stderr) = run_cli(&["inspect", "outbox", "--status", "all"]);
    assert!(status.success());
    let rows: Value = serde_json::from_str(&stdout).unwrap();
    let arr = rows.as_array().unwrap();
    assert_eq!(arr.len(), 2, "all should show both rows; got {arr:?}");
}

#[tokio::test(flavor = "current_thread")]
async fn inspect_outbox_status_delivered_shows_only_delivered_rows() {
    reset_db().await;
    seed_two_pending_outbox_rows();
    let claim_out: Value = serde_json::from_str(
        &run_cli(&["outbox", "claim", "--intent-type", "JournalEntryPosted"]).1,
    )
    .unwrap();
    let intent_id = claim_out["row"]["intent_id"].as_str().unwrap().to_string();
    let worker_id = claim_out["row"]["locked_by"].as_str().unwrap().to_string();
    run_cli(&[
        "outbox",
        "complete",
        &intent_id,
        "--worker-id",
        &worker_id,
        "--outcome",
        "delivered",
    ]);

    let (status, stdout, _stderr) = run_cli(&["inspect", "outbox", "--status", "delivered"]);
    assert!(status.success());
    let rows: Value = serde_json::from_str(&stdout).unwrap();
    let arr = rows.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["status"], "delivered");
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
    let args_json = r#"[
        {"type":"subject","value":"e2e_entry"},
        {"type":"subject","value":"2026-05-01"},
        {"type":"subject","value":"q2_2026"},
        {"type":"subject","value":"account_cash"},
        {"type":"subject","value":"account_revenue"},
        {"type":"decimal","value":"500"}
    ]"#;
    let (status, stdout, stderr) = run_cli(&[
        "run",
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
