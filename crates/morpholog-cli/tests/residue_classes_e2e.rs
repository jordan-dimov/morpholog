//! The residue-classes example through the binary: the historical
//! verification, and the login binding of the checker's name.
//!
//! The attacker modelled holds a receipt, the packs and the programme and
//! may present any receipt against any history; and holds a database login
//! that is not the checker's and tries to record a check under the
//! checker's name. Verification must say which history a receipt is about,
//! and the name must be refused before the body runs. What is not modelled
//! is a checker that reports falsely: the record attributes a report and
//! pins what it was about, never that the checker ran.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{database_url, reset_db};

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use serde_json::{Value, json};

const EXPLORER: &str = "explorer";
const OPERATOR: &str = "operator";
const CHECKER: &str = "lean4";
const LEMMA: &str = "reduction_respects_arithmetic";
const SQUARES: &str = "squares_are_zero_or_one";
const ODD_LEGS: &str = "no_square_is_a_sum_of_two_odd_squares";

fn run(args: &[&str], db: bool) -> (ExitStatus, String, String) {
    let mut command = Command::new(common::bin());
    command.args(args);
    if db {
        command.args(["--database-url", &database_url()]);
    }
    let output = command.output().expect("spawn morpholog binary");
    (
        output.status,
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

fn example_dir() -> PathBuf {
    common::repo_root().join("examples/22_residue_classes")
}

fn program() -> PathBuf {
    example_dir().join("residue_classes.morph")
}

fn subject(s: &str) -> Value {
    json!({"type": "subject", "value": s})
}

fn decimal(n: i64) -> Value {
    json!({"type": "decimal", "value": n.to_string()})
}

fn residues(modulus: i64) -> Value {
    json!({"type": "collection", "value": (0..modulus).map(decimal).collect::<Vec<_>>()})
}

/// Propose as `actor` and return the exit status with stdout parsed.
fn propose(actor: &str, transformation: &str, args: &[Value]) -> (ExitStatus, Value) {
    let file = program();
    let args = Value::Array(args.to_vec()).to_string();
    let (status, stdout, stderr) = run(
        &[
            "propose",
            file.to_str().unwrap(),
            transformation,
            "--actor",
            actor,
            "--args",
            &args,
        ],
        true,
    );
    let outcome: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!("{transformation}: no JSON on stdout ({e}): {stdout}; {stderr}")
    });
    (status, outcome)
}

fn committed(actor: &str, transformation: &str, args: &[Value]) {
    let (status, outcome) = propose(actor, transformation, args);
    assert!(status.success(), "{transformation}: {outcome}");
    assert_eq!(
        outcome["status"], "committed",
        "{transformation}: {outcome}"
    );
}

async fn session_user() -> String {
    let pool = morpholog_postgres::PgPool::connect(&database_url())
        .await
        .expect("connect to test DB");
    sqlx::query_scalar::<_, String>("select session_user::text")
        .fetch_one(&pool)
        .await
        .unwrap()
}

/// The lemma file's digest, as the checker would record it.
fn lemma_digest() -> String {
    let output = Command::new("python3")
        .args([
            "-c",
            "import hashlib, sys; print(hashlib.sha256(open(sys.argv[1], 'rb').read()).hexdigest())",
            example_dir().join("reduction.lean").to_str().unwrap(),
        ])
        .output()
        .expect("python3 hashes the lemma file");
    assert!(output.status.success());
    format!(
        "sha256:{}",
        String::from_utf8(output.stdout).unwrap().trim()
    )
}

fn checkpoint() -> u64 {
    let (status, stdout, stderr) = run(&["audit", "checkpoint"], true);
    assert!(status.success(), "{stderr}");
    let created: Value = serde_json::from_str(&stdout).unwrap();
    created["tree_size"]
        .as_u64()
        .unwrap_or_else(|| panic!("a checkpoint with a tree size: {created}"))
}

fn export(dir: &Path, name: &str) -> PathBuf {
    let (status, stdout, stderr) = run(&["audit", "export"], true);
    assert!(status.success(), "{stderr}");
    let path = dir.join(name);
    std::fs::write(&path, stdout).unwrap();
    path
}

fn receipt(dir: &Path, name: &str, pack: &Path) -> (PathBuf, Value) {
    let (status, stdout, stderr) = run(
        &[
            "audit",
            "receipt",
            program().to_str().unwrap(),
            "--pack",
            pack.to_str().unwrap(),
            "--derived",
            "Grounded",
        ],
        false,
    );
    assert!(status.success(), "{stderr}");
    let path = dir.join(name);
    std::fs::write(&path, &stdout).unwrap();
    (path, serde_json::from_str(&stdout).unwrap())
}

