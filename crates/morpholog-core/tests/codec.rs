//! JSON codec round-trip tests for the runtime value types.
//!
//! PG persistence writes these JSON shapes into JSONB columns declared
//! in `crates/morpholog-core/sql/schema.sql`. The round-trip tests pin
//! both the wire shape (so future readers cannot accidentally change
//! it) and the exactness contract (decimals serialise as JSON
//! **strings**, never as JSON numbers).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{ClaimInstance, EvalValue, IntentInstance, Subject, Transition};
use morpholog_test_support::{dec, subj};
use rust_decimal::Decimal;
use std::str::FromStr;

#[test]
fn eval_value_decimal_round_trips_as_tagged_json_string() {
    let v = EvalValue::Decimal(Decimal::new(917, 1));
    let json = serde_json::to_string(&v).unwrap();
    assert_eq!(json, r#"{"type":"decimal","value":"91.7"}"#);
    let parsed: EvalValue = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, v);
}

#[test]
fn eval_value_subject_round_trips_as_tagged_json_string() {
    let v = EvalValue::Subject("asset_a".into());
    let json = serde_json::to_string(&v).unwrap();
    assert_eq!(json, r#"{"type":"subject","value":"asset_a"}"#);
    let parsed: EvalValue = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, v);
}

#[test]
fn eval_value_bool_round_trips_as_tagged_json() {
    let v = EvalValue::Bool(true);
    let json = serde_json::to_string(&v).unwrap();
    assert_eq!(json, r#"{"type":"bool","value":true}"#);
    let parsed: EvalValue = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, v);
}

#[test]
fn eval_value_date_round_trips_as_tagged_json_string() {
    // Civil dates serialise as ISO-8601 strings under the
    // adjacently-tagged shape. Pinning the exact wire shape so future
    // changes to jiff's serde format cannot silently break the PG JSONB
    // contract.
    let v: EvalValue = EvalValue::Date("2026-03-12".parse().unwrap());
    let json = serde_json::to_string(&v).unwrap();
    assert_eq!(json, r#"{"type":"date","value":"2026-03-12"}"#);
    let parsed: EvalValue = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, v);
}

