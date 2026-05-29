//! End-to-end tests for the `morpholog schema` subcommand.
//! Spawns the built binary against the trade_lifecycle example,
//! asserts on the stdout JSON Schema shape, stderr, and exit code.
//!
//! Distinct from the `transformation_arg_schema` unit tests in
//! morpholog-core (which pin per-kind property shapes against
//! hand-built programmes): this test catches regressions in the
//! CLI wiring - file reading, validation, schema generation, JSON
//! emission, exit codes.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::path::PathBuf;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_morpholog")
}

fn repo_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn write_fixture(name: &str, content: &str) -> tempfile::TempPath {
    let mut f = tempfile::Builder::new()
        .prefix(name)
        .suffix(".morph")
        .tempfile()
        .expect("create tempfile");
    f.write_all(content.as_bytes()).expect("write fixture");
    f.into_temp_path()
}

#[test]
fn schema_emits_json_schema_for_known_transformation() {
    let morph = repo_root().join("examples/10_trade_lifecycle/trade_lifecycle.morph");
    let out = Command::new(bin())
        .args(["schema", morph.to_str().unwrap(), "capture_trade"])
        .output()
        .expect("run morpholog schema");
    assert!(
        out.status.success(),
        "expected exit 0; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let schema: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is JSON");
    assert_eq!(
        schema["$schema"], "https://json-schema.org/draft/2020-12/schema",
        "the embedder must be able to detect this as a Draft 2020-12 schema",
    );
    assert_eq!(schema["title"], "capture_trade");
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    let required = schema["required"].as_array().expect("required is array");
    assert!(
        required.iter().any(|v| v == "trade"),
        "capture_trade must require `trade` as a parameter",
    );
    // Sanity: the property shapes are pinned in the morpholog-core
    // unit tests; here we just confirm the CLI delivered them.
    assert!(
        schema["properties"]["trade"]["type"] == "string",
        "trade is a Subject (uuid string)",
    );
}

#[test]
fn schema_intent_emits_json_schema_for_known_intent() {
    let morph = repo_root().join("examples/10_trade_lifecycle/trade_lifecycle.morph");
    let out = Command::new(bin())
        .args([
            "schema",
            morph.to_str().unwrap(),
            "--intent",
            "TradeSettlementRequested",
        ])
        .output()
        .expect("run morpholog schema --intent");
    assert!(
        out.status.success(),
        "expected exit 0; stderr:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8(out.stdout).expect("utf8 stdout");
    let schema: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is JSON");
    assert_eq!(schema["title"], "TradeSettlementRequested");
    assert_eq!(schema["type"], "object");
    // `required[]` is the positional order a deliverer decodes the
    // emitted payload by - the load-bearing contract for the embedder.
    let required = schema["required"].as_array().expect("required is array");
    assert_eq!(
        required,
        &["settlement_id", "trade", "settled_qty"],
        "intent payload field order must match the declaration",
    );
    assert_eq!(schema["properties"]["settled_qty"]["type"], "string");
}

#[test]
fn schema_unknown_intent_errors_to_stderr_and_exits_nonzero() {
    let morph = repo_root().join("examples/10_trade_lifecycle/trade_lifecycle.morph");
    let out = Command::new(bin())
        .args(["schema", morph.to_str().unwrap(), "--intent", "GhostIntent"])
        .output()
        .expect("run morpholog schema --intent");
    assert!(
        !out.status.success(),
        "expected non-zero exit on unknown intent"
    );
    assert!(out.stdout.is_empty(), "no stdout on error");
    let stderr = String::from_utf8(out.stderr).expect("utf8 stderr");
    assert!(
        stderr.contains("unknown intent") && stderr.contains("GhostIntent"),
        "stderr should name the missing intent; got:\n{stderr}",
    );
}

#[test]
fn schema_unknown_transformation_errors_to_stderr_and_exits_nonzero() {
    let morph = repo_root().join("examples/10_trade_lifecycle/trade_lifecycle.morph");
    let out = Command::new(bin())
        .args(["schema", morph.to_str().unwrap(), "ghost_transformation"])
        .output()
        .expect("run morpholog schema");
    assert!(
        !out.status.success(),
        "expected non-zero exit on unknown transformation",
    );
    assert!(
        out.stdout.is_empty(),
        "schema output stream stays empty on error; got:\n{}",
        String::from_utf8_lossy(&out.stdout),
    );
    let stderr = String::from_utf8(out.stderr).expect("utf8 stderr");
    assert!(
        stderr.contains("unknown transformation") && stderr.contains("ghost_transformation"),
        "stderr should name the missing transformation; got:\n{stderr}",
    );
}

#[test]
fn schema_parse_error_renders_diagnostic_and_exits_nonzero() {
    // Parser-level rejection: `Money` is not a declared
    // `PredicateArgKind`, so the parser refuses the predicate
    // declaration. Triggers the same `parse_or_exit` path every
    // other subcommand uses, so we confirm the schema subcommand
    // wires it in.
    let path = write_fixture("bad", "program demo\npredicate Foo(amount: Money)\n");
    let out = Command::new(bin())
        .args(["schema", path.to_str().unwrap(), "anything"])
        .output()
        .expect("run morpholog schema");
    assert!(
        !out.status.success(),
        "expected non-zero exit on parse failure",
    );
    assert!(
        out.stdout.is_empty(),
        "no stdout on parse failure; got:\n{}",
        String::from_utf8_lossy(&out.stdout),
    );
    assert!(
        !out.stderr.is_empty(),
        "stderr should carry the parse diagnostic",
    );
}
