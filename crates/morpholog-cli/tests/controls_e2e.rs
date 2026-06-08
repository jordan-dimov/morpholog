//! End-to-end test of `morpholog inspect controls` against a real
//! `.morph` fixture: the static control-matrix view (prose and JSON),
//! spawned through the built binary. No database - controls is a pure
//! read over the parsed programme.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_morpholog")
}

fn write_fixture(content: &str) -> tempfile::TempPath {
    let mut f = tempfile::Builder::new()
        .suffix(".morph")
        .tempfile()
        .expect("create tempfile");
    f.write_all(content.as_bytes()).expect("write fixture");
    f.into_temp_path()
}

const TWO_PERSON: &str = "\
program two_person
predicate Match(m: Subject)
predicate Verified(m: Subject, who: Subject)
predicate Decision(d: Subject, m: Subject)
invariant decision_needs_two_distinct:
    Decision(d, m) implies (Verified(m, a) and Verified(m, b) and a != b)
transformation verify(m, who):
    require Match(m)
    admit Verified(m, who)
transformation decide(d, m):
    bind Match(m)
    require (Verified(m, a) and Verified(m, b) and a != b)
    admit Decision(d, m)
";

#[test]
fn controls_prose_shows_gates_and_guarantees() {
    let path = write_fixture(TWO_PERSON);
    let out = Command::new(bin())
        .args(["inspect", "controls", path.to_str().unwrap()])
        .output()
        .expect("run inspect controls");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    for fragment in [
        "Controls for `two_person`",
        "decide may commit only when:",
        "exactly one claim matches Match(m)",
        "a != b",
        "Always (invariants):",
        "decision_needs_two_distinct",
    ] {
        assert!(stdout.contains(fragment), "`{fragment}` not in:\n{stdout}");
    }
}

#[test]
fn controls_json_carries_the_structured_matrix() {
    let path = write_fixture(TWO_PERSON);
    let out = Command::new(bin())
        .args(["inspect", "controls", path.to_str().unwrap(), "--json"])
        .output()
        .expect("run inspect controls --json");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    assert_eq!(v["program"], "two_person");
    let decide = v["transformations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["transformation"] == "decide")
        .expect("decide present");
    let gates = decide["gates"].as_array().unwrap();
    assert_eq!(gates[0]["form"], "bind");
    assert_eq!(gates[0]["consults"][0], "Match");
    assert!(gates.iter().any(|g| g["form"] == "require"));
    assert!(!v["guarantees"].as_array().unwrap().is_empty());
}
