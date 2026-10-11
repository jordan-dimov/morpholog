//! A claims read names the transition whose commit made each claim
//! present. Read now, that is the claims table's own column; read as of a
//! transition, it comes from replaying the audit history. The two agree
//! on everything the runtime wrote, and only the replay is checkable: the
//! tamper case models an operator with write access to the claims table,
//! who can change the column without touching the audit log.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{database_url, reset_db};

use serde_json::Value;
use std::path::Path;
use std::process::Command;

const IMPORTS: &str = "\
program imports

predicate Imported(batch: Subject)

transformation import(batch):
    admit Imported(batch)

transformation withdraw(batch):
    retract Imported(batch)

transformation reimport(batch):
    retract Imported(batch)
    admit Imported(batch)
";

fn cli(args: &[&str]) -> Value {
    let output = Command::new(common::bin())
        .args(args)
        .args(["--database-url", &database_url()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn propose(file: &Path, transformation: &str) -> String {
    let path = file.to_str().unwrap();
    let outcome = cli(&[
        "propose",
        path,
        transformation,
        "--actor",
        "clerk",
        "--args-named",
        r#"{"batch": "b1"}"#,
    ]);
    assert_eq!(outcome["status"], "committed", "{outcome}");
    outcome["transition_id"].as_str().unwrap().to_string()
}

/// The claims read as `(predicate, args, asserted_in)`, sorted, so a
/// current read and a replayed one compare by identity, not position.
fn claims(as_of: Option<&str>) -> Vec<(String, String, String)> {
    let mut args = vec!["inspect", "claims"];
    if let Some(at) = as_of {
        args.extend(["--as-of", at]);
    }
    let mut rows: Vec<(String, String, String)> = cli(&args)
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["predicate"].as_str().unwrap().to_string(),
                c["args"].to_string(),
                c["asserted_in"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    rows.sort();
    rows
}

fn asserted_in(as_of: Option<&str>) -> Option<String> {
    claims(as_of).into_iter().next().map(|(_, _, t)| t)
}

#[tokio::test(flavor = "current_thread")]
async fn a_claims_read_names_the_transition_that_made_each_claim_present() {
    reset_db().await;
    let fixture = common::write_fixture("claim_provenance", IMPORTS);
    let file = fixture.path.as_path();
    let mut steps = Vec::new();
    let mut step = |transformation: &str| {
        let transition = propose(file, transformation);
        steps.push((transition.clone(), claims(None)));
        transition
    };

    let first = step("import");
    assert_eq!(asserted_in(None).as_deref(), Some(first.as_str()));

    // Admitting it again is recorded in the audit row, but the claim was
    // already present, so it still began with the first transition.
    let again = step("import");
    let tail = Command::new(common::bin())
        .args(["inspect", "audit", "--database-url", &database_url()])
        .output()
        .unwrap();
    let again_row: Value = String::from_utf8(tail.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .find(|row| row["transition_id"] == again.as_str())
        .unwrap_or_else(|| panic!("no audit row for {again}"));
    assert_eq!(
        again_row["asserted_claims"].as_array().unwrap().len(),
        1,
        "{again_row}"
    );
    assert_eq!(asserted_in(None).as_deref(), Some(first.as_str()));

    step("withdraw");
    assert_eq!(asserted_in(None), None);

    let back = step("import");
    assert_eq!(asserted_in(None).as_deref(), Some(back.as_str()));

    // Retracted and admitted in one act: the act ended one presence and
    // began another.
    let both = step("reimport");
    assert_eq!(asserted_in(None).as_deref(), Some(both.as_str()));

    // The named read carries the same transition.
    let named = cli(&["inspect", "claims", "--named", file.to_str().unwrap()]);
    assert_eq!(named[0]["asserted_in"], both.as_str(), "{named}");

    // Replaying the audit history to each step gives what the table said
    // just after it.
    for (transition, current) in &steps {
        assert_eq!(&claims(Some(transition)), current, "as of {transition}");
    }

    let pool = morpholog_postgres::PgPool::connect(&database_url())
        .await
        .unwrap();
    sqlx::query("UPDATE morpholog.claims SET asserted_in = $1::uuid")
        .bind(&first)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(asserted_in(None).as_deref(), Some(first.as_str()));
    assert_eq!(asserted_in(Some(&both)).as_deref(), Some(both.as_str()));
    let verify = cli(&["audit", "verify"]);
    assert_eq!(
        verify["replay"]["status"], "consistent",
        "verify compares claims, not the column: {verify}"
    );
}
