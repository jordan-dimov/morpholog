//! One ledger kept across programme changes. Each audit row names the
//! programme that admitted it, and the audit log replays whatever the
//! programme now is. A new rule judges only the cases a change touches,
//! and `evaluate` says beforehand which past commits it would have
//! refused. A predicate whose fields change in place leaves its old claims
//! where no pattern of the new shape matches them, and `check
//! --database-url` refuses that programme before it is deployed.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{database_url, reset_db};

use serde_json::Value;
use std::path::Path;
use std::process::Command;

const BEFORE: &str = "\
program ledger

predicate Limit(book: Subject, cap: Decimal)
predicate Position(book: Subject, qty: Decimal)

transformation set_limit(book, cap):
    admit Limit(book, cap)

transformation book_position(book, qty):
    admit Position(book, qty)
";

const NEW_RULE: &str = "\
program ledger

predicate Limit(book: Subject, cap: Decimal)
predicate Position(book: Subject, qty: Decimal)
predicate Note(book: Subject)

invariant position_within_limit:
    Position(book, qty) and Limit(book, cap) implies qty <= cap

transformation set_limit(book, cap):
    admit Limit(book, cap)

transformation book_position(book, qty):
    admit Position(book, qty)

transformation add_note(book):
    admit Note(book)
";

const RESHAPED: &str = "\
program ledger

predicate Limit(book: Subject, cap: Decimal, currency: Subject)
predicate Position(book: Subject, qty: Decimal)

transformation set_limit(book, cap, currency):
    admit Limit(book, cap, currency)

transformation book_position(book, qty):
    require has_limit: Limit(book, _, _)
    admit Position(book, qty)
";

const RETIRE: &str = "\
program ledger

predicate Limit(book: Subject, cap: Decimal)

transformation drop_limit(book, cap):
    retract Limit(book, cap)
";

const PRICED: &str = "\
program priced

predicate Price(item: Subject, amount: Decimal[USD])

transformation set_price(item, amount):
    admit Price(item, amount)
";

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

fn propose(file: &Path, transformation: &str, args: &str) -> Value {
    let (_, outcome, stderr) = run(&[
        "propose",
        file.to_str().unwrap(),
        transformation,
        "--actor",
        "desk",
        "--args-named",
        args,
    ]);
    assert!(outcome.is_object(), "{transformation} {args}: {stderr}");
    outcome
}

fn committed(file: &Path, transformation: &str, args: &str) -> String {
    let outcome = propose(file, transformation, args);
    assert_eq!(outcome["status"], "committed", "{outcome}");
    outcome["transition_id"].as_str().unwrap().to_string()
}

fn refused_by(file: &Path, transformation: &str, args: &str, rule: &str) {
    let outcome = propose(file, transformation, args);
    assert_eq!(outcome["status"], "rejected", "{outcome}");
    assert_eq!(outcome["rule"], rule, "{outcome}");
}

fn verify_passes() {
    let (ok, report, stderr) = run(&["audit", "verify"]);
    assert!(ok && report["passes"] == true, "{report} {stderr}");
}

fn hash(file: &Path) -> String {
    let output = Command::new(common::bin())
        .args(["hash", file.to_str().unwrap()])
        .output()
        .unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    report["hash"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "current_thread")]
