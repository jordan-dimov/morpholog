//! The chess README's tutorial, checked against the binary.
//!
//! Every command the test runs is formatted the way the README prints it,
//! and the README must contain that exact line; so is the fix the reader
//! types. A move, a flag or a snippet changed on one side alone fails here.
//! The README's `createdb` and `init` steps are the test database's own
//! setup, which `reset_db` stands in for.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{database_url, repo_root, reset_db};

use serde_json::Value;
use std::path::Path;
use std::process::Command;

const README: &str = "examples/07_chess_transition_invariants/README.md";
const MORPH: &str = "examples/07_chess_transition_invariants/chess.morph";

const DEFINE: &str = "\
define keeps_square_colour(piece_type, src_f, src_r, dst_f, dst_r):
  piece_type != #bishop or (src_f + src_r) % 2 = (dst_f + dst_r) % 2
";
const GATE: &str = "  require bishop_keeps_its_colour: keeps_square_colour(piece_type, src_f, src_r, dst_f, dst_r)\n";
const GATE_ANCHOR: &str = "  require piece_color = current_turn\n";
const DEFINE_ANCHOR: &str = "transformation start_game():";
const FIXED: &str = "/tmp/chess_fixed.morph";

fn readme() -> String {
    std::fs::read_to_string(repo_root().join(README)).unwrap()
}

fn printed(line: &str) {
    assert!(
        readme().lines().any(|l| l == line),
        "the README no longer shows this command, so the test and the page disagree: {line}"
    );
}

/// Run the binary on `args` against the test database: (exit ok, stdout).
fn run(args: &[&str]) -> (bool, Value) {
    let output = Command::new(common::bin())
        .args(args)
        .env("DATABASE_URL", database_url())
        .current_dir(repo_root())
        .output()
        .unwrap();
    let stdout = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
    (output.status.success(), stdout)
}

/// One `propose` exactly as the README prints it, then run.
fn propose(transformation: &str, args: &str) -> Value {
    printed(&format!(
        "morpholog propose \"$M\" {transformation} --actor you --args-named '{args}'"
    ));
    run(&[
        "propose",
        MORPH,
        transformation,
        "--actor",
        "you",
        "--args-named",
        args,
    ])
    .1
}

fn quiet(src_f: &str, src_r: &str, dst_f: &str, dst_r: &str, next: &str) -> String {
    format!(
        r#"{{"src_f": "{src_f}", "src_r": "{src_r}", "dst_f": "{dst_f}", "dst_r": "{dst_r}", "new_turn": "{next}"}}"#
    )
}

/// The squares of white's bishops, by the README's `inspect` line.
fn white_bishops() -> Vec<(String, String)> {
    printed(
        "morpholog inspect claims --named \"$M\" --predicate PieceAt --where piece_type=bishop --where color=white",
    );
    let (ok, claims) = run(&[
        "inspect",
        "claims",
        "--named",
        MORPH,
        "--predicate",
        "PieceAt",
        "--where",
        "piece_type=bishop",
        "--where",
        "color=white",
    ]);
    assert!(ok, "{claims}");
    let mut squares: Vec<(String, String)> = claims
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let at = |f: &str| c["args"][f].as_str().unwrap().to_string();
            (at("file"), at("rank"))
        })
        .collect();
    squares.sort();
    squares
}

fn square(file: &str, rank: &str) -> (String, String) {
    (file.into(), rank.into())
}

fn witness(v: &Value, var: &str) -> Value {
    v["witness"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["var"] == var)
        .map_or(Value::Null, |w| w["value"]["value"].clone())
}

