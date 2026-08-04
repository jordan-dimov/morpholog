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
    BatchScore, CandidateScorer, CaseOutcome, CaseResult, ClaimInstance, CoverageTracker,
    EvalValue, IntentInstance, SplitBoundaryReport, State, Subject, Transition, WitnessBinding,
    explain,
};
use morpholog_postgres::{
    AuditRow, AuditedInvariantCheck, Checkpoint, CheckpointOutcome, EvidencePack, OutboxRow,
    PackManifest, PgProposalOutcome, RowInclusionProof, SelectiveEvidencePack,
    SelectivePackManifest, SelectiveVerification, TreeHeadSignature, TreeVerification,
    VerifyOutcome, VerifyReport, ViewsVerification, WindowEvidencePack, WindowPackManifest,
    WindowVerification,
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

/// Like [`assert_golden`], but pins the value's DIRECT serialization -
/// the exact bytes `print_json` emits - instead of the
/// `Value`-normalized form (whose object keys re-sort). For envelopes
/// whose struct declaration order (or a `flatten`) is the wire order.
fn assert_golden_bytes<T: serde::Serialize>(name: &str, value: &T) {
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
        rule: Some("no_flagged_accounts".to_string()),
        witness: Vec::new(),
    }
}

/// The same rejection, carrying the values the rule was reading. Pinned
/// separately from the witness-less shape so both stay covered: a refusal
/// the kernel cannot pin to one iteration still emits the original
/// envelope, byte for byte.
fn rejected_with_witness_outcome() -> PgProposalOutcome {
    PgProposalOutcome::Rejected {
        reason: "invariant `no_flagged_accounts` violated".to_string(),
        rule: Some("no_flagged_accounts".to_string()),
        witness: witness_sample(),
    }
}

/// The witness both rejection goldens share.
fn witness_sample() -> Vec<WitnessBinding> {
    vec![
        WitnessBinding {
            var: "account".into(),
            value: EvalValue::Subject(Subject::from("acct_1")),
        },
        WitnessBinding {
            var: "exposure".into(),
            value: EvalValue::Decimal(rust_decimal::Decimal::new(10550, 2)),
        },
    ]
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
    assert_golden(
        "rejected_with_witness.json",
        &to_value(&rejected_with_witness_outcome()),
    );
}

/// `explanation_program` with its gate named, and nothing else changed.
fn named_gate_program() -> morpholog_core::Program {
    let mut p = explanation_program();
    for t in &mut p.transformations {
        for stmt in &mut t.body {
            if let morpholog_core::Stmt::Require { name, .. } = stmt {
                *name = Some("account_is_not_flagged".into());
            }
        }
    }
    p
}

/// A programme whose statements reach every trace step kind, so the
/// goldens below can cover each arm of the union rather than the arms one
/// happy path happens to hit.
fn traced_program() -> morpholog_core::Program {
    use morpholog_core::ir_builder::{
        bind_one_named, emit, for_, let_, let_new_subject, require_named, retract, term,
    };
    program("traced")
        .predicates(vec![
            predicate("Account").subject("account").build(),
            predicate("Approved").subject("account").build(),
            predicate("Draft").subject("account").build(),
            predicate("Seen").subject("item").build(),
            predicate("Note").subject("note").build(),
        ])
        .intents(vec![morpholog_core::IntentDecl {
            name: "Notified".into(),
            args: vec![morpholog_core::ArgDecl {
                name: "account".into(),
                kind: morpholog_core::PredicateArgKind::Subject,
            }],
        }])
        .invariants(vec![invariant(
            "approved_accounts_are_known",
            implies(
                claim("Approved", vec![var("account")]),
                claim("Account", vec![var("account")]),
            ),
        )])
        .transformations(vec![
            transformation(
                "walk",
                params(&["account", "items"]),
                vec![
                    bind_one_named("the_account", claim("Account", vec![var("account")])),
                    require_named(
                        "not_yet_approved",
                        not(claim("Approved", vec![var("account")])),
                    ),
                    let_("copy", term(var("account"))),
                    let_new_subject("receipt"),
                    retract("Draft", vec![var("account")]),
                    assert_("Approved", vec![var("account")]),
                    for_(
                        "item",
                        term(var("items")),
                        vec![assert_("Seen", vec![var("item")])],
                    ),
                    emit("Notified", vec![var("account")]),
                ],
            ),
            // Admits standing with no account behind it, so the invariant
            // check fails and the trace records held: false.
            transformation(
                "orphan",
                params(&["account"]),
                vec![assert_("Approved", vec![var("account")])],
            ),
            // An unbound lookup over ambiguous state: a kernel error, and
            // the only way to reach BindOneOutcome::MultipleMatches.
            transformation(
                "pick_note",
                params(&[]),
                vec![
                    bind_one_named("any_note", claim("Note", vec![var("note")])),
                    assert_("Seen", vec![var("note")]),
                ],
            ),
        ])
        .build()
}

