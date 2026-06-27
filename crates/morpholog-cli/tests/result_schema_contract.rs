//! The outcome-envelope contract, pinned from both sides.
//!
//! `schema --result` embeds a hand-pinned JSON Schema document; this
//! test is what keeps it honest, in two layers over one set of golden
//! envelope files (`tests/golden/envelopes/*.json`):
//!
//! 1. **Reality**: each golden is byte-equal to a freshly serialized
//!    real value (`PgProposalOutcome`, `explain()` output, an
//!    `OutboxRow`) or to a composite built exactly the way the command
//!    code builds it (`run.rs`'s traced/batch `json!` shapes,
//!    `outbox.rs`'s wrappers). A serde change anywhere shows up here.
//! 2. **Pin**: each golden validates against its `$defs` entry in the
//!    embedded `result.json` - discriminants, required keys, key-set
//!    strictness - via a small walker (no schema-validation
//!    dependency; format/pattern are deliberately not checked here,
//!    the reality layer pins exact bytes).
//!
//! The same goldens are loaded by the generated Python client's
//! `test_envelopes.py`, so the binary, the schema document, and the
//! emitted client all answer to one sample set.
//!
//! To regenerate after a deliberate contract change:
//! `UPDATE_GOLDENS=1 cargo test -p morpholog-cli --test result_schema_contract`

#![allow(clippy::unwrap_used, clippy::expect_used)]

use chrono::TimeZone;
use morpholog_core::ir_builder::{
    assert_, claim, implies, invariant, not, params, predicate, program, require, transformation,
    var,
};
use morpholog_core::{
    ClaimInstance, CoverageTracker, EvalValue, IntentInstance, State, Subject, Transition, explain,
};
use morpholog_postgres::{
    AuditRow, AuditedInvariantCheck, Checkpoint, CheckpointOutcome, EvidencePack, OutboxRow,
    PackManifest, PgProposalOutcome, TreeHeadSignature, TreeVerification, VerifyOutcome,
    VerifyReport,
};
use rust_decimal::Decimal;
use std::path::PathBuf;
use std::str::FromStr;

fn golden_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/envelopes")
}

/// Assert `value` serializes byte-equal to the named golden - or, under
/// `UPDATE_GOLDENS=1`, write the golden instead.
fn assert_golden(name: &str, value: &serde_json::Value) {
    let path = golden_dir().join(name);
    let rendered = format!("{}\n", serde_json::to_string_pretty(value).unwrap());
    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        std::fs::create_dir_all(golden_dir()).unwrap();
        std::fs::write(&path, &rendered).unwrap();
        return;
    }
    let on_disk = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing golden {name} ({e}); run with UPDATE_GOLDENS=1"));
    assert_eq!(
        on_disk, rendered,
        "{name} drifted from the binary's real serialization; if the \
         contract change is deliberate, regenerate with UPDATE_GOLDENS=1 \
         and update result.json to match"
    );
}

fn to_value<T: serde::Serialize>(v: &T) -> serde_json::Value {
    serde_json::to_value(v).unwrap()
}

// ============================================================
// Sample values - fixed inputs, deterministic output.
// ============================================================

fn sample_uuid() -> uuid::Uuid {
    uuid::Uuid::from_str("01900000-0000-7000-8000-000000000001").unwrap()
}

/// One claim whose args exercise every tagged-value variant.
fn kitchen_sink_claim() -> ClaimInstance {
    ClaimInstance {
        predicate: "EveryKind".into(),
        args: vec![
            EvalValue::Subject(Subject::from("acct_1")),
            EvalValue::Decimal(Decimal::from_str("100.50").unwrap()),
            EvalValue::Bool(true),
            EvalValue::Date(jiff::civil::date(2026, 6, 1)),
            EvalValue::Timestamp(jiff::Timestamp::from_str("2026-06-01T12:00:00Z").unwrap()),
            EvalValue::Duration(jiff::SignedDuration::from_str("PT6H").unwrap()),
            EvalValue::Quantity {
                amount: Decimal::from_str("25000").unwrap(),
                unit: "USD".into(),
            },
            EvalValue::Collection(vec![EvalValue::Subject(Subject::from("nested"))]),
        ],
    }
}