async fn a_ledger_outlives_a_programme_change_and_a_reshape_hides_old_claims() {
    reset_db().await;
    let before = common::write_fixture("before", BEFORE);
    let new_rule = common::write_fixture("new_rule", NEW_RULE);
    let reshaped = common::write_fixture("reshaped", RESHAPED);

    committed(&before, "set_limit", r#"{"book": "A", "cap": "10"}"#);
    committed(&before, "set_limit", r#"{"book": "B", "cap": "10"}"#);
    let over = committed(&before, "book_position", r#"{"book": "A", "qty": "50"}"#);
    committed(&before, "book_position", r#"{"book": "B", "qty": "5"}"#);

    // Before deploying the new rule: which past commits would it refuse?
    let (ok, score, stderr) = run(&["evaluate", new_rule.to_str().unwrap()]);
    assert!(ok, "{stderr}");
    let rule = &score["invariants"][0];
    assert_eq!(rule["invariant"], "position_within_limit", "{score}");
    assert_eq!(
        rule["refused_transitions"],
        serde_json::json!([over]),
        "{score}"
    );

    // Deployed on the same ledger. Book A's old position breaks the new
    // rule, and that does not block a change in another case: a new
    // position within the limit, or a claim the rule does not read.
    committed(&new_rule, "book_position", r#"{"book": "A", "qty": "6"}"#);
    committed(&new_rule, "add_note", r#"{"book": "A"}"#);
    // A change that touches the breaking case is judged with it.
    refused_by(
        &new_rule,
        "set_limit",
        r#"{"book": "A", "cap": "20"}"#,
        "position_within_limit",
    );
    verify_passes();
    let mut admitted_under: Vec<String> = Vec::new();
    let tail = Command::new(common::bin())
        .args(["inspect", "audit", "--database-url", &database_url()])
        .output()
        .unwrap();
    for line in String::from_utf8(tail.stdout).unwrap().lines() {
        let row: Value = serde_json::from_str(line).unwrap();
        admitted_under.push(row["model_hash"].as_str().unwrap().to_string());
    }
    admitted_under.dedup();
    assert_eq!(admitted_under, [hash(&before), hash(&new_rule)]);

    // Limit gains a field in place. The stored two-field claims stay, the
    // log still replays, and no three-field pattern matches them: book B
    // has a limit on the ledger, and the gate refuses as if it had none.
    refused_by(
        &reshaped,
        "book_position",
        r#"{"book": "B", "qty": "1"}"#,
        "has_limit",
    );
    verify_passes();
    // A named read under the new shape says so rather than skip them.
    let (ok, _, stderr) = run(&[
        "inspect",
        "claims",
        "--named",
        reshaped.to_str().unwrap(),
        "--predicate",
        "Limit",
    ]);
    assert!(!ok && stderr.contains("arity 2"), "{stderr}");

    // Checked against the ledger first, the reshape is refused by name.
    let (ok, report, stderr) = run(&["check", reshaped.to_str().unwrap(), "--json"]);
    assert!(!ok, "the reshaped Limit must be refused: {report} {stderr}");
    let misfit = &report["diagnostics"][0];
    assert_eq!(misfit["severity"], "error", "{report}");
    assert_eq!(misfit["line"], 3, "on the declaration of Limit: {report}");
    assert!(
        misfit["message"].as_str().unwrap().contains(
            "predicate `Limit` declares 3 fields (book: Subject, cap: Decimal, \
             currency: Subject), but 2 stored claims have 2"
        ),
        "{report}"
    );
    // The programme that only added a predicate fits.
    let (ok, _, stderr) = run(&["check", "--strict", new_rule.to_str().unwrap()]);
    assert!(ok && stderr.is_empty(), "{stderr}");
    // The original fits too. It does not declare `Note`, which is a hint,
    // an error under --strict, and silent when a supplied file declares it.
    let (ok, _, stderr) = run(&["check", before.to_str().unwrap()]);
    assert!(
        ok && stderr.contains(
            "hint: stored predicate `Note` has 1 current claim but is not declared by \
             any supplied programme"
        ),
        "{stderr}"
    );
    let (ok, _, stderr) = run(&["check", "--strict", before.to_str().unwrap()]);
    assert!(
        !ok && stderr.contains("error: stored predicate `Note`"),
        "{stderr}"
    );
    let (ok, _, stderr) = run(&[
        "check",
        before.to_str().unwrap(),
        "--against",
        new_rule.to_str().unwrap(),
    ]);
    assert!(
        ok && !stderr.contains("`Note`"),
        "the --against file declares Note: {stderr}"
    );

    // Only current claims are judged: once the old limits are retracted,
    // the reshape fits, though the log still holds them.
    let retire = common::write_fixture("retire", RETIRE);
    committed(&retire, "drop_limit", r#"{"book": "A", "cap": "10"}"#);
    committed(&retire, "drop_limit", r#"{"book": "B", "cap": "10"}"#);
    let (ok, _, stderr) = run(&["check", reshaped.to_str().unwrap()]);
    assert!(ok && !stderr.contains("`Limit`"), "{stderr}");
}

/// A field that keeps its arity but changes kind or unit is refused too;
/// a field declared `Any` takes whatever is stored.
#[tokio::test(flavor = "current_thread")]
async fn check_refuses_a_field_whose_stored_kind_or_unit_differs() {
    reset_db().await;
    let priced = common::write_fixture("priced", PRICED);
    committed(&priced, "set_price", r#"{"item": "X", "amount": "5"}"#);
    committed(&priced, "set_price", r#"{"item": "Y", "amount": "7"}"#);

    let check = |amount: &str| {
        let file = common::write_fixture(
            "variant",
            &format!("program priced\npredicate Price(item: Subject, amount: {amount})\n"),
        );
        run(&["check", file.to_str().unwrap()])
    };
    for declared in ["Decimal", "Decimal[EUR]"] {
        let (ok, _, stderr) = check(declared);
        assert!(
            !ok && stderr.contains(&format!(
                "field `amount` of predicate `Price` is declared {declared}, but 2 stored \
                 claims hold Decimal[USD] there"
            )),
            "{stderr}"
        );
    }
    for declared in ["Decimal[USD]", "Any"] {
        let (ok, _, stderr) = check(declared);
        assert!(ok && stderr.is_empty(), "{declared}: {stderr}");
    }
}