/// One real `propose_with_trace` run: the outcome AND the trace from the
/// same invocation.
///
/// Taking them from different runs is how the first cut of these goldens
/// went wrong - a trace that stopped at a failed lookup was pinned beside
/// an invariant-violation reason, a pairing the runtime cannot produce. The
/// point of a cross-layer golden is that it holds the runtime, the schema
/// and the client to one sample; a self-contradicting sample holds them to
/// nothing.
fn traced_run(
    name: &str,
    args: Vec<EvalValue>,
    pre: &State,
) -> (PgProposalOutcome, Vec<morpholog_core::TraceEntry>) {
    use morpholog_core::{Outcome, RejectionReason, TracedProposal, propose_with_trace};
    let p = traced_program();
    let t = p.transformations.iter().find(|t| t.name == *name).unwrap();
    let transition = Transition {
        transformation_name: t.name.clone(),
        args,
        actor: Subject::from("alex"),
    };
    match propose_with_trace(t, &transition, pre, &p.invariants, &p.definitions) {
        TracedProposal::Completed { outcome, trace } => {
            let envelope = match outcome {
                Outcome::Accepted {
                    asserted_claims,
                    retracted_claims,
                    emitted_intents,
                    ..
                } => PgProposalOutcome::Committed {
                    transition_id: sample_uuid(),
                    actor: transition.actor.clone(),
                    asserted_claims,
                    retracted_claims,
                    emitted_intents,
                },
                Outcome::Rejected { reason } => PgProposalOutcome::Rejected {
                    reason: reason.to_string(),
                    // Matched off the variant, the way the adapter does it.
                    rule: match &reason {
                        RejectionReason::Invariant { name, .. } => Some(name.to_string()),
                        RejectionReason::Require { name, .. }
                        | RejectionReason::BindNone { name, .. } => {
                            name.as_ref().map(ToString::to_string)
                        }
                    },
                    witness: match &reason {
                        RejectionReason::Invariant { witness, .. } => witness.clone(),
                        _ => Vec::new(),
                    },
                },
            };
            let mut trace = trace;
            pin_fresh_subjects(&mut trace);
            (envelope, trace)
        }
        TracedProposal::Errored { error, .. } => panic!("expected a decision, got {error:?}"),
    }
}

/// The kernel-error path, which carries a trace of its own: the statements
/// that ran before the error, ending in the ambiguous lookup.
fn errored_run() -> (String, Vec<morpholog_core::TraceEntry>) {
    use morpholog_core::{TracedProposal, propose_with_trace};
    let p = traced_program();
    let t = p
        .transformations
        .iter()
        .find(|t| t.name == "pick_note")
        .unwrap();
    let transition = Transition {
        transformation_name: t.name.clone(),
        args: vec![],
        actor: Subject::from("alex"),
    };
    let pre = State::from_claims(vec![
        ClaimInstance {
            predicate: "Note".into(),
            args: vec![EvalValue::Subject(Subject::from("note_1"))],
        },
        ClaimInstance {
            predicate: "Note".into(),
            args: vec![EvalValue::Subject(Subject::from("note_2"))],
        },
    ]);
    match propose_with_trace(t, &transition, &pre, &p.invariants, &p.definitions) {
        TracedProposal::Errored { error, trace } => (error.to_string(), trace),
        TracedProposal::Completed { outcome, .. } => {
            panic!("expected a kernel error, got {outcome:?}")
        }
    }
}

/// A `let x = new Subject()` step mints a fresh UUIDv7 on every run, so its
/// entry is the one thing a byte-equal golden cannot take verbatim. That
/// subject - and only that subject - is replaced with a fixed one; every
/// other byte in these goldens is what the runtime produced. Without this
/// the step could not be pinned at all, and the schema would go on
/// describing an arm no cross-layer sample covers.
fn pin_fresh_subjects(trace: &mut [morpholog_core::TraceEntry]) {
    use morpholog_core::TraceEntry;
    for entry in trace.iter_mut() {
        match entry {
            TraceEntry::LetNewSubject { subject, .. } => {
                *subject =
                    EvalValue::Subject(Subject::from("01900000-0000-7000-8000-00000000000f"));
            }
            TraceEntry::For { iterations, .. } => {
                for it in iterations.iter_mut() {
                    pin_fresh_subjects(&mut it.trace);
                }
            }
            _ => {}
        }
    }
}

fn account_state(extra: &[&str]) -> State {
    let mut claims = vec![
        ClaimInstance {
            predicate: "Account".into(),
            args: vec![EvalValue::Subject(Subject::from("acct_1"))],
        },
        ClaimInstance {
            predicate: "Draft".into(),
            args: vec![EvalValue::Subject(Subject::from("acct_1"))],
        },
    ];
    for p in extra {
        claims.push(ClaimInstance {
            predicate: (*p).into(),
            args: vec![EvalValue::Subject(Subject::from("acct_1"))],
        });
    }
    State::from_claims(claims)
}

fn walk_args() -> Vec<EvalValue> {
    vec![
        EvalValue::Subject(Subject::from("acct_1")),
        EvalValue::Collection(vec![EvalValue::Subject(Subject::from("item_1"))]),
    ]
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
    // Two goldens, so both shapes stay covered: an unnamed gate omits
    // `rule` entirely (above, byte-identical to before names existed) and
    // a named one carries it. `explain` is a command in its own right, so
    // this is the only place a dry run can report which rule would refuse.
    let named = explain(
        &named_gate_program(),
        &transition("open_account"),
        &State::from_claims(vec![]),
    );
    assert_golden("explanation_gate_named.json", &to_value(&named));
    assert_golden("explanation_invariant.json", &to_value(&invariant_violated));
    assert_golden("explanation_error.json", &to_value(&error));
}

