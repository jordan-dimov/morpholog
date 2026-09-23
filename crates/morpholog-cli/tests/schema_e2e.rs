//! End-to-end tests for `morpholog schema` through the built binary: the
//! JSON Schema on stdout, stderr, and exit codes. Per-kind property shapes
//! are pinned in morpholog-core; this covers the CLI wiring.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{bin, repo_root};

use std::io::Write;
use std::process::Command;

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
    // Shapes are pinned in morpholog-core; just confirm they arrived.
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
    // A deliverer decodes the payload by `x-morpholog-arg-order`, not by
    // the incidental order of `required`.
    assert_eq!(
        schema["x-morpholog-arg-order"],
        serde_json::json!(["settlement_id", "trade", "settled_qty"]),
        "intent payload positional order must match the declaration",
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
    // `Money` is not a known kind, so the parser refuses the declaration.
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

// ============================================================
// `morpholog hash` and `morpholog schema --all`
// ============================================================

#[test]
fn hash_is_formatting_insensitive_and_rule_sensitive() {
    // Same rules with different formatting and comments: same hash.
    // A changed rule: different hash.
    let original = write_fixture(
        "hash_a",
        "program demo\n\
         -- a teaching comment\n\
         predicate Foo(x: Subject, n: Decimal)\n\
         invariant cap: Foo(x, n) implies n <= 100\n",
    );
    let reformatted = write_fixture(
        "hash_b",
        "program demo\n\
         predicate Foo(x:    Subject,   n: Decimal)\n\
         -- entirely different commentary\n\
         invariant cap:\n    Foo(x, n) implies n <= 100\n",
    );
    let rule_changed = write_fixture(
        "hash_c",
        "program demo\n\
         predicate Foo(x: Subject, n: Decimal)\n\
         invariant cap: Foo(x, n) implies n <= 99\n",
    );

    let hash_of = |path: &std::path::Path| -> String {
        let out = Command::new(bin())
            .args(["hash", path.to_str().unwrap()])
            .output()
            .expect("run morpholog hash");
        assert!(
            out.status.success(),
            "hash should succeed; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: serde_json::Value =
            serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
        assert_eq!(v["program"], "demo");
        let h = v["hash"].as_str().unwrap().to_string();
        assert!(h.starts_with("sha256:"), "self-describing prefix: {h}");
        h
    };

    let a = hash_of(&original);
    let b = hash_of(&reformatted);
    let c = hash_of(&rule_changed);
    assert_eq!(a, b, "formatting and comments must not change the hash");
    assert_ne!(a, c, "a rule change must change the hash");
}

#[test]
fn schema_all_emits_one_manifest_covering_the_whole_programme() {
    let path = write_fixture(
        "manifest",
        "program manifesto\n\
         predicate Balance(acct: Subject, amount: Decimal)\n\
         intent Posted(acct: Subject)\n\
         invariant non_negative: Balance(a, x) implies 0 <= x\n\
         transformation wipe(acct):\n    \
             retract Balance(acct, _)\n\
         transformation post(acct, amount):\n    \
             admit Balance(acct, amount)\n    \
             emit Posted(acct)\n",
    );
    let out = Command::new(bin())
        .args(["schema", path.to_str().unwrap(), "--all"])
        .output()
        .expect("run morpholog schema --all");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let m: serde_json::Value =
        serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();

    assert_eq!(m["program"], "manifesto");
    assert!(m["hash"].as_str().unwrap().starts_with("sha256:"));
    // Every transformation and intent present, schemas intact.
    assert!(m["transformations"]["post"]["properties"]["amount"].is_object());
    assert!(m["transformations"]["wipe"]["properties"]["acct"].is_object());
    assert!(m["intents"]["Posted"]["properties"]["acct"].is_object());
    // Declaration order comes from the explicit arrays. The fixture
    // declares `wipe` before `post` so it differs from alphabetical order.
    assert_eq!(
        m["transformation_order"],
        serde_json::json!(["wipe", "post"]),
        "declaration order, not alphabetical"
    );
    assert_eq!(m["intent_order"], serde_json::json!(["Posted"]));
    // The predicate vocabulary rides along for claim decoding.
    assert_eq!(m["predicates"][0]["name"], "Balance");
    assert_eq!(m["predicates"][0]["args"][0]["name"], "acct");

    // The embedded hash is the same hash `morpholog hash` reports.
    let h = Command::new(bin())
        .args(["hash", path.to_str().unwrap()])
        .output()
        .unwrap();
    let hv: serde_json::Value =
        serde_json::from_str(&String::from_utf8(h.stdout).unwrap()).unwrap();
    assert_eq!(m["hash"], hv["hash"], "one hash, two surfaces");
}

#[test]
fn schema_all_conflicts_with_the_single_shot_forms() {
    let path = write_fixture("conflicted", "program p\npredicate A(x: Subject)\n");
    let out = Command::new(bin())
        .args([
            "schema",
            path.to_str().unwrap(),
            "some_transformation",
            "--all",
        ])
        .output()
        .expect("run morpholog schema");
    assert!(
        !out.status.success(),
        "--all plus a positional transformation must conflict"
    );
    // The mirror: --intent plus --all is the same conflict.
    let out = Command::new(bin())
        .args([
            "schema",
            path.to_str().unwrap(),
            "--intent",
            "Posted",
            "--all",
        ])
        .output()
        .expect("run morpholog schema");
    assert!(!out.status.success(), "--all plus --intent must conflict");
}

// ============================================================
// `schema --result`: the outcome-envelope contract, no .morph needed.
// ============================================================

#[test]
fn schema_result_emits_the_envelope_contract() {
    let out = Command::new(bin())
        .args(["schema", "--result"])
        .output()
        .expect("run morpholog schema --result");
    assert!(out.status.success(), "schema --result should exit zero");
    let doc: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout is one JSON document");
    let defs = doc["$defs"].as_object().expect("$defs object");
    // The entries an embedder branches on; result_schema_contract pins
    // the full set.
    for key in [
        "run_outcome",
        "explanation",
        "batch_receipt",
        "outbox_claim",
        "check_report",
        "tagged_value",
    ] {
        assert!(defs.contains_key(key), "$defs lacks `{key}`: {doc}");
    }
}

#[test]
fn schema_result_conflicts_with_per_programme_modes() {
    let path = write_fixture("result_conflict", "program p\npredicate A(x: Subject)\n");
    let out = Command::new(bin())
        .args(["schema", "--result", path.to_str().unwrap(), "--all"])
        .output()
        .expect("run morpholog schema");
    assert!(!out.status.success(), "--result plus --all must conflict");
}