fn committed_outcome() -> PgProposalOutcome {
    PgProposalOutcome::Committed {
        transition_id: sample_uuid(),
        actor: Subject::from("alex"),
        asserted_claims: vec![kitchen_sink_claim()],
        retracted_claims: vec![ClaimInstance {
            predicate: "Flag".into(),
            args: vec![EvalValue::Subject(Subject::from("acct_1"))],
        }],
        emitted_intents: vec![IntentInstance {
            name: "AccountOpened".into(),
            args: vec![EvalValue::Subject(Subject::from("acct_1"))],
        }],
    }
}

fn rejected_outcome() -> PgProposalOutcome {
    PgProposalOutcome::Rejected {
        reason: "invariant `no_flagged_accounts` violated".to_string(),
    }
}

/// A tiny programme exercising every explanation verdict: a gate that
/// can be missing its claim, an invariant the gated path then violates,
/// and a transformation that is cleanly admissible.
fn explanation_program() -> morpholog_core::Program {
    let p = program("envelopes")
        .predicates(vec![
            predicate("Account").subject("account_id").build(),
            predicate("Flag").subject("account_id").build(),
        ])
        .invariants(vec![invariant(
            "no_flagged_accounts",
            implies(
                claim("Account", vec![var("a")]),
                not(claim("Flag", vec![var("a")])),
            ),
        )])
        .transformations(vec![
            transformation(
                "flag_account",
                params(&["account_id"]),
                vec![assert_("Flag", vec![var("account_id")])],
            ),
            transformation(
                "open_account",
                params(&["account_id"]),
                vec![
                    require(claim("Flag", vec![var("account_id")])),
                    assert_("Account", vec![var("account_id")]),
                ],
            ),
        ])
        .build();
    p.validate().expect("the sample programme validates");
    p
}

fn transition(name: &str) -> Transition {
    Transition {
        transformation_name: name.into(),
        args: vec![EvalValue::Subject(Subject::from("acct_1"))],
        actor: Subject::from("alex"),
    }
}

fn flagged_state() -> State {
    State::from_claims(vec![ClaimInstance {
        predicate: "Flag".into(),
        args: vec![EvalValue::Subject(Subject::from("acct_1"))],
    }])
}

// ============================================================
// Reality layer: goldens are the binary's real serialization.
// ============================================================

#[test]
fn run_outcomes_serialize_as_pinned() {
    assert_golden("committed.json", &to_value(&committed_outcome()));
    assert_golden("rejected.json", &to_value(&rejected_outcome()));
}

#[test]
fn explanations_serialize_as_pinned() {
    let p = explanation_program();
    let admissible = explain(&p, &transition("flag_account"), &State::from_claims(vec![]));
    let gate = explain(&p, &transition("open_account"), &State::from_claims(vec![]));
    let invariant_violated = explain(&p, &transition("open_account"), &flagged_state());
    let error = explain(&p, &transition("no_such_transformation"), &flagged_state());
    assert_golden("explanation_admissible.json", &to_value(&admissible));
    assert_golden("explanation_gate.json", &to_value(&gate));
    assert_golden("explanation_invariant.json", &to_value(&invariant_violated));
    assert_golden("explanation_error.json", &to_value(&error));
}