/// The reader's fix: the README's `define` and gate, placed where it says.
fn write_fixed(dir: &Path) -> std::path::PathBuf {
    let text = readme();
    assert!(
        text.contains(DEFINE),
        "the README's define is not the one the test applies"
    );
    assert!(
        text.contains(GATE),
        "the README's gate is not the one the test applies"
    );
    let source = std::fs::read_to_string(repo_root().join(MORPH)).unwrap();
    assert_eq!(source.matches(DEFINE_ANCHOR).count(), 1);
    assert_eq!(
        source.matches(GATE_ANCHOR).count(),
        2,
        "the README says the gate goes below this line in both moves"
    );
    let fixed = source
        .replace(DEFINE_ANCHOR, &format!("{DEFINE}\n{DEFINE_ANCHOR}"))
        .replace(GATE_ANCHOR, &format!("{GATE_ANCHOR}{GATE}"));
    let path = dir.join("chess_fixed.morph");
    std::fs::write(&path, fixed).unwrap();
    path
}

#[tokio::test(flavor = "current_thread")]
async fn the_chess_tutorial_runs_as_written() {
    reset_db().await;

    // 2. Start a game.
    let started = propose("start_game", "{}");
    assert_eq!(started["status"], "committed", "{started}");
    assert_eq!(started["asserted_claims"].as_array().unwrap().len(), 35);

    // 3. The bishop rule refuses c1 to d3 while f1 stands, and nothing moves.
    let refused = propose("quiet_move", &quiet("3", "1", "4", "3", "black"));
    assert_eq!(refused["status"], "rejected", "{refused}");
    assert_eq!(refused["rule"], "bishops_on_opposite_square_colors");
    let mut files = [witness(&refused, "f_a"), witness(&refused, "f_b")];
    files.sort_by_key(ToString::to_string);
    assert_eq!(files, ["4", "6"], "{refused}");
    assert_eq!(white_bishops(), [square("3", "1"), square("6", "1")]);

    // 4. The hole: with one bishop left, it changes colour.
    for (transformation, args) in [
        ("quiet_move", quiet("3", "1", "5", "3", "black")),
        ("capturing_move", quiet("6", "7", "6", "1", "white")),
        ("quiet_move", quiet("5", "3", "4", "3", "black")),
    ] {
        let moved = propose(transformation, &args);
        assert_eq!(moved["status"], "committed", "{args}: {moved}");
    }
    assert_eq!(white_bishops(), [square("4", "3")]);

    // 5. The fix checks clean.
    printed(&format!("cp \"$M\" {FIXED}"));
    printed(&format!("morpholog check {FIXED}"));
    let dir = tempfile::tempdir().unwrap();
    let fixed = write_fixed(dir.path());
    let fixed = fixed.to_str().unwrap();
    let (ok, _) = run(&["check", fixed]);
    assert!(ok, "the fixed programme must check clean");

    // 6. Rehearsal: four actions, one refused by the new gate, e3 to d3.
    printed(&format!("morpholog rehearse {FIXED}"));
    for line in [
        "\"transitions_replayed\": 4,",
        "  \"same\": 3,",
        "  \"different_delta\": 0,",
        "  \"would_reject\": 1,",
    ] {
        printed(line);
    }
    let (ok, report) = run(&["rehearse", fixed]);
    assert!(ok, "{report}");
    assert_eq!(report["transitions_replayed"], 4, "{report}");
    assert_eq!(report["totals"]["same"], 3, "{report}");
    assert_eq!(report["totals"]["different_delta"], 0, "{report}");
    assert_eq!(report["totals"]["would_reject"], 1, "{report}");
    let refusals = report["refusals"].as_array().unwrap();
    assert_eq!(refusals.len(), 1, "{report}");
    assert_eq!(refusals[0]["rule"], "bishop_keeps_its_colour");
    assert_eq!(refusals[0]["would_refuse"], 1);
    let refused = &report["differences"][0];
    assert_eq!(
        [
            witness(refused, "src_f"),
            witness(refused, "src_r"),
            witness(refused, "dst_f"),
            witness(refused, "dst_r"),
        ],
        ["5", "3", "4", "3"],
        "{refused}"
    );

    // The record is unchanged: rehearse only reads.
    assert_eq!(white_bishops(), [square("4", "3")]);
}
