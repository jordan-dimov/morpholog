//! `rehearse` over a ledger the test builds: every committed proposal
//! re-decided under a candidate, against the prefix state before it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{database_url, reset_db};

use serde_json::Value;
use std::path::Path;
use std::process::Command;

const LEDGER: &str = "\
program ledger

predicate Limit(book: Subject, cap: Decimal)
predicate Position(book: Subject, qty: Decimal)
predicate Book(book: Subject, name: Subject)
intent Booked(book: Subject)

transformation set_limit(book, cap):
    admit Limit(book, cap)

transformation book_position(book, qty):
    admit Position(book, qty)
    emit Booked(book)

transformation open_book(name):
    let book = new Subject()
    admit Book(book, name)
";

fn with(body: &str) -> String {
    LEDGER.replace(
        "transformation set_limit(book, cap):",
        &format!("{body}\ntransformation set_limit(book, cap):"),
    )
}

fn run(args: &[&str]) -> (bool, Value, String) {
    let output = Command::new(common::bin())
        .args(args)
        .args(["--database-url", &database_url()])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let stdout = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    (output.status.success(), stdout, stderr)
}

fn committed(file: &Path, transformation: &str, args: &str) -> String {
    let (_, outcome, stderr) = run(&[
        "propose",
        file.to_str().unwrap(),
        transformation,
        "--actor",
        "desk",
        "--args-named",
        args,
    ]);
    assert_eq!(outcome["status"], "committed", "{outcome} {stderr}");
    outcome["transition_id"].as_str().unwrap().to_string()
}

fn rehearse(file: &Path) -> Value {
    let (ok, report, stderr) = run(&["rehearse", file.to_str().unwrap()]);
    assert!(ok, "{stderr}");
    report
}

fn totals(report: &Value) -> Value {
    report["totals"].clone()
}