/// The rejection log's row shape, both with and without a witness.
///
/// Pinned now because it carries structured evidence: an embedder reading
/// `inspect rejections` programmatically was reaching an ad-hoc surface, and
/// the moment a surface is consumed it has to be an envelope with a floor
/// under it rather than a shape that can drift silently.
#[test]
fn rejection_rows_serialize_as_pinned() {
    let base = morpholog_postgres::RejectionRow {
        rejection_id: sample_uuid(),
        transformation_name: "issue_invoice".into(),
        arguments: vec![EvalValue::Subject(Subject::from("inv_1"))],
        actor: Subject::from("alex"),
        kind: "invariant".to_string(),
        rule: "line_net_is_the_rounded_recompute".to_string(),
        invariant_version: Some(1),
        reason: "invariant `line_net_is_the_rounded_recompute` violated".to_string(),
        witness: Some(witness_sample()),
        rejected_at: chrono::Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap(),
    };
    assert_golden("rejection_row.json", &to_value(&base));
    // A gate refusal, which has no witness and no version: the key is
    // absent rather than null, so a row from before the column existed
    // reads identically to one the kernel could not pin.
    let gate = morpholog_postgres::RejectionRow {
        kind: "require".to_string(),
        rule: "actor_has_authority_for_amount".to_string(),
        invariant_version: None,
        reason: "require `actor_has_authority_for_amount` failed: MayApprove(actor, doc) did not hold over pre-state".to_string(),
        witness: None,
        ..base
    };
    assert_golden("rejection_row_gate.json", &to_value(&gate));
}

/// The `migrate` report, in both shapes an operator meets: a database that
/// is behind, and one that has just been brought current.
///
/// Pinned because a deploy step reads it - `--check` exits non-zero and the
/// report says what is outstanding, which is a contract the moment anyone
/// gates on it.
#[test]
fn migration_reports_serialize_as_pinned() {
    let behind = morpholog_postgres::MigrationReport {
        recorded_version_before: Some(9),
        recorded_version_after: Some(9),
        binary_version: 11,
        applied: Vec::new(),
        unknown: Vec::new(),
        pending: vec![
            morpholog_postgres::MigrationRef {
                version: 10,
                name: "rejections_witness".to_string(),
            },
            morpholog_postgres::MigrationRef {
                version: 11,
                name: "schema_migrations".to_string(),
            },
        ],
    };
    assert_golden("migration_report_behind.json", &to_value(&behind));

    let applied = morpholog_postgres::MigrationReport {
        recorded_version_before: Some(9),
        recorded_version_after: Some(11),
        binary_version: 11,
        applied: behind.pending.clone(),
        pending: Vec::new(),
        unknown: Vec::new(),
    };
    assert_golden("migration_report_applied.json", &to_value(&applied));

    // A database migrated by a NEWER binary. The dangerous shape: nothing
    // is pending, and it is emphatically not current.
    let ahead = morpholog_postgres::MigrationReport {
        recorded_version_before: Some(12),
        recorded_version_after: Some(12),
        binary_version: 11,
        applied: Vec::new(),
        pending: Vec::new(),
        unknown: vec![morpholog_postgres::MigrationRef {
            version: 12,
            name: "something_this_build_never_saw".to_string(),
        }],
    };
    assert!(!ahead.is_current(), "an ahead database is not current");
    assert_golden("migration_report_ahead.json", &to_value(&ahead));
}