#[test]
fn outbox_row_serializes_as_pinned() {
    let row = OutboxRow {
        intent_id: sample_uuid(),
        transition_id: sample_uuid(),
        intent_type: "AccountOpened".to_string(),
        arguments: vec![EvalValue::Subject(Subject::from("acct_1"))],
        idempotency_key: "k1".to_string(),
        status: "pending".to_string(),
        attempt_count: 0,
        enqueued_at: chrono::Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap(),
        last_attempt_at: None,
        delivered_at: None,
        failed_at: None,
        failure_reason: None,
        next_attempt_at: None,
        compensation_transition_id: None,
        locked_by: Some("worker-1".to_string()),
        lock_expires_at: Some(chrono::Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 30).unwrap()),
    };
    assert_golden("outbox_row.json", &to_value(&row));
    // The claim/update wrappers, built exactly as commands/outbox.rs
    // builds them.
    assert_golden("outbox_claim.json", &serde_json::json!({ "row": row }));
    assert_golden(
        "outbox_claim_null.json",
        &serde_json::json!({ "row": null }),
    );
    assert_golden(
        "outbox_update_applied.json",
        &serde_json::json!({ "status": "applied" }),
    );
    assert_golden(
        "outbox_update_lease_lost.json",
        &serde_json::json!({ "status": "lease_lost" }),
    );
}

// The audit tail's row, byte-equal to the AuditRow the adapter
// serialises - the shape every `inspect audit` line carries.
#[test]
fn audit_rows_serialize_as_pinned() {
    let row = AuditRow {
        transition_id: sample_uuid(),
        transformation_name: "open_account".into(),
        arguments: vec![EvalValue::Subject(Subject::from("acct_1"))],
        actor: Subject::from("alex"),
        invariant_epoch: 1,
        invariants_checked: vec![AuditedInvariantCheck {
            name: "account_unique_by_account_id".into(),
            version: 1,
        }],
        asserted_claims: vec![kitchen_sink_claim()],
        retracted_claims: vec![],
        emitted_intents: vec![IntentInstance {
            name: "AccountOpened".into(),
            args: vec![EvalValue::Subject(Subject::from("acct_1"))],
        }],
        committed_at: chrono::Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap(),
    };
    assert_golden("audit_row.json", &to_value(&row));

    // The --named form replaces the two claim arrays with
    // field-keyed bare objects (the named_claim shape) and leaves
    // everything else byte-identical. The live binary's construction
    // is pinned by cli_integration; this golden pins the bytes the
    // Python tests consume.
    let mut named = to_value(&row);
    let obj = named.as_object_mut().unwrap();
    obj.insert(
        "asserted_claims".to_string(),
        serde_json::json!([{
            "predicate": "Account",
            "args": { "account_id": "acct_1", "balance": "100.50" }
        }]),
    );
    obj.insert("retracted_claims".to_string(), serde_json::json!([]));
    assert_golden("audit_row_named.json", &named);
}

// Composite envelopes, built exactly the way commands/run.rs builds
// them (the cli_integration suite pins the live binary's output; this
// pins the bytes the Python tests consume).
#[test]
fn composite_envelopes_serialize_as_pinned() {
    let p = explanation_program();
    let explanation = explain(&p, &transition("open_account"), &flagged_state());
    assert_golden(
        "rejected_with_explanation.json",
        &serde_json::json!({
            "status": "rejected",
            "reason": "invariant `no_flagged_accounts` violated",
            "explanation": explanation,
        }),
    );
    assert_golden(
        "traced_committed.json",
        &serde_json::json!({ "result": &committed_outcome(), "trace": [] }),
    );
    assert_golden(
        "traced_errored.json",
        &serde_json::json!({
            "result": { "status": "errored", "error": "bind matched 2 claims" },
            "trace": [],
        }),
    );

    let mut batch_committed = to_value(&committed_outcome());
    batch_committed
        .as_object_mut()
        .unwrap()
        .insert("row".to_string(), serde_json::json!(1));
    assert_golden("batch_committed_receipt.json", &batch_committed);
    let mut batch_rejected = to_value(&rejected_outcome());
    batch_rejected
        .as_object_mut()
        .unwrap()
        .insert("row".to_string(), serde_json::json!(2));
    assert_golden("batch_rejected_receipt.json", &batch_rejected);
    assert_golden(
        "batch_error_receipt.json",
        &serde_json::json!({
            "row": 3,
            "status": "error",
            "error": "malformed batch row: expected value at line 1 column 1",
        }),
    );
}