fn verify(receipt: &Path, pack: &Path) -> (bool, Value) {
    let (status, stdout, stderr) = run(
        &[
            "audit",
            "verify-receipt",
            program().to_str().unwrap(),
            "--receipt",
            receipt.to_str().unwrap(),
            "--pack",
            pack.to_str().unwrap(),
        ],
        false,
    );
    let report: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("a report on stdout ({e}): {stdout:?}; {stderr}"));
    assert_eq!(report["passes"], status.success(), "{report}");
    (status.success(), report)
}

fn conclusions(answer: &Value) -> Vec<String> {
    let mut names: Vec<String> = answer
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["args"][0]["value"].as_str().unwrap().to_string())
        .collect();
    names.sort();
    names
}

/// Both parties appointed under this suite's own login, the domain and
/// table modulo 4 proposed, both statements verified.
async fn verified_world() {
    reset_db().await;
    let login = session_user().await;
    committed(
        OPERATOR,
        "appoint_operator",
        &[subject(OPERATOR), subject(&login)],
    );
    committed(
        OPERATOR,
        "appoint_checker",
        &[subject(CHECKER), subject(&login)],
    );
    committed(EXPLORER, "declare_domain", &[decimal(4), residues(4)]);
    for r in 0..4 {
        committed(
            EXPLORER,
            "tabulate_square",
            &[decimal(4), decimal(r), decimal(r * r % 4)],
        );
    }
    committed(EXPLORER, "verify", &[subject(SQUARES), decimal(4)]);
    committed(EXPLORER, "verify", &[subject(ODD_LEGS), decimal(4)]);
}

#[tokio::test(flavor = "current_thread")]
async fn a_receipt_states_what_stood_at_its_checkpoint_and_a_withdrawal_does_not_rewrite_it() {
    verified_world().await;
    committed(
        CHECKER,
        "record_check",
        &[
            subject(LEMMA),
            subject("lean4:v4.35.0"),
            subject(&lemma_digest()),
        ],
    );
    committed(
        EXPLORER,
        "deduce",
        &[
            subject("every_square_is_zero_or_one_mod_four"),
            subject(SQUARES),
            decimal(4),
            subject(LEMMA),
        ],
    );
    committed(
        EXPLORER,
        "deduce",
        &[
            subject("no_pythagorean_triple_has_two_odd_legs"),
            subject(ODD_LEGS),
            decimal(4),
            subject(LEMMA),
        ],
    );

    let dir = tempfile::tempdir().unwrap();
    let first = checkpoint();
    let pack_one = export(dir.path(), "pack1.ndjson");
    let (receipt_one, stated) = receipt(dir.path(), "receipt1.json", &pack_one);
    assert_eq!(stated["checkpoint"]["tree_size"], first);
    assert_eq!(
        conclusions(&stated["answer"]),
        [
            "every_square_is_zero_or_one_mod_four",
            "no_pythagorean_triple_has_two_odd_legs"
        ]
    );
    let (passed, report) = verify(&receipt_one, &pack_one);
    assert!(passed, "{report}");
    assert_eq!(report["completeness"]["status"], "complete");
    assert_eq!(report["checkpoint"]["status"], "matches");
    assert_eq!(report["program"]["status"], "matches");
    assert_eq!(report["evaluation"]["status"], "reproduced");

    // The checker withdraws. The read empties; every claim stays.
    committed(
        CHECKER,
        "withdraw_check",
        &[subject(LEMMA), subject("proof_regressed")],
    );
    let second = checkpoint();
    assert_eq!(second, first + 1);
    let pack_two = export(dir.path(), "pack2.ndjson");
    let (receipt_two, later) = receipt(dir.path(), "receipt2.json", &pack_two);
    assert!(later["answer"].as_array().unwrap().is_empty(), "{later}");
    let (passed, report) = verify(&receipt_two, &pack_two);
    assert!(passed, "{report}");
    let (status, claims, stderr) = run(
        &[
            "inspect",
            "claims",
            "--predicate",
            "Checked",
            "--predicate",
            "Deduced",
        ],
        true,
    );
    assert!(status.success(), "{stderr}");
    assert_eq!(
        serde_json::from_str::<Value>(&claims)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        3
    );

    // The first receipt is still a true statement about the first
    // history, and about nothing else.
    let (passed, report) = verify(&receipt_one, &pack_one);
    assert!(passed, "{report}");
    let (passed, report) = verify(&receipt_one, &pack_two);
    assert!(!passed);
    assert_eq!(report["checkpoint"]["status"], "differs");
    assert_eq!(report["checkpoint"]["receipt"]["tree_size"], first);
    assert_eq!(report["checkpoint"]["pack"]["tree_size"], second);
    assert_eq!(report["evaluation"]["status"], "not_evaluated");
}