#[test]
fn outbox_row_serializes_as_pinned() {
    let row = OutboxRow {
        intent_id: sample_uuid(),
        transition_id: sample_uuid(),
        intent_type: "AccountOpened".to_string(),
        arguments: vec![EvalValue::Subject(Subject::from("acct_1"))],
        idempotency_key: "k1".to_string(),
        status: morpholog_postgres::OutboxStatus::Pending,
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
        attestation: None,
    };
    assert_golden("audit_row.json", &to_value(&row));

    // The attested variant: same row, plus the gateway attestation
    // lineage the adapter records on every commit it writes today.
    let attested = AuditRow {
        attestation: Some(morpholog_postgres::AuditAttestation::Gateway {
            authenticated_by: "morpholog_writer".to_string(),
        }),
        ..row.clone()
    };
    assert_golden("audit_row_attested.json", &to_value(&attested));

    // The --named form replaces the two claim arrays with
    // field-keyed bare objects (the named_claim shape) and leaves
    // everything else byte-identical - through the same projection
    // the binary runs.
    let named = morpholog_cli::envelopes::audit_row_named(
        &row,
        vec![morpholog_cli::envelopes::NamedClaim {
            args: [
                ("account_id".to_string(), serde_json::json!("acct_1")),
                ("balance".to_string(), serde_json::json!("100.50")),
            ]
            .into_iter()
            .collect(),
            predicate: "Account".into(),
        }],
        vec![],
    )
    .unwrap();
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
        &to_value(&morpholog_cli::envelopes::RejectedWithExplanation::new(
            "invariant `no_flagged_accounts` violated",
            Some("no_flagged_accounts"),
            &[],
            explanation,
        )),
    );
    // The combination the schema always permitted and the runtime could
    // not produce: the diagnosis path dropped the witness, so the one
    // command built for diagnosis answered least.
    let explanation = explain(&p, &transition("open_account"), &flagged_state());
    assert_golden(
        "rejected_with_explanation_and_witness.json",
        &to_value(&morpholog_cli::envelopes::RejectedWithExplanation::new(
            "invariant `no_flagged_accounts` violated",
            Some("no_flagged_accounts"),
            &witness_sample(),
            explanation,
        )),
    );
    // Every traced golden below is one real run: outcome and trace from the
    // same `propose_with_trace` call, so the pair is one the runtime can
    // actually produce. Between them they reach each arm of the trace union.
    let (committed, trace) = traced_run("walk", walk_args(), &account_state(&[]));
    assert_golden(
        "traced_committed.json",
        &to_value(&morpholog_cli::envelopes::Traced {
            result: &committed,
            trace,
        }),
    );
    // Refused by the invariant, so the trace ends in a failed check.
    let (rejected, trace) = traced_run(
        "orphan",
        vec![EvalValue::Subject(Subject::from("acct_1"))],
        &State::from_claims(vec![]),
    );
    assert_golden(
        "traced_rejected.json",
        &to_value(&morpholog_cli::envelopes::Traced {
            result: &rejected,
            trace,
        }),
    );
    // Refused at the gate: the lookup binds, then the require fails.
    let (gated, trace) = traced_run("walk", walk_args(), &account_state(&["Approved"]));
    assert_golden(
        "traced_rejected_at_gate.json",
        &to_value(&morpholog_cli::envelopes::Traced {
            result: &gated,
            trace,
        }),
    );
    // Refused at the lookup, before any gate runs.
    let (unbound, trace) = traced_run("walk", walk_args(), &State::from_claims(vec![]));
    assert_golden(
        "traced_rejected_at_bind.json",
        &to_value(&morpholog_cli::envelopes::Traced {
            result: &unbound,
            trace,
        }),
    );
    // The kernel-error path, and the only way to reach a multi-match trace.
    let (error, trace) = errored_run();
    assert_golden(
        "traced_errored.json",
        &to_value(&morpholog_cli::envelopes::Traced {
            result: morpholog_cli::envelopes::TracedError::new(error),
            trace,
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

// Single-shape reports, serialized from the same envelope structs
// the commands print.
#[test]
fn report_envelopes_serialize_as_pinned() {
    use morpholog_cli::envelopes::{
        CheckDiagnostic, CheckReport, HashReport, InitReport, LeastPrivilegeReport, NamedClaim,
    };

    assert_golden(
        "init_report.json",
        &to_value(&InitReport {
            least_privilege: None,
            schema: "morpholog",
            status: "initialised",
        }),
    );
    assert_golden(
        "init_report_least_privilege.json",
        &to_value(&InitReport {
            least_privilege: Some(LeastPrivilegeReport::applied()),
            schema: "morpholog",
            status: "initialised",
        }),
    );
    assert_golden(
        "hash_report.json",
        &to_value(&HashReport {
            hash: format!("sha256:{}", "0".repeat(64)),
            program: "envelopes".to_string(),
        }),
    );
    {
        use morpholog_cli::envelopes::{SessionErrorCode, SessionErrorReceipt, SessionReady};
        // The version is stamped from the crate at serialization time,
        // which would rot the golden on every release; pin the value
        // shape with the field overridden to a fixed string.
        let mut ready = to_value(&SessionReady::new(
            format!("sha256:{}", "0".repeat(64)),
            "envelopes".to_string(),
        ));
        ready["morpholog_version"] = serde_json::json!("0.0.0");
        assert_golden("session_ready.json", &ready);
        assert_golden(
            "session_error_receipt.json",
            &to_value(&SessionErrorReceipt::new(
                SessionErrorCode::SerializationFailure,
                "the proposal could not be decided: restart the transaction".to_string(),
                17,
            )),
        );
    }
    assert_golden(
        "check_report.json",
        &to_value(&CheckReport {
            diagnostics: vec![CheckDiagnostic {
                column: Some(1),
                end: Some(447),
                line: Some(19),
                message: "undeclared predicate `Ghost` referenced in invariant `cap`".to_string(),
                severity: "error",
                start: Some(412),
            }],
            file: "model.morph".to_string(),
        }),
    );
    assert_golden(
        "named_claim.json",
        &to_value(&NamedClaim {
            args: [
                ("trade".to_string(), serde_json::json!("trade_1")),
                ("settled_qty".to_string(), serde_json::json!("5000")),
                ("flagged".to_string(), serde_json::json!(false)),
            ]
            .into_iter()
            .collect(),
            predicate: "TradeSettled".into(),
        }),
    );
}

// The two array shapes `inspect claims` and `inspect derived` print:
// the whole stdout is the pinned value, not just each row.
#[test]
fn claim_arrays_serialize_as_pinned() {
    use morpholog_cli::envelopes::NamedClaim;

    // Direct serialization, not Value-normalized: the commands print
    // these Vecs straight through print_json, so the golden must carry
    // the structs' true wire order (`predicate` before `args`).
    assert_golden_bytes("claim_instances.json", &vec![kitchen_sink_claim()]);
    assert_golden_bytes(
        "named_claims.json",
        &vec![NamedClaim {
            args: [
                ("trade".to_string(), serde_json::json!("trade_1")),
                ("quantity".to_string(), serde_json::json!("100.5")),
            ]
            .into_iter()
            .collect(),
            predicate: "TradeCaptured".into(),
        }],
    );
}

// `refresh derived`: the snapshot pair is present or absent together
// (it comes from one audit row), and the schema + validator hold the
// pair constraint, not just this serialization.
#[test]
fn refresh_derived_reports_serialize_as_pinned() {
    use morpholog_cli::envelopes::RefreshDerivedReport;

    let committed_at = chrono::DateTime::parse_from_rfc3339("2026-06-01T12:00:00Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    assert_golden(
        "refresh_derived_report.json",
        &to_value(&RefreshDerivedReport {
            derived_claim_count: 4,
            derived_predicate_count: 1,
            model_hash: format!("sha256:{}", "0".repeat(64)),
            refresh_id: sample_uuid(),
            source_claim_count: 12,
            source_snapshot_committed_at: Some(committed_at),
            source_snapshot_transition_id: Some(sample_uuid()),
        }),
    );
    assert_golden(
        "refresh_derived_report_no_transitions.json",
        &to_value(&RefreshDerivedReport {
            derived_claim_count: 0,
            derived_predicate_count: 1,
            model_hash: format!("sha256:{}", "0".repeat(64)),
            refresh_id: sample_uuid(),
            source_claim_count: 0,
            source_snapshot_committed_at: None,
            source_snapshot_transition_id: None,
        }),
    );
}

// A one-sided snapshot pair is unrepresentable on the wire, and the
// schema layer agrees: dependentRequired refuses it, and the validator
// actually enforces dependentRequired.
#[test]
fn refresh_derived_report_rejects_one_sided_snapshot() {
    let schema = result_schema();
    let defs = schema.get("$defs").unwrap();
    let entry = defs.get("refresh_derived_report").unwrap();
    let one_sided = serde_json::json!({
        "derived_claim_count": 1,
        "derived_predicate_count": 1,
        "model_hash": format!("sha256:{}", "0".repeat(64)),
        "refresh_id": "01900000-0000-7000-8000-000000000001",
        "source_claim_count": 1,
        "source_snapshot_transition_id": "01900000-0000-7000-8000-000000000002"
    });
    let err = validate(&one_sided, entry, defs, "one_sided").unwrap_err();
    assert!(err.contains("dependentRequired"), "unexpected error: {err}");
}

// The evaluate score reports, byte-pinned as the CLI prints them
// (direct struct serialization, no Value normalization): the discovery
// harness consumes this stdout by subprocess, so the pin covers the
// true wire order - including `CaseResult`'s flatten.
#[test]
fn score_reports_serialize_as_pinned() {
    let candidate = program("candidate")
        .invariants(vec![invariant(
            "no_flagged",
            not(claim("Flagged", vec![var("x")])),
        )])
        .build();
    let flagged = State::from_claims(vec![ClaimInstance {
        predicate: "Flagged".into(),
        args: vec![EvalValue::Subject(Subject::from("acct_1"))],
    }]);
    let empty = State::from_claims(vec![]);
    let t1 = "01900000-0000-7000-8000-000000000001";
    let t2 = "01900000-0000-7000-8000-000000000002";

    let mut scorer = CandidateScorer::new(&candidate).unwrap();
    scorer.observe(&flagged, &empty, t1).unwrap();
    let report = scorer.into_report();
    assert_golden_bytes("score_report.json", &report);

    let mut scorer = CandidateScorer::new(&candidate).unwrap();
    scorer.observe(&flagged, &empty, t1).unwrap();
    scorer.mark_split(SplitBoundaryReport {
        requested: "2026-06-01T12:00:00+00:00".to_string(),
        resolved_transition_id: t1.to_string(),
        resolved_committed_at: "2026-06-01T12:00:00+00:00".to_string(),
    });
    scorer.observe(&empty, &flagged, t2).unwrap();
    assert_golden_bytes("score_report_split.json", &scorer.into_report());

    let batch = BatchScore {
        score_format_version: report.score_format_version,
        semantics: report.semantics.clone(),
        program: report.program.clone(),
        program_hash: report.program_hash.clone(),
        cases: vec![
            CaseResult {
                pack: "case_1.json".to_string(),
                outcome: CaseOutcome::Scored {
                    transitions_replayed: report.transitions_replayed,
                    invariants: report.invariants.clone(),
                },
            },
            CaseResult {
                pack: "case_2.json".to_string(),
                outcome: CaseOutcome::Failed {
                    error: "refusing to score: the evidence pack does not verify \
                            as intact (run `audit verify-pack` for the verdict)"
                        .to_string(),
                },
            },
        ],
    };
    assert_golden_bytes("batch_score.json", &batch);
}

// The tamper-evidence family: the envelopes of `verify`, `checkpoint`,
// and `audit export`/`verify-pack`. One golden per serialized variant so
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
        attestation: None,
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
            views: None,
        }),
    );
    // With the opt-in views leg: one golden per verdict shape.
    assert_golden(
        "verify_report_with_views.json",
        &to_value(&VerifyReport {
            replay: VerifyOutcome::Consistent {
                transitions: 2,
                claims: 3,
            },
            tree: TreeVerification::Intact {
                checkpoints: 1,
                tree_size: 2,
            },
            views: Some(ViewsVerification::Intact { views_checked: 4 }),
        }),
    );
    assert_golden(
        "views_verification_intact.json",
        &to_value(&ViewsVerification::Intact { views_checked: 4 }),
    );
    assert_golden(
        "views_verification_tampered.json",
        &to_value(&ViewsVerification::Tampered {
            mismatched: vec!["trade_captured".to_string()],
            missing: vec!["_morpholog_catalog".to_string()],
        }),
    );
    assert_golden(
        "views_verification_not_sealed.json",
        &to_value(&ViewsVerification::NotSealed),
    );
    assert_golden(
        "verify_report_divergent.json",
        &to_value(&VerifyReport {
            replay: VerifyOutcome::Divergent {
                only_in_claims_table: vec![kitchen_sink_claim()],
                only_in_replay: vec![],
            },
            views: None,
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

    // `audit export`: the portable pack.
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

    // `audit verify-pack`: the remaining tree verdicts (intact and tampered
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

    // `evidence export --from-*`: the windowed pack (v2).
    assert_golden(
        "window_evidence_pack.json",
        &to_value(&WindowEvidencePack {
            manifest: WindowPackManifest {
                pack_format_version: 2,
                pack_kind: "window".into(),
                from_tree_size: 2,
                to_tree_size: 3,
                from_checkpoint_hash: format!("sha256:{}", "b".repeat(64)),
                to_checkpoint_hash: format!("sha256:{}", "d".repeat(64)),
                from_root_hash: format!("sha256:{}", "a".repeat(64)),
                to_root_hash: format!("sha256:{}", "c".repeat(64)),
            },
            from_checkpoint: sample_checkpoint(),
            to_checkpoint: Checkpoint {
                tree_size: 3,
                root_hash: format!("sha256:{}", "c".repeat(64)),
                prev_checkpoint_hash: Some(format!("sha256:{}", "b".repeat(64))),
                checkpoint_hash: format!("sha256:{}", "d".repeat(64)),
                signatures: Vec::new(),
            },
            consistency_proof: vec![format!("sha256:{}", "e".repeat(64))],
            rows: vec![sample_audit_row()],
            inclusion_proofs: vec![RowInclusionProof {
                leaf_index: 2,
                proof: vec![format!("sha256:{}", "f".repeat(64))],
            }],
        }),
    );

    // `audit verify-pack` window verdicts.
    assert_golden(
        "window_verification_intact.json",
        &to_value(&WindowVerification::Intact {
            from_tree_size: 2,
            to_tree_size: 3,
            rows: 1,
        }),
    );
    assert_golden(
        "window_verification_inconsistent_extension.json",
        &to_value(&WindowVerification::InconsistentExtension {
            from_tree_size: 2,
            to_tree_size: 3,
        }),
    );
    assert_golden(
        "window_verification_row_not_included.json",
        &to_value(&WindowVerification::RowNotIncluded { leaf_index: 2 }),
    );
    assert_golden(
        "window_verification_anchor_mismatch.json",
        &to_value(&WindowVerification::AnchorMismatch {
            tree_size: 2,
            anchor_checkpoint_hash: format!("sha256:{}", "b".repeat(64)),
            pack_checkpoint_hash: format!("sha256:{}", "d".repeat(64)),
        }),
    );
    assert_golden(
        "window_verification_signature_invalid.json",
        &to_value(&WindowVerification::SignatureInvalid {
            tree_size: 3,
            key_id: "audit-2026-q3".into(),
            purpose: "audit_checkpoint_v1".into(),
            public_key: format!("ed25519-pub:{}", "c".repeat(64)),
        }),
    );
    assert_golden(
        "window_verification_signature_required.json",
        &to_value(&WindowVerification::SignatureRequired { tree_size: 3 }),
    );
    assert_golden(
        "window_verification_malformed.json",
        &to_value(&WindowVerification::Malformed {
            detail: "window covers 2 rows but the pack carries 1".into(),
        }),
    );

    // The selective pack: a chosen subset, each row proven at its position.
    assert_golden(
        "selective_evidence_pack.json",
        &to_value(&SelectiveEvidencePack {
            manifest: SelectivePackManifest {
                pack_format_version: 3,
                pack_kind: "selective".into(),
                tree_size: 3,
                root_hash: format!("sha256:{}", "c".repeat(64)),
                checkpoint_hash: format!("sha256:{}", "d".repeat(64)),
            },
            checkpoint: Checkpoint {
                tree_size: 3,
                root_hash: format!("sha256:{}", "c".repeat(64)),
                prev_checkpoint_hash: Some(format!("sha256:{}", "b".repeat(64))),
                checkpoint_hash: format!("sha256:{}", "d".repeat(64)),
                signatures: Vec::new(),
            },
            rows: vec![sample_audit_row()],
            inclusion_proofs: vec![RowInclusionProof {
                leaf_index: 1,
                proof: vec![format!("sha256:{}", "f".repeat(64))],
            }],
        }),
    );
    assert_golden(
        "selective_verification_intact.json",
        &to_value(&SelectiveVerification::Intact {
            tree_size: 3,
            rows_disclosed: 1,
        }),
    );
    assert_golden(
        "selective_verification_row_not_included.json",
        &to_value(&SelectiveVerification::RowNotIncluded { leaf_index: 1 }),
    );
    assert_golden(
        "selective_verification_anchor_mismatch.json",
        &to_value(&SelectiveVerification::AnchorMismatch {
            tree_size: 3,
            anchor_checkpoint_hash: format!("sha256:{}", "b".repeat(64)),
            pack_checkpoint_hash: format!("sha256:{}", "d".repeat(64)),
        }),
    );
    assert_golden(
        "selective_verification_signature_invalid.json",
        &to_value(&SelectiveVerification::SignatureInvalid {
            tree_size: 3,
            key_id: "audit-2026-q3".into(),
            purpose: "audit_checkpoint_v1".into(),
            public_key: format!("ed25519-pub:{}", "c".repeat(64)),
        }),
    );
    assert_golden(
        "selective_verification_signature_required.json",
        &to_value(&SelectiveVerification::SignatureRequired { tree_size: 3 }),
    );
    assert_golden(
        "selective_verification_malformed.json",
        &to_value(&SelectiveVerification::Malformed {
            detail: "a selective pack must disclose at least one row".into(),
        }),
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
        if let Some(deps) = schema.get("dependentRequired").and_then(|d| d.as_object()) {
            for (trigger, needed) in deps {
                if object.contains_key(trigger.as_str()) {
                    for key in needed
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|k| k.as_str())
                    {
                        if !object.contains_key(key) {
                            return Err(format!(
                                "{path}: `{trigger}` present without `{key}` (dependentRequired)"
                            ));
                        }
                    }
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

/// Every arm of the trace union appears in some traced golden.
///
/// The arms are read out of the schema rather than listed here, so adding
/// one without a sample reddens this test instead of quietly shipping a
/// branch no golden exercises - which is how four arms came to be described
/// by the schema and produced by nothing.
#[test]
fn every_trace_arm_appears_in_a_golden() {
    let schema = result_schema();
    let defs = schema.get("$defs").expect("$defs");

    // The discriminant values the schema declares, per union.
    let consts = |def: &str, key: &str| -> Vec<String> {
        defs.get(def)
            .and_then(|d| d.get("oneOf"))
            .and_then(|o| o.as_array())
            .expect("a discriminated union")
            .iter()
            .map(|arm| {
                arm.get("properties")
                    .and_then(|p| p.get(key))
                    .and_then(|k| k.get("const"))
                    .and_then(|c| c.as_str())
                    .expect("each arm pins its discriminant")
                    .to_string()
            })
            .collect()
    };
    let kinds = consts("trace_entry", "kind");
    let require_arms = consts("require_outcome", "status");
    let bind_arms = consts("bind_one_outcome", "status");
    assert!(
        kinds.len() > 5 && require_arms.len() == 2 && bind_arms.len() == 3,
        "anti-vacuity: the arms must be read, not silently empty"
    );

    // What the goldens actually contain, descending through `for` bodies.
    let mut seen_kinds = std::collections::BTreeSet::new();
    let mut seen_statuses = std::collections::BTreeSet::new();
    fn walk(
        entries: &[serde_json::Value],
        kinds: &mut std::collections::BTreeSet<String>,
        statuses: &mut std::collections::BTreeSet<String>,
    ) {
        for e in entries {
            if let Some(k) = e.get("kind").and_then(|k| k.as_str()) {
                kinds.insert(k.to_string());
            }
            if let Some(st) = e
                .get("outcome")
                .and_then(|o| o.get("status"))
                .and_then(|s| s.as_str())
            {
                statuses.insert(st.to_string());
            }
            if let Some(iters) = e.get("iterations").and_then(|i| i.as_array()) {
                for it in iters {
                    if let Some(sub) = it.get("trace").and_then(|t| t.as_array()) {
                        walk(sub, kinds, statuses);
                    }
                }
            }
        }
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/envelopes");
    let mut traced_goldens = 0;
    for entry in std::fs::read_dir(&dir).expect("golden dir") {
        let path = entry.expect("dir entry").path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if !name.starts_with("traced_") {
            continue;
        }
        traced_goldens += 1;
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).expect("read")).expect("json");
        if let Some(trace) = value.get("trace").and_then(|t| t.as_array()) {
            walk(trace, &mut seen_kinds, &mut seen_statuses);
        }
    }
    assert!(
        traced_goldens > 1,
        "anti-vacuity: traced goldens must exist"
    );

    for kind in kinds {
        assert!(
            seen_kinds.contains(&kind),
            "no traced golden contains a `{kind}` step; the schema describes an arm \
             nothing produced. Add a real run that reaches it."
        );
    }
    for status in require_arms.into_iter().chain(bind_arms) {
        assert!(
            seen_statuses.contains(&status),
            "no traced golden contains a `{status}` outcome; add a real run that reaches it."
        );
    }
}

/// No golden carries an empty `witness`, because absence and emptiness must
/// not both be sayable.
///
/// The prose promises "absent, never `[]`" - absence means nothing was
/// captured, while `[]` would claim the rule was reading nothing, which is
/// never true of a refusal. `skip_serializing_if` is what delivers that and
/// the schema's `minItems` states it, but the walker here ignores `minItems`
/// deliberately, so without this check the promise would rest on a keyword
/// nothing in the repo enforces.
/// A gate refusal carrying an invariant version, or a witness, is not a row
/// the writer can produce - and the pin must say so.
///
/// A flat object with both fields independently optional validated those
/// combinations happily, which left the pinned client unable to notice a
/// serializer regression that attached invariant-only evidence to a gate.
#[test]
fn a_gate_row_cannot_carry_invariant_only_fields() {
    let schema = result_schema();
    let defs = schema.get("$defs").expect("$defs");
    let def = defs.get("rejection_row").expect("rejection_row");
    let mut gate: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(golden_dir().join("rejection_row_gate.json")).unwrap(),
    )
    .unwrap();
    // Sanity: the untampered gate row validates, or the refusals below prove
    // nothing about the fields and everything about a broken fixture.
    validate(&gate, def, defs, "rejection_row").expect("the gate golden validates as it stands");

    gate["invariant_version"] = serde_json::json!(4);
    assert!(
        validate(&gate, def, defs, "rejection_row").is_err(),
        "a gate refusal with an invariant version must not validate"
    );

    let mut gate: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(golden_dir().join("rejection_row_gate.json")).unwrap(),
    )
    .unwrap();
    gate["witness"] =
        serde_json::json!([{ "var": "x", "value": { "type": "subject", "value": "a" } }]);
    assert!(
        validate(&gate, def, defs, "rejection_row").is_err(),
        "a gate refusal with a witness must not validate"
    );
}

#[test]
fn no_golden_carries_an_empty_witness() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/envelopes");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("golden dir") {
        let path = entry.expect("dir entry").path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let text = std::fs::read_to_string(&path).expect("read");
        let value: serde_json::Value = serde_json::from_str(&text).expect("json");
        fn scan(v: &serde_json::Value, name: &str, checked: &mut usize) {
            match v {
                serde_json::Value::Object(map) => {
                    if let Some(w) = map.get("witness") {
                        *checked += 1;
                        assert_ne!(
                            w.as_array().map(std::vec::Vec::len),
                            Some(0),
                            "{name} carries an empty witness; it should be absent instead"
                        );
                    }
                    for v in map.values() {
                        scan(v, name, checked);
                    }
                }
                serde_json::Value::Array(items) => {
                    for v in items {
                        scan(v, name, checked);
                    }
                }
                _ => {}
            }
        }
        scan(&value, &name, &mut checked);
    }
    assert!(
        checked > 1,
        "anti-vacuity: goldens carrying a witness must exist, found {checked}"
    );
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
        ("traced_rejected.json", "traced_envelope"),
        ("traced_rejected_at_gate.json", "traced_envelope"),
        ("traced_rejected_at_bind.json", "traced_envelope"),
        ("traced_errored.json", "traced_envelope"),
        ("batch_committed_receipt.json", "batch_receipt"),
        ("batch_rejected_receipt.json", "batch_receipt"),
        ("batch_error_receipt.json", "batch_receipt"),
        ("session_ready.json", "session_ready"),
        ("session_error_receipt.json", "session_error_receipt"),
        ("explanation_admissible.json", "explanation"),
        ("explanation_gate.json", "explanation"),
        ("explanation_gate_named.json", "explanation"),
        ("explanation_invariant.json", "explanation"),
        ("explanation_error.json", "explanation"),
        ("outbox_row.json", "outbox_row"),
        ("migration_report_behind.json", "migration_report"),
        ("migration_report_applied.json", "migration_report"),
        ("migration_report_ahead.json", "migration_report"),
        ("rejection_row.json", "rejection_row"),
        ("rejection_row_gate.json", "rejection_row"),
        ("outbox_claim.json", "outbox_claim"),
        ("outbox_claim_null.json", "outbox_claim"),
        ("outbox_update_applied.json", "outbox_update"),
        ("outbox_update_lease_lost.json", "outbox_update"),
        ("coverage_report.json", "coverage_report"),
        ("score_report.json", "score_report"),
        ("score_report_split.json", "score_report"),
        ("batch_score.json", "batch_score"),
        ("audit_row.json", "audit_row"),
        ("audit_row_attested.json", "audit_row"),
        ("audit_row_named.json", "audit_row_named"),
        ("check_report.json", "check_report"),
        ("hash_report.json", "hash_report"),
        ("init_report.json", "init_report"),
        ("init_report_least_privilege.json", "init_report"),
        ("named_claim.json", "named_claim"),
        ("claim_instances.json", "claim_instance_array"),
        ("named_claims.json", "named_claim_array"),
        ("refresh_derived_report.json", "refresh_derived_report"),
        (
            "refresh_derived_report_no_transitions.json",
            "refresh_derived_report",
        ),
        ("verify_report_consistent.json", "verify_report"),
        ("verify_report_divergent.json", "verify_report"),
        ("verify_report_with_views.json", "verify_report"),
        ("views_verification_intact.json", "views_verification"),
        ("views_verification_tampered.json", "views_verification"),
        ("views_verification_not_sealed.json", "views_verification"),
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
        ("window_evidence_pack.json", "window_evidence_pack"),
        ("selective_evidence_pack.json", "selective_evidence_pack"),
        (
            "selective_verification_intact.json",
            "selective_verification",
        ),
        (
            "selective_verification_row_not_included.json",
            "selective_verification",
        ),
        (
            "selective_verification_anchor_mismatch.json",
            "selective_verification",
        ),
        (
            "selective_verification_signature_invalid.json",
            "selective_verification",
        ),
        (
            "selective_verification_signature_required.json",
            "selective_verification",
        ),
        (
            "selective_verification_malformed.json",
            "selective_verification",
        ),
        ("window_verification_intact.json", "window_verification"),
        (
            "window_verification_inconsistent_extension.json",
            "window_verification",
        ),
        (
            "window_verification_row_not_included.json",
            "window_verification",
        ),
        (
            "window_verification_anchor_mismatch.json",
            "window_verification",
        ),
        (
            "window_verification_signature_invalid.json",
            "window_verification",
        ),
        (
            "window_verification_signature_required.json",
            "window_verification",
        ),
        ("window_verification_malformed.json", "window_verification"),
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

/// Every session error code the binary can emit must be in the pinned
/// enum. The goldens pin one code each; nothing else would notice a
/// new variant reaching an embedder that had matched exhaustively on
/// the published set.
#[test]
fn every_session_error_code_is_in_the_pinned_enum() {
    use morpholog_cli::envelopes::SessionErrorCode;
    let schema = result_schema();
    let published = schema["$defs"]["session_error_receipt"]["properties"]["code"]["enum"]
        .as_array()
        .expect("the code enum is an array")
        .iter()
        .map(|v| v.as_str().expect("codes are strings").to_string())
        .collect::<Vec<_>>();
    for code in SessionErrorCode::ALL {
        let wire = serde_json::to_value(code).expect("a code serialises");
        let wire = wire.as_str().expect("as a string");
        assert!(
            published.iter().any(|p| p == wire),
            "`{wire}` is emitted but not published in result.json; \
             an embedder matching the pinned set would not know it"
        );
    }
    assert_eq!(
        published.len(),
        SessionErrorCode::ALL.len(),
        "result.json publishes codes the binary cannot emit: {published:?}"
    );
}