// A real coverage report over a programme with a DECLARED discipline,
// so the golden carries the generated invariant's `from` provenance -
// the one optional field with a special Python mapping (`from` is a
// Python keyword; the client maps it to `from_clause`). The authored
// invariant is refused once (constrained, with first/last refusal
// ids), flag_account is used, open_account is declared-but-unused
// with a gate refusal, and historical-only names are flagged on both
// the transformation and invariant sides: every optional field of
// the shape appears in the golden.
#[test]
fn coverage_report_serializes_as_pinned() {
    let source = "program envelopes_coverage\n\
        predicate Account(account_id: Subject)\n\
        predicate Flag(account_id: Subject)\n\
        predicate CurrentRef(account_id: Subject, ref_id: Subject)\n    \
            current pointer by (account_id)\n\
        invariant no_flagged_accounts:\n    \
            Account(a) implies not Flag(a)\n\
        transformation open_account(account_id):\n    \
            admit Account(account_id)\n\
        transformation flag_account(account_id):\n    \
            admit Flag(account_id)\n";
    let p = morpholog_surface::parse_program(source).unwrap();
    p.validate().unwrap();

    let mut tracker = CoverageTracker::new(&p);
    let empty = State::from_claims(vec![]);
    let with_ref = State::from_claims(vec![ClaimInstance {
        predicate: "CurrentRef".into(),
        args: vec![
            EvalValue::Subject(Subject::from("acct_1")),
            EvalValue::Subject(Subject::from("ref_1")),
        ],
    }]);
    let delta_ref = std::iter::once("CurrentRef".into()).collect();
    let delta_flag = std::iter::once("Flag".into()).collect();
    tracker
        .observe(&with_ref, &empty, &delta_ref, "t1", "flag_account")
        .unwrap();
    tracker
        .observe(&with_ref, &with_ref, &delta_flag, "t2", "renamed_long_ago")
        .unwrap();
    tracker.observe_rejection(Some("no_flagged_accounts"), "flag_account", "r1");
    tracker.observe_rejection(None, "open_account", "r2");
    tracker.observe_rejection(Some("retired_rule"), "renamed_long_ago", "r3");
    assert_golden("coverage_report.json", &to_value(&tracker.into_report()));
}

// Single-shape reports, built as their commands build them.
#[test]
fn report_envelopes_serialize_as_pinned() {
    assert_golden(
        "init_report.json",
        &serde_json::json!({ "status": "initialised", "schema": "morpholog" }),
    );
    assert_golden(
        "hash_report.json",
        &serde_json::json!({
            "program": "envelopes",
            "hash": format!("sha256:{}", "0".repeat(64)),
        }),
    );
    assert_golden(
        "check_report.json",
        &serde_json::json!({
            "file": "model.morph",
            "diagnostics": [{
                "severity": "error",
                "message": "undeclared predicate `Ghost` referenced in invariant `cap`",
                "start": 412, "end": 447, "line": 19, "column": 1,
            }],
        }),
    );
    assert_golden(
        "named_claim.json",
        &serde_json::json!({
            "predicate": "TradeSettled",
            "args": { "trade": "trade_1", "settled_qty": "5000", "flagged": false },
        }),
    );
}

// The tamper-evidence family: the envelopes of `verify`, `checkpoint`,
// and `evidence export`/`verify`. One golden per serialized variant so
// the reality layer pins every shape an embedder decodes.

fn sample_checkpoint() -> Checkpoint {
    Checkpoint {
        tree_size: 2,
        root_hash: format!("sha256:{}", "a".repeat(64)),
        prev_checkpoint_hash: None,
        checkpoint_hash: format!("sha256:{}", "b".repeat(64)),
        signatures: Vec::new(),
    }
}

