//! Evaluation receipts end to end: a borrower's history governed by the
//! borrowing-base example, a checkpoint and pack exported from it, and a
//! lender recomputing the facility utilisation a receipt states, offline.
//!
//! The attacker modelled holds the receipt, the pack and the programme
//! file, and may edit any of them. Verification must catch any edit that
//! makes the receipt's statement disagree with recomputation, and report
//! which layer it broke. It cannot catch a rewrite into another true
//! statement: an unsigned receipt proves its statement, not its origin.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{database_url, reset_db, write_fixture};

use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use serde_json::{Value, json};

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

fn borrowing_base() -> PathBuf {
    common::repo_root().join("examples/11_borrowing_base/borrowing_base.morph")
}

fn subject(s: &str) -> Value {
    json!({"type": "subject", "value": s})
}

fn decimal(s: &str) -> Value {
    json!({"type": "decimal", "value": s})
}

/// Propose against the borrowing base and return the transition id.
fn propose(transformation: &str, args: &[Value]) -> String {
    let file = borrowing_base();
    let args = Value::Array(args.to_vec()).to_string();
    let (status, stdout, stderr) = run(
        &[
            "propose",
            file.to_str().unwrap(),
            transformation,
            "--actor",
            "lender_ops",
            "--args",
            &args,
        ],
        true,
    );
    assert!(status.success(), "{transformation}: {stderr}");
    let outcome: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(outcome["status"], "committed", "{stdout}");
    outcome["transition_id"].as_str().unwrap().to_string()
}

fn checkpoint() {
    let (status, _, stderr) = run(&["audit", "checkpoint"], true);
    assert!(status.success(), "{stderr}");
}

/// Export with `extra` flags and write the pack into `dir`.
fn export(dir: &Path, name: &str, extra: &[&str]) -> PathBuf {
    let mut args = vec!["audit", "export"];
    args.extend_from_slice(extra);
    let (status, stdout, stderr) = run(&args, true);
    assert!(status.success(), "{stderr}");
    write(dir, name, &stdout)
}

fn write(dir: &Path, name: &str, content: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, content).unwrap();
    path
}

/// Two facilities: f1 draws 600 against 1500 of collateral, f2 50
/// against 200. Returns the last transition's id.
fn borrower_history() -> String {
    propose("open_facility", &[subject("f1"), decimal("0.8")]);
    propose(
        "pledge_collateral",
        &[subject("f1"), subject("a1"), decimal("1000")],
    );
    propose(
        "pledge_collateral",
        &[subject("f1"), subject("a2"), decimal("500")],
    );
    propose("draw", &[subject("f1"), subject("d1"), decimal("600")]);
    propose("open_facility", &[subject("f2"), decimal("0.5")]);
    propose(
        "pledge_collateral",
        &[subject("f2"), subject("a3"), decimal("200")],
    );
    propose("draw", &[subject("f2"), subject("d2"), decimal("50")])
}

fn issue(program: &Path, pack: &Path, derived: &str) -> (ExitStatus, String, String) {
    run(
        &[
            "audit",
            "receipt",
            program.to_str().unwrap(),
            "--pack",
            pack.to_str().unwrap(),
            "--derived",
            derived,
        ],
        false,
    )
}