fn classes(report: &Value) -> Vec<(String, String)> {
    report["differences"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| {
            (
                d["transition_id"].as_str().unwrap().to_string(),
                d["class"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn every_recorded_proposal_is_re_decided_under_the_candidate() {
    reset_db().await;
    let ledger = common::write_fixture("ledger", LEDGER);
    let t1 = committed(&ledger, "set_limit", r#"{"book": "A", "cap": "10"}"#);
    let t2 = committed(&ledger, "set_limit", r#"{"book": "B", "cap": "10"}"#);
    let over = committed(&ledger, "book_position", r#"{"book": "A", "qty": "50"}"#);
    let under = committed(&ledger, "book_position", r#"{"book": "B", "qty": "5"}"#);
    let opened = committed(&ledger, "open_book", r#"{"name": "alpha"}"#);

    // Identity: the programme that admitted every row, including the
    // one that drew a fresh subject, decides each the same way.
    let report = rehearse(&ledger);
    assert_eq!(report["contract"], "canonical_audit_prefix_v1");
    assert_eq!(report["transitions_replayed"], 5);
    assert_eq!(
        totals(&report),
        serde_json::json!({"same": 5, "different_delta": 0, "would_reject": 0, "would_error": 0, "unreplayable": 0}),
        "{report}"
    );
    assert_eq!(report["differences"], serde_json::json!([]));

    // An invariant refuses the over-limit position and nothing else.
    let invariant = common::write_fixture(
        "invariant",
        &with(
            "invariant position_within_limit:\n    Position(book, qty) and Limit(book, cap) implies qty <= cap\n",
        ),
    );
    let report = rehearse(&invariant);
    assert_eq!(
        classes(&report),
        vec![(over.clone(), "would_reject".into())]
    );
    let refused = &report["differences"][0];
    assert_eq!(refused["rule"], "position_within_limit");
    assert_eq!(refused["kind"], "invariant");
    assert_eq!(refused["compared"]["op"], "<=", "{refused}");
    assert_eq!(refused["semantics_version"], report["semantics_version"]);
    assert_eq!(
        report["refusals"],
        serde_json::json!([{"rule": "position_within_limit", "kind": "invariant", "would_refuse": 1, "refused_transitions": [over]}])
    );

    // The pack path agrees with the database path.
    let pack = common::write_fixture("pack.ndjson", "");
    let (ok, _, stderr) = run(&["audit", "checkpoint"]);
    assert!(ok, "{stderr}");
    let exported = Command::new(common::bin())
        .args(["audit", "export", "--database-url", &database_url()])
        .output()
        .unwrap();
    std::fs::write(&*pack, &exported.stdout).unwrap();
    let offline = Command::new(common::bin())
        .args([
            "rehearse",
            invariant.to_str().unwrap(),
            "--pack",
            pack.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    let offline: Value = serde_json::from_slice(&offline.stdout).unwrap();
    assert_eq!(offline, report);

    // A gate refuses it too, under the gate's name.
    let gate = common::write_fixture(
        "gate",
        &LEDGER.replace(
            "transformation book_position(book, qty):\n",
            "transformation book_position(book, qty):\n    require within_cap: Limit(book, cap) and qty <= cap\n",
        ),
    );
    let report = rehearse(&gate);
    assert_eq!(
        classes(&report),
        vec![(over.clone(), "would_reject".into())]
    );
    assert_eq!(report["differences"][0]["rule"], "within_cap");
    assert_eq!(report["differences"][0]["kind"], "require");

    // Two broken rules count once, under the first that refused.
    let two = common::write_fixture(
        "two",
        &with(
            "invariant position_within_limit:\n    Position(book, qty) and Limit(book, cap) implies qty <= cap\n\ninvariant position_small:\n    Position(book, qty) implies qty <= 20\n",
        ),
    );
    let report = rehearse(&two);
    assert_eq!(totals(&report)["would_reject"], 1);
    assert_eq!(report["refusals"].as_array().unwrap().len(), 1, "{report}");
    assert_eq!(report["refusals"][0]["rule"], "position_within_limit");

    // Same claims, a different intent: a different effect.
    let intent = common::write_fixture(
        "intent",
        &LEDGER
            .replace(
                "intent Booked(book: Subject)",
                "intent Booked(book: Subject)\nintent Queued(book: Subject)",
            )
            .replace("    emit Booked(book)", "    emit Queued(book)"),
    );
    let report = rehearse(&intent);
    assert_eq!(
        classes(&report),
        vec![
            (over.clone(), "different_delta".into()),
            (under.clone(), "different_delta".into())
        ]
    );
    let diff = &report["differences"][0];
    assert_eq!(
        diff["recorded"]["asserted"], diff["candidate"]["asserted"],
        "{diff}"
    );
    assert_eq!(diff["recorded"]["emitted"][0]["name"], "Booked", "{diff}");
    assert_eq!(diff["candidate"]["emitted"][0]["name"], "Queued", "{diff}");

    // An act the candidate lacks, and one whose parameters do not take
    // the recorded arguments.
    let renamed = common::write_fixture(
        "renamed",
        &LEDGER.replace(
            "transformation book_position(book, qty):",
            "transformation book(book, qty):",
        ),
    );
    let report = rehearse(&renamed);
    assert_eq!(
        classes(&report),
        vec![
            (over.clone(), "unreplayable".into()),
            (under.clone(), "unreplayable".into())
        ]
    );
    assert!(
        report["differences"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("no transformation `book_position`"),
        "{report}"
    );
    let arity = common::write_fixture(
        "arity",
        &LEDGER
            .replace("predicate Limit(book: Subject, cap: Decimal)", "predicate Limit(book: Subject, cap: Decimal, currency: Subject)")
            .replace("transformation set_limit(book, cap):\n    admit Limit(book, cap)", "transformation set_limit(book, cap, currency):\n    admit Limit(book, cap, currency)"),
    );
    let report = rehearse(&arity);
    assert_eq!(
        classes(&report),
        vec![
            (t1.clone(), "unreplayable".into()),
            (t2.clone(), "unreplayable".into())
        ]
    );
    assert!(
        report["differences"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("takes 3 arguments; the row records 2"),
        "{report}"
    );

    // Arguments accepted, the kernel errors in execution.
    let erring = common::write_fixture(
        "erring",
        &LEDGER.replace(
            "    admit Position(book, qty)\n",
            "    let nothing = (qty - qty)\n    let r = (qty / nothing)\n    require r <= r\n    admit Position(book, qty)\n",
        ),
    );
    let report = rehearse(&erring);
    assert_eq!(
        classes(&report),
        vec![
            (over.clone(), "would_error".into()),
            (under.clone(), "would_error".into())
        ]
    );
    assert!(
        report["differences"][0]["error"]
            .as_str()
            .unwrap()
            .contains("division by zero"),
        "{report}"
    );

    // Draws beyond the record are unreplayable, never minted.
    let drawing = common::write_fixture(
        "drawing",
        &LEDGER.replace(
            "    let book = new Subject()\n    admit Book(book, name)\n",
            "    let book = new Subject()\n    let twin = new Subject()\n    admit Book(book, name)\n    admit Book(twin, name)\n",
        ),
    );
    let report = rehearse(&drawing);
    assert_eq!(
        classes(&report),
        vec![(opened.clone(), "unreplayable".into())]
    );
    assert!(
        report["differences"][0]["reason"]
            .as_str()
            .unwrap()
            .contains("more fresh subjects than the row records"),
        "{report}"
    );
}