fn sample_audit_row() -> AuditRow {
    AuditRow {
        transition_id: sample_uuid(),
        transformation_name: "open_account".into(),
        arguments: vec![EvalValue::Subject(Subject::from("acct_1"))],
        actor: Subject::from("alex"),
        invariant_epoch: 1,
        invariants_checked: vec![],
        asserted_claims: vec![kitchen_sink_claim()],
        retracted_claims: vec![],
        emitted_intents: vec![],
        committed_at: chrono::Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap(),
    }
}

#[test]
fn tamper_evidence_envelopes_serialize_as_pinned() {
    // `verify`: replay verdict beside tamper-evidence verdict.
    assert_golden(
        "verify_report_consistent.json",
        &to_value(&VerifyReport {
            replay: VerifyOutcome::Consistent {
                transitions: 2,
                claims: 3,
            },
            tree: TreeVerification::Intact {
                checkpoints: 1,
                tree_size: 2,
            },
        }),
    );
    assert_golden(
        "verify_report_divergent.json",
        &to_value(&VerifyReport {
            replay: VerifyOutcome::Divergent {
                only_in_claims_table: vec![kitchen_sink_claim()],
                only_in_replay: vec![],
            },
            tree: TreeVerification::Tampered {
                tree_size: 2,
                recorded_root: format!("sha256:{}", "a".repeat(64)),
                recomputed_root: format!("sha256:{}", "c".repeat(64)),
            },
        }),
    );

    // `checkpoint`: both variants carry a full checkpoint under the tag.
    assert_golden(
        "checkpoint_created.json",
        &to_value(&CheckpointOutcome::Created(sample_checkpoint())),
    );
    let mut signed = sample_checkpoint();
    signed.signatures = vec![TreeHeadSignature {
        key_id: "audit-2026-q3".into(),
        purpose: "audit_checkpoint_v1".into(),
        public_key: format!("ed25519-pub:{}", "c".repeat(64)),
        signature: format!("ed25519-sig:{}", "d".repeat(128)),
    }];
    assert_golden(
        "checkpoint_created_signed.json",
        &to_value(&CheckpointOutcome::Created(signed)),
    );
    assert_golden(
        "checkpoint_no_new_rows.json",
        &to_value(&CheckpointOutcome::NoNewRows(sample_checkpoint())),
    );

    // `evidence export`: the portable pack.
    assert_golden(
        "evidence_pack.json",
        &to_value(&EvidencePack {
            manifest: PackManifest {
                pack_format_version: 1,
                tree_size: 2,
                root_hash: format!("sha256:{}", "a".repeat(64)),
                checkpoint_hash: format!("sha256:{}", "b".repeat(64)),
            },
            checkpoints: vec![sample_checkpoint()],
            rows: vec![sample_audit_row()],
        }),
    );

    // `evidence verify`: the remaining tree verdicts (intact and tampered
    // are pinned via verify_report above).
    assert_golden(
        "tree_verification_chain_broken.json",
        &to_value(&TreeVerification::ChainBroken {
            detail: "checkpoint 2 prev link does not match checkpoint 1".into(),
        }),
    );
    assert_golden(
        "tree_verification_anchor_mismatch.json",
        &to_value(&TreeVerification::AnchorMismatch {
            tree_size: 2,
            anchor_checkpoint_hash: format!("sha256:{}", "b".repeat(64)),
            stored_checkpoint_hash: Some(format!("sha256:{}", "d".repeat(64))),
        }),
    );
    assert_golden(
        "tree_verification_malformed_pack.json",
        &to_value(&TreeVerification::MalformedPack {
            detail: "pack rows do not match the manifest tree_size".into(),
        }),
    );
    assert_golden(
        "tree_verification_signature_invalid.json",
        &to_value(&TreeVerification::SignatureInvalid {
            tree_size: 2,
            key_id: "audit-2026-q3".into(),
            purpose: "audit_checkpoint_v1".into(),
            public_key: format!("ed25519-pub:{}", "c".repeat(64)),
        }),
    );
    assert_golden(
        "tree_verification_unauthorized_key.json",
        &to_value(&TreeVerification::UnauthorizedKey {
            tree_size: 2,
            key_id: "audit-2026-q3".into(),
            purpose: "audit_checkpoint_v1".into(),
            public_key: format!("ed25519-pub:{}", "c".repeat(64)),
        }),
    );
    assert_golden(
        "tree_verification_signature_required.json",
        &to_value(&TreeVerification::SignatureRequired { tree_size: 2 }),
    );
}