#[test]
fn eval_value_collection_round_trips_through_nested_json() {
    let v = EvalValue::Collection(vec![
        EvalValue::Subject("l1".into()),
        EvalValue::Subject("l2".into()),
        EvalValue::Decimal(Decimal::new(60, 0)),
    ]);
    let json = serde_json::to_string(&v).unwrap();
    let parsed: EvalValue = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, v);
    // Nested decimal must still be a string inside the collection.
    assert!(json.contains(r#""type":"decimal","value":"60""#));
}

#[test]
fn claim_instance_round_trips_through_json_with_mixed_args() {
    let c = ClaimInstance {
        predicate: "BankRecognisedRevenue".to_string(),
        args: vec![subj("asset_a"), subj("p_2026_04"), dec(92), subj("rec_001")],
    };
    let json = serde_json::to_string(&c).unwrap();
    let parsed: ClaimInstance = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, c);
    // Shape is { "predicate": "...", "args": [ ... ] }.
    assert!(json.starts_with(r#"{"predicate":"BankRecognisedRevenue","args":["#));
}

#[test]
fn intent_instance_round_trips_through_json() {
    let i = IntentInstance {
        name: "NetSettlementCreated".to_string(),
        args: vec![subj("net1")],
    };
    let json = serde_json::to_string(&i).unwrap();
    let parsed: IntentInstance = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, i);
    assert!(json.starts_with(r#"{"name":"NetSettlementCreated","args":["#));
}

#[test]
fn decimal_value_preserves_high_precision_through_json() {
    // 18 fractional digits - well within rust_decimal's 96-bit range.
    let exact = "1234567890.123456789";
    let v = EvalValue::Decimal(Decimal::from_str(exact).unwrap());
    let json = serde_json::to_string(&v).unwrap();
    // The decimal must appear verbatim, as a quoted string.
    assert!(json.contains(&format!(r#""value":"{exact}""#)));
    let parsed: EvalValue = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, v);
}

#[test]
fn claim_args_serialise_as_a_json_array() {
    // Pins the split contract: the `claims.arguments` column has a CHECK
    // constraint `jsonb_typeof(arguments) = 'array'`. The PG adapter
    // writes only the `args` field of a ClaimInstance into that column.
    let c = ClaimInstance {
        predicate: "Quantity".to_string(),
        args: vec![subj("trade123"), dec(10)],
    };
    let args_json = serde_json::to_string(&c.args).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&args_json).unwrap();
    assert!(
        parsed.is_array(),
        "claim.args must serialise as a JSON array"
    );
}

#[test]
fn intent_args_serialise_as_a_json_array() {
    // Pins the split contract: the `outbox.arguments` column has a CHECK
    // constraint `jsonb_typeof(arguments) = 'array'`. The PG adapter
    // writes only the `args` field of an IntentInstance into that column.
    let i = IntentInstance {
        name: "NetSettlementCreated".to_string(),
        args: vec![subj("net1"), dec(100)],
    };
    let args_json = serde_json::to_string(&i.args).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&args_json).unwrap();
    assert!(
        parsed.is_array(),
        "intent.args must serialise as a JSON array"
    );
}

// ============================================================
// Transition actor codec (`actor_repr`)
//
// `Transition.actor` is a `Subject`, but it serialises through
// `actor_repr` as a tagged `EvalValue::Subject`, so the audit `actor`
// column and the CLI transition JSON keep their v0 shape. Deserialisation
// validates the tag at the boundary - the one place a non-subject actor
// can still enter, now that the kernel type makes it otherwise
// unrepresentable. These pin both halves of that contract.
// ============================================================

#[test]
fn transition_actor_round_trips_as_tagged_subject() {
    let t = Transition {
        transformation_name: "do_it".to_string(),
        args: vec![],
        actor: Subject::from("alice"),
    };
    let json = serde_json::to_string(&t).unwrap();
    assert!(
        json.contains(r#""actor":{"type":"subject","value":"alice"}"#),
        "actor must serialise as a tagged subject: {json}"
    );
    let parsed: Transition = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, t);
}

#[test]
fn transition_deserialize_rejects_non_subject_actor() {
    let bad = r#"{"transformation_name":"do_it","args":[],"actor":{"type":"decimal","value":"1"}}"#;
    let err = serde_json::from_str::<Transition>(bad)
        .expect_err("a non-subject actor must fail to deserialise");
    assert!(
        err.to_string().contains("actor must be a subject"),
        "error must name the boundary contract: {err}"
    );
}

// ============================================================
// Trace wire format
//
// The CLI's `morpholog propose --trace` flag emits a JSON object
// whose `trace` field is a Vec<TraceEntry>. The tests below pin the
// serde-derived wire shape so that an accidental serde-attribute
// change (renaming a `kind` tag, switching from snake_case, etc.)
// breaks the test rather than silently breaking downstream
// consumers.
//
// `TracedProposal` is NOT covered here - it deliberately does not
// derive serde at this stage (would transitively require Outcome /
// EvalError / State to serialise). The CLI assembles its own
// {result, trace} wrapper.
// ============================================================

use morpholog_core::{
    BindOneOutcome, ForIterationTrace, RenderedClaim, RequireOutcome, TraceEntry,
};

#[test]
fn trace_entry_require_held_round_trips_with_tagged_shape() {
    let entry = TraceEntry::Require {
        expression: "Foo(x)".to_string(),
        outcome: RequireOutcome::Held { match_count: 1 },
    };
    let json = serde_json::to_string(&entry).unwrap();
    // Internally tagged on `kind` with snake_case variants.
    assert!(
        json.contains(r#""kind":"require""#),
        "expected `kind: require` discriminant, got: {json}"
    );
    assert!(
        json.contains(r#""expression":"Foo(x)""#),
        "expression must round-trip verbatim: {json}"
    );
    // Outer Require entry contains a nested RequireOutcome,
    // internally tagged on `status` with snake_case.
    assert!(
        json.contains(r#""status":"held""#),
        "nested outcome must carry status discriminant: {json}"
    );
    assert!(
        json.contains(r#""match_count":1"#),
        "match_count must be present: {json}"
    );
    let parsed: TraceEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, entry);
}

#[test]
fn trace_entry_require_rejected_round_trips() {
    let entry = TraceEntry::Require {
        expression: "Bar(y)".to_string(),
        outcome: RequireOutcome::Rejected {
            reason: "require failed: Bar(y) did not hold over pre-state".to_string(),
            failing_sub_expression: None,
            directly_missing_claims: vec![],
        },
    };
    let json = serde_json::to_string(&entry).unwrap();
    assert!(json.contains(r#""status":"rejected""#));
    assert!(json.contains(r#""reason":"#));
    // failing_sub_expression: None must be SKIPPED in JSON output
    // (skip_serializing_if). Wire stays compact when the walker
    // declines to drill in.
    assert!(
        !json.contains("failing_sub_expression"),
        "None failing_sub_expression must be skipped from JSON: {json}"
    );
    let parsed: TraceEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, entry);
}

/// `failing_sub_expression: Some(...)` must round-trip with the
/// field present in JSON.
#[test]
fn trace_entry_require_rejected_round_trips_with_failing_sub_expression() {
    let entry = TraceEntry::Require {
        expression: "and(Foo(x), Bar(y))".to_string(),
        outcome: RequireOutcome::Rejected {
            reason: "require failed: and(Foo(x), Bar(y)) did not hold over pre-state".to_string(),
            failing_sub_expression: Some("Bar(y)".to_string()),
            directly_missing_claims: vec![],
        },
    };
    let json = serde_json::to_string(&entry).unwrap();
    assert!(json.contains(r#""status":"rejected""#));
    assert!(json.contains(r#""failing_sub_expression":"Bar(y)""#));
    let parsed: TraceEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, entry);
}

/// A non-empty `directly_missing_claims` must round-trip with each
/// claim's `predicate` and `rendered` present in JSON. Pins the wire
/// shape of the structured missing-claim list the explanation engine
/// reads.
#[test]
fn trace_entry_require_rejected_round_trips_with_directly_missing_claims() {
    let entry = TraceEntry::Require {
        expression: "MayApprove(actor, doc_type)".to_string(),
        outcome: RequireOutcome::Rejected {
            reason: "require failed: MayApprove(actor, doc_type) did not hold over pre-state"
                .to_string(),
            failing_sub_expression: None,
            directly_missing_claims: vec![RenderedClaim {
                predicate: "MayApprove".to_string(),
                rendered: "MayApprove(alice, contract)".to_string(),
            }],
        },
    };
    let json = serde_json::to_string(&entry).unwrap();
    assert!(json.contains(r#""predicate":"MayApprove""#));
    assert!(json.contains(r#""rendered":"MayApprove(alice, contract)""#));
    let parsed: TraceEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, entry);
}

#[test]
fn trace_entry_bind_one_bound_round_trips_with_sorted_bindings() {
    let entry = TraceEntry::BindOne {
        expression: "Policy(pid, limit)".to_string(),
        outcome: BindOneOutcome::Bound {
            bindings: vec![("limit".into(), dec(100)), ("pid".into(), subj("p1"))],
        },
    };
    let json = serde_json::to_string(&entry).unwrap();
    assert!(json.contains(r#""kind":"bind_one""#));
    assert!(json.contains(r#""status":"bound""#));
    let parsed: TraceEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, entry);
}

#[test]
fn trace_entry_bind_one_no_match_round_trips() {
    let entry = TraceEntry::BindOne {
        expression: "Policy(pid, limit)".to_string(),
        outcome: BindOneOutcome::NoMatch {
            failing_sub_expression: None,
            directly_missing_claims: vec![],
        },
    };
    let json = serde_json::to_string(&entry).unwrap();
    assert!(json.contains(r#""status":"no_match""#));
    assert!(
        !json.contains("failing_sub_expression"),
        "None failing_sub_expression must be skipped from JSON: {json}"
    );
    let parsed: TraceEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, entry);
}

#[test]
fn trace_entry_bind_one_multiple_matches_round_trips() {
    let entry = TraceEntry::BindOne {
        expression: "Policy(pid, limit)".to_string(),
        outcome: BindOneOutcome::MultipleMatches { count: 3 },
    };
    let json = serde_json::to_string(&entry).unwrap();
    assert!(json.contains(r#""status":"multiple_matches""#));
    assert!(json.contains(r#""count":3"#));
    let parsed: TraceEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, entry);
}

#[test]
fn trace_entry_for_with_per_iteration_items_round_trips() {
    let entry = TraceEntry::For {
        binding: "line".into(),
        iterations: vec![
            ForIterationTrace {
                item: subj("L1"),
                trace: vec![TraceEntry::Assert {
                    claim: ClaimInstance {
                        predicate: "Echo".to_string(),
                        args: vec![subj("L1")],
                    },
                }],
            },
            ForIterationTrace {
                item: subj("L2"),
                trace: vec![],
            },
        ],
    };
    let json = serde_json::to_string(&entry).unwrap();
    assert!(json.contains(r#""kind":"for""#));
    assert!(json.contains(r#""binding":"line""#));
    assert!(json.contains(r#""kind":"assert""#));
    let parsed: TraceEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, entry);
}

#[test]
fn trace_entry_invariant_check_round_trips() {
    let entry = TraceEntry::InvariantCheck {
        name: "balanced_posted_entry".to_string(),
        expression: "implies(JournalEntry(...), ...)".to_string(),
        held: false,
    };
    let json = serde_json::to_string(&entry).unwrap();
    assert!(json.contains(r#""kind":"invariant_check""#));
    assert!(json.contains(r#""held":false"#));
    let parsed: TraceEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, entry);
}

#[test]
fn trace_entry_let_let_new_subject_assert_retract_emit_round_trip() {
    // Each remaining TraceEntry variant in one batch. The exact
    // wire bytes are less important than the round-trip closure.
    let entries = vec![
        TraceEntry::Let {
            name: "x".into(),
            value: dec(42),
        },
        TraceEntry::LetNewSubject {
            name: "y".into(),
            subject: subj("generated-uuid"),
        },
        TraceEntry::Assert {
            claim: ClaimInstance {
                predicate: "Echo".to_string(),
                args: vec![subj("a"), dec(1)],
            },
        },
        TraceEntry::Retract {
            predicate: "OldClaim".to_string(),
            retracted: vec![ClaimInstance {
                predicate: "OldClaim".to_string(),
                args: vec![subj("z")],
            }],
        },
        TraceEntry::Emit {
            intent: IntentInstance {
                name: "Notified".to_string(),
                args: vec![subj("a")],
            },
        },
    ];
    for entry in entries {
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: TraceEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(
            parsed, entry,
            "round-trip mismatch on {entry:?}, wire was: {json}"
        );
    }
}