#[tokio::test(flavor = "current_thread")]
async fn a_login_the_checkers_name_is_not_bound_to_cannot_record_a_check() {
    verified_world().await;
    committed(
        OPERATOR,
        "appoint_checker",
        &[subject("other_checker"), subject("mtest_rc_nobody")],
    );
    let (status, outcome) = propose(
        "other_checker",
        "record_check",
        &[subject(LEMMA), subject("v"), subject("sha256:x")],
    );
    assert!(!status.success());
    assert_eq!(
        outcome["code"], "actor_assertion_unauthorised",
        "refused before the body runs: {outcome}"
    );
    // The explorer's own name is unrestricted; it is the gate that refuses it.
    let (status, outcome) = propose(
        EXPLORER,
        "record_check",
        &[subject(LEMMA), subject("v"), subject("sha256:x")],
    );
    assert!(!status.success());
    assert_eq!(outcome["status"], "rejected", "{outcome}");
    assert!(
        outcome["reason"]
            .as_str()
            .unwrap()
            .contains("Checker(actor)"),
        "{outcome}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_dry_run_names_the_row_that_refuses_squares_modulo_eight() {
    reset_db().await;
    committed(EXPLORER, "declare_domain", &[decimal(8), residues(8)]);
    for r in 0..8 {
        committed(
            EXPLORER,
            "tabulate_square",
            &[decimal(8), decimal(r), decimal(r * r % 8)],
        );
    }
    let args = Value::Array(vec![subject(SQUARES), decimal(8)]).to_string();
    let (status, stdout, stderr) = run(
        &[
            "explain",
            program().to_str().unwrap(),
            "verify",
            "--actor",
            EXPLORER,
            "--args",
            &args,
            "--json",
        ],
        true,
    );
    assert!(status.success(), "{stderr}");
    let explanation: Value = serde_json::from_str(&stdout).unwrap();
    let rejected = &explanation["verdict"]["rejected"];
    assert_eq!(rejected["kind"], "invariant", "{explanation}");
    assert_eq!(rejected["name"], "squares_are_zero_or_one", "{explanation}");
    let witness = rejected["witness"].as_array().unwrap();
    assert!(
        witness
            .iter()
            .any(|b| b["var"] == "s" && b["value"] == decimal(4)),
        "{explanation}"
    );
}

/// The explorer script, when `lean` is on the path, tells the whole story
/// against the test database; without it, it says what it needs and does
/// nothing.
#[tokio::test(flavor = "current_thread")]
async fn the_explorer_script_runs_the_story_or_says_what_it_needs() {
    reset_db().await;
    let bin_dir = Path::new(common::bin()).parent().unwrap();
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new("python3")
        .arg(example_dir().join("explore.py"))
        .env("DATABASE_URL", database_url())
        .env("MORPHOLOG_BIN", common::bin())
        .env("PATH", &path)
        .output()
        .expect("python3 runs the explorer");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let lean_available = Command::new("lean")
        .env("PATH", &path)
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if lean_available {
        assert!(output.status.success(), "{stdout}\n{stderr}");
        assert!(stdout.contains("Grounded: 2"), "{stdout}");
        assert!(
            stdout.contains("refused by squares_are_zero_or_one (m = 8, s = 4)"),
            "{stdout}"
        );
        assert!(stdout.contains("2 grounded, passes = True"), "{stdout}");
        assert!(
            stdout.contains("0 grounded, Checked rows still on the record: 1"),
            "{stdout}"
        );
        assert!(
            stdout.contains("passes = False, checkpoint = differs"),
            "{stdout}"
        );
    } else {
        assert_eq!(output.status.code(), Some(2), "{stdout}\n{stderr}");
        assert!(stderr.contains("lean is not on the path"), "{stderr}");
        assert!(stdout.is_empty(), "{stdout}");
    }
}