// ============================================================
// Pin layer: every golden validates against its $defs entry in the
// embedded result.json.
// ============================================================

fn result_schema() -> serde_json::Value {
    serde_json::from_str(include_str!("../src/schemas/result.json"))
        .expect("the embedded result schema is valid JSON")
}

/// Shallow structural validation: discriminants (const/enum), required
/// keys, key-set strictness, recursion through properties / items /
/// $ref / oneOf. Deliberately ignores format/pattern/minimum - the
/// reality layer pins exact bytes; this layer pins that result.json
/// AGREES with those bytes structurally.
fn validate(
    value: &serde_json::Value,
    schema: &serde_json::Value,
    defs: &serde_json::Value,
    path: &str,
) -> Result<(), String> {
    if let Some(reference) = schema.get("$ref").and_then(|r| r.as_str()) {
        let name = reference
            .strip_prefix("#/$defs/")
            .ok_or_else(|| format!("{path}: unsupported $ref {reference}"))?;
        let target = defs
            .get(name)
            .ok_or_else(|| format!("{path}: $ref to unknown def {name}"))?;
        return validate(value, target, defs, path);
    }
    if let Some(branches) = schema.get("oneOf").and_then(|o| o.as_array()) {
        let mut failures = Vec::new();
        for (i, branch) in branches.iter().enumerate() {
            match validate(value, branch, defs, &format!("{path}|{i}")) {
                Ok(()) => return Ok(()),
                Err(e) => failures.push(e),
            }
        }
        return Err(format!("{path}: no oneOf branch matched ({failures:?})"));
    }
    if let Some(expected) = schema.get("const") {
        return if value == expected {
            Ok(())
        } else {
            Err(format!("{path}: expected const {expected}, got {value}"))
        };
    }
    if let Some(allowed) = schema.get("enum").and_then(|e| e.as_array()) {
        return if allowed.contains(value) {
            Ok(())
        } else {
            Err(format!("{path}: {value} not in enum {allowed:?}"))
        };
    }
    let types: Vec<&str> = match schema.get("type") {
        Some(serde_json::Value::String(s)) => vec![s.as_str()],
        Some(serde_json::Value::Array(a)) => a.iter().filter_map(|t| t.as_str()).collect(),
        _ => return Ok(()),
    };
    let matches_type = types.iter().any(|t| match *t {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "integer" => value.is_i64() || value.is_u64(),
        "null" => value.is_null(),
        _ => false,
    });
    if !matches_type {
        return Err(format!("{path}: {value} is not of type {types:?}"));
    }
    if value.is_object() && types.contains(&"object") {
        let object = value.as_object().unwrap();
        let properties = schema.get("properties").and_then(|p| p.as_object());
        if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
            for key in required.iter().filter_map(|k| k.as_str()) {
                if !object.contains_key(key) {
                    return Err(format!("{path}: missing required key `{key}`"));
                }
            }
        }
        for (key, item) in object {
            let property_schema = properties.and_then(|p| p.get(key));
            match property_schema {
                Some(s) => validate(item, s, defs, &format!("{path}.{key}"))?,
                None => match schema.get("additionalProperties") {
                    Some(serde_json::Value::Bool(false)) => {
                        return Err(format!("{path}: unexpected key `{key}`"));
                    }
                    Some(extra) if extra.is_object() => {
                        validate(item, extra, defs, &format!("{path}.{key}"))?;
                    }
                    _ => {}
                },
            }
        }
    }
    if value.is_array()
        && let Some(items) = schema.get("items")
    {
        for (i, item) in value.as_array().unwrap().iter().enumerate() {
            validate(item, items, defs, &format!("{path}[{i}]"))?;
        }
    }
    Ok(())
}