fn issued(program: &Path, pack: &Path, derived: &str) -> Value {
    let (status, stdout, stderr) = issue(program, pack, derived);
    assert!(status.success(), "{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

fn verify(program: &Path, receipt: &Path, pack: &Path, extra: &[&str]) -> (bool, Value) {
    let mut args = vec![
        "audit",
        "verify-receipt",
        program.to_str().unwrap(),
        "--receipt",
        receipt.to_str().unwrap(),
        "--pack",
        pack.to_str().unwrap(),
    ];
    args.extend_from_slice(extra);
    let (status, stdout, stderr) = run(&args, false);
    let report: Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("a report on stdout ({e}): {stdout:?}; {stderr}"));
    assert_eq!(
        report["passes"],
        status.success(),
        "the report says what the exit code says: {report}"
    );
    (status.success(), report)
}

/// Every layer's status, in report order, for one assertion per case.
fn statuses(report: &Value) -> [String; 5] {
    [
        "receipt",
        "completeness",
        "checkpoint",
        "program",
        "evaluation",
    ]
    .map(|layer| report[layer]["status"].as_str().unwrap().to_string())
}

const ALL_HOLD: [&str; 5] = [
    "well_formed",
    "complete",
    "matches",
    "matches",
    "reproduced",
];

/// A borrower's history checkpointed and exported, with a receipt for
/// facility utilisation over it.
struct World {
    dir: tempfile::TempDir,
    pack: PathBuf,
    receipt: Value,
    last_transition: String,
}

impl World {
    async fn new() -> Self {
        reset_db().await;
        let last_transition = borrower_history();
        checkpoint();
        let dir = tempfile::tempdir().unwrap();
        let pack = export(dir.path(), "pack.ndjson", &[]);
        let receipt = issued(&borrowing_base(), &pack, "FacilityUtilisation");
        Self {
            dir,
            pack,
            receipt,
            last_transition,
        }
    }

    /// Write `receipt` and verify it against this world's pack.
    fn check(&self, receipt: &Value, program: &Path) -> (bool, Value) {
        let path = write(self.dir.path(), "receipt.json", &receipt.to_string());
        verify(program, &path, &self.pack, &[])
    }

    fn edited(&self, edit: impl FnOnce(&mut Value)) -> (bool, Value) {
        let mut receipt = self.receipt.clone();
        edit(&mut receipt);
        self.check(&receipt, &borrowing_base())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_receipt_recomputes_from_any_form_of_the_pack_and_agrees_with_the_live_read() {
    let world = World::new().await;
    let receipt = &world.receipt;
    assert_eq!(receipt["receipt_format_version"], 1);
    assert_eq!(receipt["semantics_version"], 2);
    assert_eq!(receipt["checkpoint"]["tree_size"], 7);
    assert_eq!(
        receipt["query"],
        json!({"kind": "derived", "predicate": "FacilityUtilisation"})
    );
    let (status, hash, _) = run(&["hash", borrowing_base().to_str().unwrap()], false);
    assert!(status.success());
    assert_eq!(
        receipt["program_hash"],
        serde_json::from_str::<Value>(&hash).unwrap()["hash"]
    );

    // The answer is the live derived read at the checkpoint's last
    // transition, in canonical order.
    let (status, live, stderr) = run(
        &[
            "inspect",
            "derived",
            borrowing_base().to_str().unwrap(),
            "FacilityUtilisation",
            "--as-of",
            &world.last_transition,
        ],
        true,
    );
    assert!(status.success(), "{stderr}");
    let mut live: Vec<Value> = serde_json::from_str(&live).unwrap();
    live.sort_by_key(Value::to_string);
    assert_eq!(receipt["answer"], Value::Array(live));
    assert_eq!(receipt["answer"].as_array().unwrap().len(), 2);

    let (passed, report) = world.check(receipt, &borrowing_base());
    assert!(passed, "{report}");
    assert_eq!(statuses(&report), ALL_HOLD.map(String::from));
    assert_eq!(report["evidence"]["verdict"]["status"], "intact");
    assert_eq!(report["verdict_kind"], "prefix");

    // The same history as one document, and gzip-compressed.
    let lines: Vec<Value> = std::fs::read_to_string(&world.pack)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let rows = 1 + lines[0]["checkpoint_count"].as_u64().unwrap() as usize;
    let document = json!({
        "manifest": {
            "pack_format_version": lines[0]["pack_format_version"],
            "pack_kind": lines[0]["pack_kind"],
            "morpholog_version": lines[0]["morpholog_version"],
            "tree_size": lines[0]["tree_size"],
            "root_hash": lines[0]["root_hash"],
            "checkpoint_hash": lines[0]["checkpoint_hash"],
        },
        "checkpoints": lines[1..rows],
        "rows": lines[rows..],
    });
    let document = write(world.dir.path(), "pack.json", &document.to_string());
    let gzipped = world.dir.path().join("pack.ndjson.gz");
    let mut encoder = flate2::write::GzEncoder::new(
        std::fs::File::create(&gzipped).unwrap(),
        flate2::Compression::default(),
    );
    std::io::Write::write_all(&mut encoder, &std::fs::read(&world.pack).unwrap()).unwrap();
    encoder.finish().unwrap();
    let receipt_path = write(world.dir.path(), "receipt.json", &receipt.to_string());
    for pack in [&document, &gzipped] {
        let (passed, report) = verify(&borrowing_base(), &receipt_path, pack, &[]);
        assert!(passed, "{}: {report}", pack.display());
        assert_eq!(
            issued(&borrowing_base(), pack, "FacilityUtilisation"),
            *receipt
        );
    }

    // The receipt binds what the programme means, not its source bytes.
    let source = std::fs::read_to_string(borrowing_base()).unwrap();
    let recommented = write_fixture(
        "borrowing_base",
        &format!("-- A lender's copy, annotated.\n\n{source}"),
    );
    let (passed, report) = world.check(receipt, &recommented);
    assert!(passed, "a comment changes no meaning: {report}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_receipt_is_refused_against_another_history_or_rulebook() {
    let world = World::new().await;
    let receipt = world.receipt.clone();

    // A programme that means something else.
    let source = std::fs::read_to_string(borrowing_base()).unwrap();
    let amended = write_fixture(
        "borrowing_base",
        &format!("{source}\npredicate CovenantWaiver(facility: Subject)\n"),
    );
    let (passed, report) = world.check(&receipt, &amended);
    assert!(!passed);
    assert_eq!(
        statuses(&report),
        [
            "well_formed",
            "complete",
            "matches",
            "differs",
            "not_evaluated"
        ]
        .map(String::from)
    );
    assert_eq!(report["program"]["receipt"], receipt["program_hash"]);

    // A receipt naming another programme.
    let (passed, report) = world.edited(|r| {
        r["program_hash"] = json!(format!("sha256:{}", "0".repeat(64)));
    });
    assert!(!passed);
    assert_eq!(report["program"]["status"], "differs");
    assert_eq!(report["evaluation"]["status"], "not_evaluated");

    // A receipt naming another checkpoint than the pack's.
    let (passed, report) = world.edited(|r| r["checkpoint"]["tree_size"] = json!(6));
    assert!(!passed);
    assert_eq!(report["checkpoint"]["status"], "differs", "{report}");
    assert_eq!(report["checkpoint"]["pack"]["tree_size"], 7);
    assert_eq!(report["evaluation"]["status"], "not_evaluated");

    // A longer history is another checkpoint: it does not reproduce an
    // older receipt, even though it contains that history.
    propose("draw", &[subject("f2"), subject("d3"), decimal("10")]);
    checkpoint();
    let longer = export(world.dir.path(), "longer.ndjson", &[]);
    let path = write(world.dir.path(), "receipt.json", &receipt.to_string());
    let (passed, report) = verify(&borrowing_base(), &path, &longer, &[]);
    assert!(!passed);
    assert_eq!(report["checkpoint"]["status"], "differs", "{report}");

    // An edited row breaks the evidence, which then decides everything
    // after it, whatever policy was asked for.
    let mut lines: Vec<Value> = std::fs::read_to_string(&world.pack)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let last = lines.len() - 1;
    let tampered_row = lines[last].to_string().replace("\"50\"", "\"5\"");
    assert_ne!(
        tampered_row,
        lines[last].to_string(),
        "the draw is in the last row"
    );
    lines[last] = serde_json::from_str(&tampered_row).unwrap();
    let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let tampered = write(world.dir.path(), "tampered.ndjson", &text);

    // A witness that does not vouch for its head fails the receipt as it
    // fails `verify-pack`, but never subtracts from the tree's own verdict:
    // the answer still recomputes over the intact history.
    let mut witnessed: Vec<Value> = std::fs::read_to_string(&world.pack)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    witnessed[1]["witnesses"] = json!([{
        "scheme": "rfc3161",
        "proof": "not base64!",
        "submitted_to": "https://tsa.example",
    }]);
    let text: String = witnessed.iter().map(|l| format!("{l}\n")).collect();
    let witnessed = write(world.dir.path(), "witnessed.ndjson", &text);
    let (passed, report) = verify(&borrowing_base(), &path, &witnessed, &["--witnesses"]);
    assert!(!passed);
    assert_eq!(
        report["evidence"]["verdict"]["status"], "intact",
        "{report}"
    );
    assert_eq!(
        report["evidence"]["witnesses"]["checkpoints"][0]["witnesses"][0]["status"], "invalid",
        "{report}"
    );
    assert_eq!(statuses(&report), ALL_HOLD.map(String::from));
    for extra in [&[][..], &["--require-signatures"][..]] {
        let (passed, report) = verify(&borrowing_base(), &path, &tampered, extra);
        assert!(!passed);
        assert_eq!(
            report["evidence"]["verdict"]["status"], "tampered",
            "{report}"
        );
        assert_eq!(
            statuses(&report),
            [
                "well_formed",
                "not_checked",
                "not_checked",
                "matches",
                "not_evaluated"
            ]
            .map(String::from)
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_receipt_whose_statement_does_not_recompute_is_refused_by_layer() {
    let world = World::new().await;

    // A row dropped, and a row altered.
    let (passed, report) = world.edited(|r| {
        r["answer"].as_array_mut().unwrap().pop();
    });
    assert!(!passed);
    assert_eq!(
        report["evaluation"],
        json!({"status": "differs", "missing": 1, "unexpected": 0})
    );
    let (passed, report) = world.edited(|r| {
        let row = r["answer"][0].to_string().replace("0.4", "0.3");
        r["answer"][0] = serde_json::from_str(&row).unwrap();
    });
    assert!(!passed);
    assert_eq!(
        report["evaluation"],
        json!({"status": "differs", "missing": 1, "unexpected": 1}),
        "{report}"
    );

    // A query the programme does not derive is named, never read as an
    // empty answer.
    let (passed, report) = world.edited(|r| {
        r["query"]["predicate"] = json!("Wibble");
        r["answer"] = json!([]);
    });
    assert!(!passed);
    assert_eq!(
        report["evaluation"],
        json!({"status": "query_unknown", "predicate": "Wibble"})
    );

    // Another semantics, with every other layer holding, is not
    // re-evaluated rather than reported as a mismatch: the prior
    // version a v0.0.14 receipt carries, and one this binary has never
    // seen, alike. A receipt whose pack is in a prior format never gets
    // this far; the evidence layer refuses the pack first.
    for other in [1, 99] {
        let (passed, report) = world.edited(|r| r["semantics_version"] = json!(other));
        assert!(!passed);
        assert_eq!(
            report["evaluation"],
            json!({"status": "not_re_evaluated", "receipt_semantics": other, "binary_semantics": 2})
        );
        assert_eq!(report["program"]["status"], "matches");
    }

    // One representation only: reordered or repeated rows, a newer
    // format and an unknown field are malformed, not normalised.
    type Edit = Box<dyn Fn(&mut Value)>;
    let malformed: [(&str, Edit); 5] = [
        (
            "canonical order",
            Box::new(|r| r["answer"].as_array_mut().unwrap().reverse()),
        ),
        (
            "canonical order",
            Box::new(|r| {
                let first = r["answer"][0].clone();
                r["answer"].as_array_mut().unwrap().insert(0, first);
            }),
        ),
        (
            "newer than this binary",
            Box::new(|r| r["receipt_format_version"] = json!(2)),
        ),
        ("unknown field", Box::new(|r| r["signed"] = json!(true))),
        (
            "names no Morpholog semantics",
            Box::new(|r| r["semantics_version"] = json!(0)),
        ),
    ];
    for (detail, edit) in malformed {
        let (passed, report) = world.edited(edit);
        assert!(!passed);
        assert_eq!(report["receipt"]["status"], "malformed", "{report}");
        assert!(
            report["receipt"]["detail"]
                .as_str()
                .unwrap()
                .contains(detail),
            "{report}"
        );
        assert_eq!(report["evaluation"]["status"], "not_evaluated");
        assert_eq!(report["evidence"]["verdict"]["status"], "intact");
    }
}

/// The limit of an unsigned receipt: rewritten into another statement
/// that is also true, it verifies, because it proves what it says, not
/// what it once said.
#[tokio::test(flavor = "current_thread")]
async fn a_receipt_rewritten_into_another_true_statement_still_verifies() {
    let world = World::new().await;
    let other = issued(&borrowing_base(), &world.pack, "AssetValue");
    let (passed, report) = world.edited(|r| {
        r["query"] = other["query"].clone();
        r["answer"] = other["answer"].clone();
    });
    assert!(passed, "{report}");
}

/// The programme that evaluates a receipt need not be one that admitted
/// the rows; each row names its own.
#[tokio::test(flavor = "current_thread")]
async fn a_reporting_programme_reads_history_another_programme_admitted() {
    let world = World::new().await;
    let source = std::fs::read_to_string(borrowing_base()).unwrap();
    let reporting = write_fixture(
        "lender_reporting",
        &format!(
            "{source}
predicate Drawn(facility: Subject, drawn: Decimal)

derived Drawn(facility):
    over Facility(facility, _)
    value drawn = sum(d | Drawdown(facility, _, d))
"
        ),
    );
    let receipt = issued(&reporting, &world.pack, "Drawn");
    assert_ne!(receipt["program_hash"], world.receipt["program_hash"]);
    let pack = std::fs::read_to_string(&world.pack).unwrap();
    assert!(
        pack.contains(world.receipt["program_hash"].as_str().unwrap()),
        "the rows name the programme that admitted them"
    );
    let (passed, report) = world.check(&receipt, &reporting);
    assert!(passed, "{report}");
    assert_eq!(receipt["answer"].as_array().unwrap().len(), 2);
}

#[tokio::test(flavor = "current_thread")]
async fn only_a_complete_prefix_carries_a_receipt() {
    let world = World::new().await;
    propose("draw", &[subject("f1"), subject("d4"), decimal("1")]);
    checkpoint();
    let window = export(world.dir.path(), "window.json", &["--from-tree-size", "7"]);

    let (status, stdout, stderr) = issue(&borrowing_base(), &window, "FacilityUtilisation");
    assert!(!status.success());
    assert!(stdout.is_empty(), "{stdout}");
    assert!(
        stderr.contains("not a complete-prefix evidence pack"),
        "{stderr}"
    );

    let path = write(world.dir.path(), "receipt.json", &world.receipt.to_string());
    let (passed, report) = verify(&borrowing_base(), &path, &window, &[]);
    assert!(!passed);
    assert_eq!(report["verdict_kind"], "window");
    assert_eq!(
        report["evidence"]["verdict"]["status"], "intact",
        "{report}"
    );
    assert_eq!(
        statuses(&report),
        [
            "well_formed",
            "not_complete",
            "not_checked",
            "matches",
            "not_evaluated"
        ]
        .map(String::from)
    );

    // A broken window reports as broken, not as incomplete.
    let tampered = std::fs::read_to_string(&window)
        .unwrap()
        .replace("\"d4\"", "\"d5\"");
    let tampered = write(world.dir.path(), "window_tampered.json", &tampered);
    let (passed, report) = verify(
        &borrowing_base(),
        &path,
        &tampered,
        &["--require-signatures"],
    );
    assert!(!passed);
    assert_ne!(
        report["evidence"]["verdict"]["status"], "intact",
        "{report}"
    );
    assert_ne!(
        report["evidence"]["verdict"]["status"], "signature_required",
        "{report}"
    );
    assert_eq!(report["completeness"]["status"], "not_checked");
}

#[tokio::test(flavor = "current_thread")]
async fn no_receipt_is_issued_for_a_read_the_programme_does_not_derive() {
    let world = World::new().await;
    let (status, stdout, stderr) = issue(&borrowing_base(), &world.pack, "Facility");
    assert!(!status.success());
    assert!(stdout.is_empty(), "{stdout}");
    assert!(
        stderr.contains("declares no derived claim `Facility`"),
        "{stderr}"
    );
}

// ============================================================
// A receipt over a read of base claims, through a derived predicate
// ============================================================

fn closed_loop() -> PathBuf {
    common::repo_root().join("examples/21_closed_loop_execution/closed_loop_execution.morph")
}

/// Propose against the closed-loop example as `actor`, asserting it
/// committed, and return the claims the act admitted.
fn propose_closed_loop(transformation: &str, actor: &str, args: &[Value]) -> Value {
    let file = closed_loop();
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
    assert!(status.success(), "{transformation}: {stderr}");
    let outcome: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(outcome["status"], "committed", "{stdout}");
    outcome["asserted_claims"].clone()
}

/// The question a third party wants answered from the record alone:
/// which orders did the venue report that nobody here authorised? The
/// read is over base claims with nothing to compute; the derived
/// predicate `Unauthorised` is the route to a receipt over it. The
/// receipt states that finding over the committed history; it says
/// nothing about whether the venue's report was complete or true.
#[tokio::test(flavor = "current_thread")]
async fn a_receipt_states_a_finding_over_base_claims_through_its_derived_predicate() {
    reset_db().await;
    let login = subject(&common::session_user(&database_url()).await);
    propose_closed_loop("appoint_operator", "ops", &[subject("ops"), login.clone()]);
    for principal in ["agent", "reporter"] {
        propose_closed_loop("enrol_login", "ops", &[subject(principal), login.clone()]);
    }
    propose_closed_loop("declare_venue", "ops", &[subject("venue")]);
    propose_closed_loop(
        "grant_mandate",
        "ops",
        &[subject("agent"), subject("power_q1"), decimal("50")],
    );
    propose_closed_loop(
        "grant_feed",
        "ops",
        &[subject("reporter"), subject("venue")],
    );
    let admitted = propose_closed_loop(
        "place_order",
        "agent",
        &[
            subject("venue"),
            subject("power_q1"),
            subject("buy"),
            decimal("10"),
            decimal("48.5"),
        ],
    );
    let order = admitted
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["predicate"] == "OrderAuthorised")
        .expect("the act admits the authorisation")["args"][0]
        .clone();
    // The executor placed the order; the venue reports it, and one more
    // that nobody proposed here.
    let venue_report = |sequence: &str, order_ref: Value, venue_order_id: &str, qty: &str| {
        vec![
            subject("venue"),
            decimal(sequence),
            order_ref,
            subject(venue_order_id),
            subject("power_q1"),
            subject("buy"),
            decimal(qty),
            decimal("48.5"),
        ]
    };
    propose_closed_loop(
        "observe_venue_report",
        "reporter",
        &venue_report("1", order, "V-1001", "10"),
    );
    let admitted = propose_closed_loop(
        "observe_venue_report",
        "reporter",
        &venue_report("2", subject("manual-1"), "V-1002", "25"),
    );
    let ghost = admitted
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["predicate"] == "VenueReport")
        .expect("the act admits the report")["args"][0]
        .clone();

    // Checkpoint after the observation, so the receipt's history holds it.
    checkpoint();
    let dir = tempfile::tempdir().unwrap();
    let pack = export(dir.path(), "pack.ndjson", &[]);
    let receipt = issued(&closed_loop(), &pack, "Unauthorised");
    assert_eq!(
        receipt["query"],
        json!({"kind": "derived", "predicate": "Unauthorised"})
    );
    assert_eq!(
        receipt["answer"],
        json!([{
            "predicate": "Unauthorised",
            "args": [ghost, subject("V-1002"), decimal("25"), decimal("48.5")]
        }]),
        "{receipt}"
    );

    let path = write(dir.path(), "receipt.json", &receipt.to_string());
    let (passed, report) = verify(&closed_loop(), &path, &pack, &[]);
    assert!(passed, "{report}");
    assert_eq!(statuses(&report), ALL_HOLD.map(String::from));

    // A receipt claiming the venue reported nothing unauthorised does
    // not recompute.
    let mut denial = receipt.clone();
    denial["answer"] = json!([]);
    let path = write(dir.path(), "denial.json", &denial.to_string());
    let (passed, report) = verify(&closed_loop(), &path, &pack, &[]);
    assert!(!passed, "{report}");
    assert_eq!(report["evaluation"]["status"], "differs", "{report}");
    assert_eq!(report["evaluation"]["missing"], 1, "{report}");
}