#[test]
fn every_golden_validates_against_its_defs_entry() {
    let schema = result_schema();
    let defs = schema.get("$defs").expect("result.json has $defs");
    let cases = [
        ("committed.json", "committed"),
        ("rejected.json", "rejected"),
        ("rejected_with_explanation.json", "rejected"),
        ("traced_committed.json", "traced_envelope"),
        ("traced_errored.json", "traced_envelope"),
        ("batch_committed_receipt.json", "batch_receipt"),
        ("batch_rejected_receipt.json", "batch_receipt"),
        ("batch_error_receipt.json", "batch_receipt"),
        ("explanation_admissible.json", "explanation"),
        ("explanation_gate.json", "explanation"),
        ("explanation_invariant.json", "explanation"),
        ("explanation_error.json", "explanation"),
        ("outbox_row.json", "outbox_row"),
        ("outbox_claim.json", "outbox_claim"),
        ("outbox_claim_null.json", "outbox_claim"),
        ("outbox_update_applied.json", "outbox_update"),
        ("outbox_update_lease_lost.json", "outbox_update"),
        ("coverage_report.json", "coverage_report"),
        ("audit_row.json", "audit_row"),
        ("audit_row_named.json", "audit_row_named"),
        ("check_report.json", "check_report"),
        ("hash_report.json", "hash_report"),
        ("init_report.json", "init_report"),
        ("named_claim.json", "named_claim"),
        ("verify_report_consistent.json", "verify_report"),
        ("verify_report_divergent.json", "verify_report"),
        ("checkpoint_created.json", "checkpoint_outcome"),
        ("checkpoint_created_signed.json", "checkpoint_outcome"),
        ("checkpoint_no_new_rows.json", "checkpoint_outcome"),
        ("evidence_pack.json", "evidence_pack"),
        ("tree_verification_chain_broken.json", "tree_verification"),
        (
            "tree_verification_anchor_mismatch.json",
            "tree_verification",
        ),
        ("tree_verification_malformed_pack.json", "tree_verification"),
        (
            "tree_verification_signature_invalid.json",
            "tree_verification",
        ),
        (
            "tree_verification_unauthorized_key.json",
            "tree_verification",
        ),
        (
            "tree_verification_signature_required.json",
            "tree_verification",
        ),
    ];
    for (file, def) in cases {
        let golden: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(golden_dir().join(file))
                .unwrap_or_else(|e| panic!("missing golden {file} ({e}); run UPDATE_GOLDENS=1")),
        )
        .unwrap();
        let entry = defs
            .get(def)
            .unwrap_or_else(|| panic!("result.json lacks $defs entry `{def}`"));
        validate(&golden, entry, defs, file)
            .unwrap_or_else(|e| panic!("{file} does not match $defs/{def}: {e}"));
    }
}

// The schema document itself stays structurally sane: every $ref in it
// resolves to a $defs entry, so a rename cannot silently orphan one.
#[test]
fn every_internal_ref_resolves() {
    let schema = result_schema();
    let defs = schema.get("$defs").unwrap().as_object().unwrap();
    let mut stack = vec![schema.clone()];
    while let Some(node) = stack.pop() {
        match node {
            serde_json::Value::Object(map) => {
                if let Some(reference) = map.get("$ref").and_then(|r| r.as_str()) {
                    let name = reference.strip_prefix("#/$defs/").unwrap_or_else(|| {
                        panic!("non-local $ref {reference}");
                    });
                    assert!(defs.contains_key(name), "$ref to unknown def {name}");
                }
                stack.extend(map.values().cloned());
            }
            serde_json::Value::Array(items) => stack.extend(items),
            _ => {}
        }
    }
}
