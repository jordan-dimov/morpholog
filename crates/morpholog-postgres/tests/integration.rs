//! Integration tests for `propose_against_pg`.
//!
//! Require a running PostgreSQL 17 with the `morpholog` schema applied
//! (`crates/morpholog-core/sql/schema.sql`). The connection string comes
//! from `DATABASE_URL`; tests panic if it is unset.
//!
//! Each test `reset_db`s (TRUNCATE) before its scenario. Run with
//! `--test-threads=1` so tests do not race on the shared schema.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::ir_builder::transformation;
use morpholog_core::{
    ClaimInstance, EvalValue, IntentInstance, Stmt, Subject, Term, Transformation,
};
use morpholog_examples::{
    approval_controls, double_entry_ledger, insurance_claim_settlement, settlement_netting,
    verified_revenue,
};
use morpholog_postgres::{
    PgProposalOutcome, PgTracedOutcome, compute_idempotency_key, list_audit_rows, list_claims,
    list_derived, list_pending_outbox, load_scoped_state,
};
use sqlx::PgPool;
use uuid::Uuid;

mod common;
use common::{claim_instance, dec, intent_instance, subj};

// ============================================================
// Test infrastructure
// ============================================================

async fn test_pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must be set for morpholog-postgres integration tests \
         (e.g. postgres:///morpholog_dev or postgres://postgres:postgres@localhost:5432/postgres)",
    );
    PgPool::connect(&url)
        .await
        .expect("failed to connect to PostgreSQL test database")
}

async fn reset_db(pool: &PgPool) {
    sqlx::query("TRUNCATE morpholog.outbox, morpholog.claims, morpholog.audit CASCADE")
        .execute(pool)
        .await
        .expect("failed to truncate test DB");
}

async fn insert_pre_state(pool: &PgPool, claims: Vec<ClaimInstance>) {
    // Pre-state claims need a non-null `asserted_in`; a fixed nil UUID
    // makes fixture rows identifiable and carries no semantic meaning.
    let fixture_transition = Uuid::nil();
    for claim in claims {
        let args_json = serde_json::to_value(&claim.args).unwrap();
        sqlx::query(
            "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
             VALUES ($1, $2, $3)",
        )
        .bind(claim.predicate.as_str())
        .bind(&args_json)
        .bind(fixture_transition)
        .execute(pool)
        .await
        .unwrap();
    }
}

fn claim(predicate: &str, args: Vec<EvalValue>) -> ClaimInstance {
    claim_instance(predicate, &args)
}

fn netting_pre_state_claims() -> Vec<ClaimInstance> {
    vec![
        claim("ApprovedSettlementLine", vec![subj("l1")]),
        claim(
            "Between",
            vec![subj("l1"), subj("party_a"), subj("party_b")],
        ),
        claim("LineAmount", vec![subj("l1"), dec(60)]),
        claim("ApprovedSettlementLine", vec![subj("l2")]),
        claim(
            "Between",
            vec![subj("l2"), subj("party_a"), subj("party_b")],
        ),
        claim("LineAmount", vec![subj("l2"), dec(40)]),
    ]
}

fn netting_args() -> Vec<EvalValue> {
    vec![
        subj("party_a"),
        subj("party_b"),
        EvalValue::Collection(vec![subj("l1"), subj("l2")]),
    ]
}

// ============================================================
// Tests
// ============================================================

#[tokio::test]
async fn settlement_netting_happy_path_commits_claims_audit_and_outbox() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    insert_pre_state(&pool, netting_pre_state_claims()).await;

    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &settlement_netting::create_net_settlement(),
        netting_args(),
        &settlement_netting::all_invariants(),
    )
    .await
    .expect("propose_against_pg should not error");

    let PgProposalOutcome::Committed {
        transition_id,
        asserted_claims,
        retracted_claims,
        emitted_intents,
        ..
    } = outcome
    else {
        panic!("expected Committed, got {outcome:?}");
    };

    // Outcome shape: 1 NetSettlement + (1 SettlementLine + 1 Netted) per line = 5 asserts.
    assert_eq!(asserted_claims.len(), 5);
    assert_eq!(retracted_claims.len(), 0);
    assert_eq!(emitted_intents.len(), 1);
    assert_eq!(emitted_intents[0].name, "NetSettlementCreated");

    // DB state.
    let claim_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM morpholog.claims")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(claim_count, 11, "6 pre-state + 5 newly asserted");

    let audit_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM morpholog.audit")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(audit_count, 1);

    let audit_transition: Uuid =
        sqlx::query_scalar("SELECT transition_id FROM morpholog.audit LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(audit_transition, transition_id);

    let audit_epoch: i32 =
        sqlx::query_scalar("SELECT invariant_epoch FROM morpholog.audit LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(audit_epoch, 1);

    let outbox_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM morpholog.outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(outbox_count, 1);

    let outbox_intent_type: String =
        sqlx::query_scalar("SELECT intent_type FROM morpholog.outbox LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(outbox_intent_type, "NetSettlementCreated");
}

/// Predicate-scoped load: noise claims of a predicate the
/// transformation does not reference must not affect the outcome. The
/// PG adapter's `load_state` must scope past the `UnrelatedNoise`
/// claims so the kernel never sees them.
#[tokio::test]
async fn propose_against_pg_does_not_load_unreferenced_predicates() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    let mut claims = netting_pre_state_claims();
    for i in 0..50 {
        claims.push(claim(
            "UnrelatedNoise",
            vec![subj(&format!("noise_{i}")), dec(i as i64)],
        ));
    }
    insert_pre_state(&pool, claims).await;

    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &settlement_netting::create_net_settlement(),
        netting_args(),
        &settlement_netting::all_invariants(),
    )
    .await
    .expect("propose_against_pg should commit despite noise claims");

    // Pin the FULL outcome against the no-noise baseline: the
    // observable result must be identical to the noise-free happy
    // path, not merely Committed.
    let PgProposalOutcome::Committed {
        asserted_claims,
        retracted_claims,
        emitted_intents,
        ..
    } = outcome
    else {
        panic!("expected Committed, got {outcome:?}");
    };
    assert_eq!(
        asserted_claims.len(),
        5,
        "scoped load must produce the same 5 asserts as the no-noise baseline"
    );
    assert_eq!(retracted_claims.len(), 0);
    assert_eq!(emitted_intents.len(), 1);
    assert_eq!(emitted_intents[0].name, "NetSettlementCreated");

    // DB-side: same audit/outbox shape as the baseline, and the
    // untouched noise claims must still be present.
    let noise_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM morpholog.claims WHERE predicate_name = 'UnrelatedNoise'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(noise_count, 50, "noise claims preserved across the commit");

    let audit_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM morpholog.audit")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(audit_count, 1, "exactly one audit row, same as baseline");

    let outbox_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM morpholog.outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        outbox_count, 1,
        "exactly one outbox intent, same as baseline"
    );

    let outbox_intent_type: String =
        sqlx::query_scalar("SELECT intent_type FROM morpholog.outbox LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(outbox_intent_type, "NetSettlementCreated");
}

#[tokio::test]
async fn require_failure_writes_nothing() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // Extra Netted(l1) makes the require check fail before any staging.
    let mut claims = netting_pre_state_claims();
    claims.push(claim("Netted", vec![subj("l1")]));
    insert_pre_state(&pool, claims).await;

    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &settlement_netting::create_net_settlement(),
        netting_args(),
        &settlement_netting::all_invariants(),
    )
    .await
    .expect("propose_against_pg should not error");

    let PgProposalOutcome::Rejected { reason } = outcome else {
        panic!("expected Rejected, got {outcome:?}");
    };
    assert!(reason.contains("require"), "got reason: {reason}");

    // Pre-state had 7 claims; no audit or outbox rows should exist.
    let claim_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM morpholog.claims")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(claim_count, 7, "pre-state unchanged");

    let audit_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM morpholog.audit")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(audit_count, 0);

    let outbox_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM morpholog.outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(outbox_count, 0);
}

/// `propose_against_pg_with_trace` returns the trace alongside the
/// outcome on both Committed and Rejected paths. Pinned end-to-end
/// against the PG adapter rather than just the kernel.
#[tokio::test]
async fn propose_against_pg_with_trace_returns_trace_on_committed() {
    use morpholog_core::TraceEntry;
    let pool = test_pool().await;
    reset_db(&pool).await;
    insert_pre_state(&pool, netting_pre_state_claims()).await;

    let traced = common::propose_pg_with_trace_using_test_actor(
        &pool,
        &settlement_netting::create_net_settlement(),
        netting_args(),
        &settlement_netting::all_invariants(),
    )
    .await
    .expect("propose_against_pg_with_trace should not error");
    let PgTracedOutcome::Outcome { outcome, trace } = traced else {
        panic!("expected PgTracedOutcome::Outcome, got {traced:?}");
    };

    assert!(
        matches!(outcome, PgProposalOutcome::Committed { .. }),
        "expected Committed, got {outcome:?}"
    );
    // The trace must carry the body's statement entries (require,
    // let_new_subject, let, assert, for, emit) plus invariant checks.
    assert!(
        trace
            .iter()
            .any(|e| matches!(e, TraceEntry::Require { .. })),
        "trace should contain a Require entry: {trace:#?}"
    );
    assert!(
        trace
            .iter()
            .any(|e| matches!(e, TraceEntry::LetNewSubject { .. })),
        "trace should contain a LetNewSubject entry"
    );
    assert!(
        trace.iter().any(|e| matches!(e, TraceEntry::For { .. })),
        "trace should contain a For entry"
    );
    assert!(
        trace
            .iter()
            .any(|e| matches!(e, TraceEntry::InvariantCheck { .. })),
        "trace should contain InvariantCheck entries on the Committed path"
    );
}

/// Kernel-errored path: when the kernel raises an `EvalError` mid-run
/// (here, a multi-match `BindOne`), the trace must be preserved on
/// the `PgTracedOutcome::KernelErrored` variant rather than dropped
/// at the PG boundary. The SERIALIZABLE transaction is rolled back
/// so the connection is released and no state is admitted.
#[tokio::test]
async fn propose_against_pg_with_trace_preserves_trace_on_kernel_error() {
    use morpholog_core::{EvalError, TraceEntry};
    let pool = test_pool().await;
    reset_db(&pool).await;

    // A second LineAmount for l1 makes the For-body's
    // `bind_one(LineAmount(line, amt))` multi-match and raise EvalError.
    let mut claims = netting_pre_state_claims();
    claims.push(claim("LineAmount", vec![subj("l1"), dec(99)]));
    insert_pre_state(&pool, claims).await;

    let traced = common::propose_pg_with_trace_using_test_actor(
        &pool,
        &settlement_netting::create_net_settlement(),
        netting_args(),
        &settlement_netting::all_invariants(),
    )
    .await
    .expect("PG-layer call should not error; kernel error is in KernelErrored variant");

    let PgTracedOutcome::KernelErrored { error, trace } = traced else {
        panic!("expected PgTracedOutcome::KernelErrored, got {traced:?}");
    };
    assert!(
        matches!(error, EvalError::TypeMismatch(_)),
        "expected TypeMismatch (bind_one multi-match), got {error:?}"
    );
    // Trace must be non-empty: the require held and statements ran up
    // to the For body's bind_one trip on iteration 0.
    assert!(!trace.is_empty(), "trace must not be empty on kernel error");
    // Walk into the For to pin the MultipleMatches BindOne shape, not
    // merely that a For entry exists.
    use morpholog_core::{BindOneOutcome, ForIterationTrace};
    let saw_multi_match = trace.iter().any(|e| match e {
        TraceEntry::For { iterations, .. } => iterations.iter().any(|iter: &ForIterationTrace| {
            iter.trace.iter().any(|inner| {
                matches!(
                    inner,
                    TraceEntry::BindOne {
                        outcome: BindOneOutcome::MultipleMatches { .. },
                        ..
                    }
                )
            })
        }),
        _ => false,
    });
    assert!(
        saw_multi_match,
        "trace should contain a BindOne::MultipleMatches entry inside a For iteration; got: {trace:#?}"
    );

    // No commit happened.
    let claim_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM morpholog.claims")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        claim_count,
        netting_pre_state_claims().len() as i64 + 1,
        "no claims should be admitted on kernel error; pre-state preserved"
    );
    let audit_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM morpholog.audit")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(audit_count, 0, "no audit row on kernel error");
}

/// On the Rejected path, the trace must include the failing require so
/// callers can identify which gate fired.
#[tokio::test]
async fn propose_against_pg_with_trace_returns_trace_on_rejected() {
    use morpholog_core::{RequireOutcome, TraceEntry};
    let pool = test_pool().await;
    reset_db(&pool).await;

    // Extra Netted(l1) makes the forall require's `not Netted` fail.
    let mut claims = netting_pre_state_claims();
    claims.push(claim("Netted", vec![subj("l1")]));
    insert_pre_state(&pool, claims).await;

    let traced = common::propose_pg_with_trace_using_test_actor(
        &pool,
        &settlement_netting::create_net_settlement(),
        netting_args(),
        &settlement_netting::all_invariants(),
    )
    .await
    .expect("propose_against_pg_with_trace should not error");
    let PgTracedOutcome::Outcome { outcome, trace } = traced else {
        panic!("expected PgTracedOutcome::Outcome, got {traced:?}");
    };

    assert!(
        matches!(outcome, PgProposalOutcome::Rejected { .. }),
        "expected Rejected, got {outcome:?}"
    );
    let failing_require = trace.iter().find_map(|e| match e {
        TraceEntry::Require {
            expression,
            outcome: RequireOutcome::Rejected { .. },
        } => Some(expression),
        _ => None,
    });
    assert!(
        failing_require.is_some(),
        "trace should record the failing require; got: {trace:#?}"
    );
}

#[tokio::test]
async fn invariant_violation_on_candidate_state_writes_nothing() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // Orphan SettlementLine for l1 (legacy data: in another net but not
    // flagged Netted). `require not Netted(l1)` passes, but the
    // candidate state would violate no_double_netting.
    let mut claims = netting_pre_state_claims();
    claims.push(claim(
        "SettlementLine",
        vec![subj("l1"), subj("old_net"), dec(60)],
    ));
    insert_pre_state(&pool, claims).await;

    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &settlement_netting::create_net_settlement(),
        netting_args(),
        &settlement_netting::all_invariants(),
    )
    .await
    .expect("propose_against_pg should not error");

    let PgProposalOutcome::Rejected { reason } = outcome else {
        panic!("expected Rejected, got {outcome:?}");
    };
    assert!(reason.contains("no_double_netting"), "got reason: {reason}");

    // Pre-state had 7 claims; no audit or outbox rows should exist.
    let claim_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM morpholog.claims")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(claim_count, 7, "pre-state unchanged");

    let audit_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM morpholog.audit")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(audit_count, 0);

    let outbox_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM morpholog.outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(outbox_count, 0);
}

#[test]
fn idempotency_key_matches_golden_hash() {
    // Pin the exact formula:
    //     hex(sha256(transition_id_bytes || 0x00 || name_bytes || 0x00 || canonical_json(args)))
    // with transition_id = nil, name = "TestIntent", args =
    // [Subject("net1")] -> canonical_json `[{"type":"subject","value":"net1"}]`.
    //
    // The expected hex was computed independently (Python hashlib).
    // Do NOT recompute via the production helper - the point is to
    // catch formula drift, including delimiter changes.
    let transition_id = Uuid::nil();
    let intent = intent_instance("TestIntent", &[EvalValue::Subject("net1".into())]);

    let expected = "c32fd9040f866912cfc522571e851ee6240c9e5d19a39db9e50ac7834fd2341f";
    let actual = compute_idempotency_key(transition_id, &intent).unwrap();
    assert_eq!(actual, expected);
}

fn retract_marker_transformation() -> Transformation {
    let var = |s: &str| Term::Var(s.into());
    transformation(
        "retract_marker",
        vec!["subject".into()],
        vec![Stmt::Retract {
            predicate: "Marker".into(),
            args: vec![var("subject")],
        }],
    )
}

#[tokio::test]
async fn retraction_deletes_targeted_row_and_preserves_others() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    insert_pre_state(
        &pool,
        vec![
            claim("Marker", vec![subj("x")]),
            claim("Marker", vec![subj("y")]),
            claim("Marker", vec![subj("z")]),
        ],
    )
    .await;

    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &retract_marker_transformation(),
        vec![subj("y")],
        &[],
    )
    .await
    .expect("propose_against_pg should not error");

    let PgProposalOutcome::Committed {
        asserted_claims,
        retracted_claims,
        emitted_intents,
        ..
    } = outcome
    else {
        panic!("expected Committed, got {outcome:?}");
    };

    assert!(asserted_claims.is_empty());
    assert_eq!(retracted_claims.len(), 1);
    assert_eq!(retracted_claims[0].predicate.as_str(), "Marker");
    assert_eq!(retracted_claims[0].args, vec![subj("y")]);
    assert!(emitted_intents.is_empty());

    // Only Marker(y) should have been deleted.
    let remaining: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT predicate_name, arguments FROM morpholog.claims
         WHERE predicate_name = 'Marker' ORDER BY arguments::text",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        remaining.len(),
        2,
        "Marker(y) deleted, Marker(x) and Marker(z) preserved"
    );

    let preserved_args: Vec<EvalValue> = remaining
        .iter()
        .map(|(_, v)| serde_json::from_value(v.clone()).unwrap())
        .map(|args: Vec<EvalValue>| args.into_iter().next().unwrap())
        .collect();
    assert!(preserved_args.contains(&subj("x")));
    assert!(preserved_args.contains(&subj("z")));
    assert!(!preserved_args.contains(&subj("y")));
}

#[tokio::test]
async fn audit_jsonb_columns_round_trip_through_codec() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    insert_pre_state(&pool, netting_pre_state_claims()).await;

    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &settlement_netting::create_net_settlement(),
        netting_args(),
        &settlement_netting::all_invariants(),
    )
    .await
    .expect("propose_against_pg should not error");

    let PgProposalOutcome::Committed { transition_id, .. } = outcome else {
        panic!("expected Committed");
    };

    // Verify every audit JSONB column decodes through the codec back
    // into Rust types with the expected shapes.
    type AuditJsonRow = (
        serde_json::Value,
        serde_json::Value,
        serde_json::Value,
        serde_json::Value,
        serde_json::Value,
        serde_json::Value,
    );
    let (args_json, actor_json, invariants_checked_json, asserted_json, retracted_json, intents_json): AuditJsonRow = sqlx::query_as(
        "SELECT arguments, actor, invariants_checked, asserted_claims, retracted_claims, emitted_intents
         FROM morpholog.audit WHERE transition_id = $1",
    )
    .bind(transition_id)
    .fetch_one(&pool)
    .await
    .unwrap();

    // arguments: Vec<EvalValue>
    let args: Vec<EvalValue> = serde_json::from_value(args_json).unwrap();
    assert_eq!(args.len(), 3);
    assert_eq!(args[0], subj("party_a"));

    // actor: EvalValue (Subject under the v0 contract)
    let actor: EvalValue = serde_json::from_value(actor_json).unwrap();
    assert_eq!(actor, subj("test_actor"));

    // invariants_checked: [{name, version}, ...]
    let checked = invariants_checked_json.as_array().unwrap();
    assert_eq!(checked.len(), 3);
    for entry in checked {
        assert!(entry.get("name").and_then(|v| v.as_str()).is_some());
        assert_eq!(
            entry.get("version").and_then(serde_json::Value::as_u64),
            Some(1)
        );
    }

    // asserted_claims: Vec<ClaimInstance> (objects with predicate + args)
    let asserted: Vec<ClaimInstance> = serde_json::from_value(asserted_json).unwrap();
    assert_eq!(asserted.len(), 5);

    // retracted_claims: Vec<ClaimInstance> (empty here)
    let retracted: Vec<ClaimInstance> = serde_json::from_value(retracted_json).unwrap();
    assert!(retracted.is_empty());

    // emitted_intents: Vec<IntentInstance>
    let intents: Vec<IntentInstance> = serde_json::from_value(intents_json).unwrap();
    assert_eq!(intents.len(), 1);
    assert_eq!(intents[0].name, "NetSettlementCreated");
}

// ============================================================
// Shared helpers for the verified-revenue PG tests below.
// ============================================================

fn asset() -> EvalValue {
    subj("asset_a")
}

fn period() -> EvalValue {
    subj("p_2026_04")
}

async fn count(pool: &PgPool, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT COUNT(*) FROM morpholog.{table}"))
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn claim_exists(pool: &PgPool, predicate: &str, args: &[EvalValue]) -> bool {
    let args_json = serde_json::to_value(args).unwrap();
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM morpholog.claims
         WHERE predicate_name = $1 AND arguments = $2",
    )
    .bind(predicate)
    .bind(args_json)
    .fetch_one(pool)
    .await
    .unwrap();
    n > 0
}

// ============================================================
// Verified revenue - durable proof that currentness-with-restatement
// and admissibility-for-purpose compose end to end through
// propose_against_pg. One scenario walks admission, multi-authority
// standing, decisions, correction (retracting standing on the prior
// verification), rejection of unstanding decisions, and re-grant on
// the corrected figure.
// ============================================================

#[tokio::test]
async fn verified_revenue_full_chain_through_pg() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let invariants = verified_revenue::all_invariants();
    let bank = subj(verified_revenue::BANK_DEBT_SERVICE);
    let investor = subj(verified_revenue::INVESTOR_REPORTING);

    // 1. Verifier admits IV at 91 with ver_001. CurrentVerification
    //    pointer is established.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &verified_revenue::admit_independent_verification(),
        vec![asset(), period(), dec(91), subj("ver_001")],
        &invariants,
    )
    .await
    .expect("step 1 propose_against_pg should not error");
    assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));

    // 2. Bank credit committee grants debt-service standing on ver_001.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &verified_revenue::grant_standing(),
        vec![
            subj("ver_001"),
            bank.clone(),
            subj("credit_committee"),
            subj("grant_bank_001"),
        ],
        &invariants,
    )
    .await
    .expect("step 2 propose_against_pg should not error");
    assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));

    // 3. Bank admits a debt-service decision against ver_001.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &verified_revenue::admit_debt_service_revenue(),
        vec![
            asset(),
            period(),
            dec(91),
            subj("decision_001"),
            subj("ver_001"),
        ],
        &invariants,
    )
    .await
    .expect("step 3 propose_against_pg should not error");
    assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));

    // 4. Investor relations office grants investor-reporting standing
    //    on the same verification - parallel admissibility.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &verified_revenue::grant_standing(),
        vec![
            subj("ver_001"),
            investor.clone(),
            subj("investor_relations_office"),
            subj("grant_inv_001"),
        ],
        &invariants,
    )
    .await
    .expect("step 4 propose_against_pg should not error");
    assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));

    // 5. Investor admits investor-reporting decision against ver_001.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &verified_revenue::admit_investor_reported_revenue(),
        vec![
            asset(),
            period(),
            dec(91),
            subj("report_001"),
            subj("ver_001"),
        ],
        &invariants,
    )
    .await
    .expect("step 5 propose_against_pg should not error");
    assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));

    // 6. Verifier corrects to 88 (ver_002 supersedes ver_001): asserts
    //    new IV + Supersedes lineage; retracts the CurrentVerification
    //    pointer and every AdmissibleFor on ver_001. The original IV
    //    and historical decisions stay admitted.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &verified_revenue::correct_independent_verification(),
        vec![asset(), period(), dec(88), subj("ver_002"), subj("ver_001")],
        &invariants,
    )
    .await
    .expect("step 6 propose_against_pg should not error");
    let PgProposalOutcome::Committed {
        retracted_claims, ..
    } = outcome
    else {
        panic!("step 6 expected Committed, got {outcome:?}");
    };
    // Three retractions: CurrentVerification(ver_001), AdmissibleFor(bank),
    // AdmissibleFor(investor).
    assert_eq!(retracted_claims.len(), 3);

    // Restatement lineage durably recorded; original IV preserved.
    assert!(
        claim_exists(
            &pool,
            "IndependentlyVerifiedRevenue",
            &[asset(), period(), dec(91), subj("ver_001")],
        )
        .await,
        "historical IV must be preserved",
    );
    assert!(
        claim_exists(
            &pool,
            "IndependentlyVerifiedRevenue",
            &[asset(), period(), dec(88), subj("ver_002")],
        )
        .await
    );
    assert!(claim_exists(&pool, "Supersedes", &[subj("ver_002"), subj("ver_001")]).await);

    // Current pointer moved to ver_002.
    assert!(
        claim_exists(
            &pool,
            "CurrentVerification",
            &[asset(), period(), subj("ver_002")],
        )
        .await
    );
    assert!(
        !claim_exists(
            &pool,
            "CurrentVerification",
            &[asset(), period(), subj("ver_001")],
        )
        .await
    );

    // Both standings on ver_001 are retracted; both grant provenances
    // persist as historical record.
    assert!(!claim_exists(&pool, "AdmissibleFor", &[subj("ver_001"), bank.clone()]).await);
    assert!(!claim_exists(&pool, "AdmissibleFor", &[subj("ver_001"), investor.clone()]).await);
    assert!(
        claim_exists(
            &pool,
            "StandingGrantedBy",
            &[
                subj("ver_001"),
                bank.clone(),
                subj("credit_committee"),
                subj("grant_bank_001"),
            ],
        )
        .await,
        "grant provenance survives correction",
    );

    // The historical decisions both survive.
    assert!(
        claim_exists(
            &pool,
            "DebtServiceRevenue",
            &[
                asset(),
                period(),
                dec(91),
                subj("decision_001"),
                subj("ver_001"),
            ],
        )
        .await,
        "the bank's historical decision must survive correction",
    );
    assert!(
        claim_exists(
            &pool,
            "InvestorReportedRevenue",
            &[
                asset(),
                period(),
                dec(91),
                subj("report_001"),
                subj("ver_001"),
            ],
        )
        .await
    );

    // 7. A new bank decision against ver_002 is rejected (no standing
    //    on the corrected figure yet). Pin no durable trace: claims,
    //    audit, and outbox must all be unchanged.
    let claims_before = list_claims(&pool).await.unwrap().len();
    let audit_before = list_audit_rows(&pool).await.unwrap().len();
    let outbox_before = list_pending_outbox(&pool).await.unwrap().len();
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &verified_revenue::admit_debt_service_revenue(),
        vec![
            asset(),
            period(),
            dec(88),
            subj("decision_002"),
            subj("ver_002"),
        ],
        &invariants,
    )
    .await
    .expect("step 7 propose_against_pg should not error");
    assert!(matches!(outcome, PgProposalOutcome::Rejected { .. }));
    assert_eq!(
        list_claims(&pool).await.unwrap().len(),
        claims_before,
        "rejected proposal must not mutate the claim set",
    );
    assert_eq!(
        list_audit_rows(&pool).await.unwrap().len(),
        audit_before,
        "rejected proposal must not write an audit row",
    );
    assert_eq!(
        list_pending_outbox(&pool).await.unwrap().len(),
        outbox_before,
        "rejected proposal must not enqueue an outbox intent",
    );

    // 8. Bank re-grants standing on the corrected ver_002; the new
    //    decision now admits.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &verified_revenue::grant_standing(),
        vec![
            subj("ver_002"),
            bank.clone(),
            subj("credit_committee"),
            subj("grant_bank_002"),
        ],
        &invariants,
    )
    .await
    .expect("step 8 propose_against_pg should not error");
    assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));

    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &verified_revenue::admit_debt_service_revenue(),
        vec![
            asset(),
            period(),
            dec(88),
            subj("decision_002"),
            subj("ver_002"),
        ],
        &invariants,
    )
    .await
    .expect("step 8b propose_against_pg should not error");
    assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));

    // Both decisions are durably present in the final claim set - the
    // historical one against ver_001 and the new one against ver_002.
    assert!(
        claim_exists(
            &pool,
            "DebtServiceRevenue",
            &[
                asset(),
                period(),
                dec(91),
                subj("decision_001"),
                subj("ver_001"),
            ],
        )
        .await
    );
    assert!(
        claim_exists(
            &pool,
            "DebtServiceRevenue",
            &[
                asset(),
                period(),
                dec(88),
                subj("decision_002"),
                subj("ver_002"),
            ],
        )
        .await
    );
}

// ============================================================
// Double-entry ledger - durable proof of balanced posting, period
// close, and restatement through propose_against_pg, inspecting the
// durable claims, audit, and outbox rows directly.
// ============================================================

fn ledger_period() -> EvalValue {
    subj("p_2026_04")
}

#[tokio::test]
async fn double_entry_full_chain_through_pg() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    let invariants = double_entry_ledger::all_invariants();

    // 1. Post a simple entry: cash debit 100, revenue credit 100.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj("entry_001"),
            subj("d_2026_04_15"),
            ledger_period(),
            subj("account_cash"),
            subj("account_revenue"),
            dec(100),
        ],
        &invariants,
    )
    .await
    .expect("step 1 propose_against_pg should not error");
    let PgProposalOutcome::Committed {
        asserted_claims,
        emitted_intents,
        ..
    } = outcome
    else {
        panic!("step 1 expected Committed, got {outcome:?}");
    };
    assert_eq!(
        asserted_claims.len(),
        3,
        "1 JournalEntry + 2 JournalLine asserts"
    );
    assert_eq!(emitted_intents[0].name, "JournalEntryPosted");

    // 2. Close the period.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &double_entry_ledger::close_period(),
        vec![ledger_period()],
        &invariants,
    )
    .await
    .expect("step 2 propose_against_pg should not error");
    let PgProposalOutcome::Committed {
        asserted_claims,
        emitted_intents,
        ..
    } = outcome
    else {
        panic!("step 2 expected Committed, got {outcome:?}");
    };
    assert_eq!(asserted_claims.len(), 1, "1 PeriodClosed assert");
    assert_eq!(emitted_intents[0].name, "PeriodClosed");

    // 3. Restate the entry with a corrected amount (101). Restatement
    //    does not check PeriodClosed - it is the closed-period path.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &double_entry_ledger::restate_entry(),
        vec![
            subj("entry_002"),
            subj("entry_001"),
            subj("d_2026_05_10"),
            ledger_period(),
            subj("account_cash"),
            subj("account_revenue"),
            dec(101),
        ],
        &invariants,
    )
    .await
    .expect("step 3 propose_against_pg should not error");
    let PgProposalOutcome::Committed {
        asserted_claims,
        retracted_claims,
        emitted_intents,
        ..
    } = outcome
    else {
        panic!("step 3 expected Committed, got {outcome:?}");
    };
    assert_eq!(
        asserted_claims.len(),
        4,
        "new JournalEntry + 2 new JournalLine + Supersedes"
    );
    assert_eq!(
        retracted_claims.len(),
        0,
        "restatement is fully append-only (no retractable pointers)"
    );
    assert_eq!(emitted_intents[0].name, "JournalEntryRestated");

    // Final DB shape: 8 claims total.
    //   2 JournalEntry (original + restated)
    //   4 JournalLine (2 per entry)
    //   1 PeriodClosed
    //   1 Supersedes
    assert_eq!(count(&pool, "claims").await, 8);
    assert_eq!(count(&pool, "audit").await, 3);
    assert_eq!(count(&pool, "outbox").await, 3);

    // Original entry preserved (the load-bearing property).
    assert!(
        claim_exists(
            &pool,
            "JournalEntry",
            &[subj("entry_001"), subj("d_2026_04_15"), ledger_period(),],
        )
        .await,
        "original entry must remain in admitted state after restatement"
    );
    assert!(
        claim_exists(
            &pool,
            "JournalLine",
            &[subj("entry_001"), subj("account_cash"), dec(100), dec(0)],
        )
        .await,
        "original debit line must remain"
    );
    assert!(
        claim_exists(
            &pool,
            "JournalLine",
            &[subj("entry_001"), subj("account_revenue"), dec(0), dec(100)],
        )
        .await,
        "original credit line must remain"
    );

    // Restated entry present at the corrected amount.
    assert!(
        claim_exists(
            &pool,
            "JournalEntry",
            &[subj("entry_002"), subj("d_2026_05_10"), ledger_period(),],
        )
        .await
    );
    assert!(
        claim_exists(
            &pool,
            "JournalLine",
            &[subj("entry_002"), subj("account_cash"), dec(101), dec(0)],
        )
        .await
    );

    // Supersession lineage recorded.
    assert!(claim_exists(&pool, "Supersedes", &[subj("entry_002"), subj("entry_001")],).await);

    // Period still closed.
    assert!(claim_exists(&pool, "PeriodClosed", &[ledger_period()]).await);

    // Outbox carries one intent per committed transformation, in
    // causal order.
    let intent_types: Vec<String> = sqlx::query_scalar(
        "SELECT intent_type FROM morpholog.outbox ORDER BY enqueued_at, intent_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        intent_types,
        vec![
            "JournalEntryPosted".to_string(),
            "PeriodClosed".to_string(),
            "JournalEntryRestated".to_string(),
        ]
    );
}

#[tokio::test]
async fn ledger_closed_period_rejects_new_entry_and_writes_nothing() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // Pre-state: period already closed.
    insert_pre_state(&pool, vec![claim("PeriodClosed", vec![ledger_period()])]).await;

    // A normal posting must be rejected by `require not PeriodClosed`,
    // with no writes to claims, audit, or outbox.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj("entry_001"),
            subj("d_2026_04_15"),
            ledger_period(),
            subj("account_cash"),
            subj("account_revenue"),
            dec(100),
        ],
        &double_entry_ledger::all_invariants(),
    )
    .await
    .expect("propose_against_pg should not error");

    let PgProposalOutcome::Rejected { reason } = outcome else {
        panic!("expected Rejected, got {outcome:?}");
    };
    assert!(reason.contains("require"), "got reason: {reason}");

    // Pre-state had 1 claim; tables must be unchanged.
    assert_eq!(count(&pool, "claims").await, 1, "pre-state unchanged");
    assert_eq!(count(&pool, "audit").await, 0);
    assert_eq!(count(&pool, "outbox").await, 0);
}

// ============================================================
// Read API - current-state inspection helpers
//
// These tests pin the codec round-trips and stable orderings, not the
// kernel semantics, against scenarios the propose_against_pg tests
// already exercise.
// ============================================================

#[tokio::test]
async fn list_claims_returns_admitted_claims_in_stable_order() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // Post a simple journal entry: 1 JournalEntry, 2 JournalLine.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj("entry_001"),
            subj("d_2026_04_15"),
            ledger_period(),
            subj("account_cash"),
            subj("account_revenue"),
            dec(100),
        ],
        &double_entry_ledger::all_invariants(),
    )
    .await
    .expect("propose_against_pg should not error");
    assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));

    let claims = list_claims(&pool)
        .await
        .expect("list_claims should not error");

    assert_eq!(claims.len(), 3, "1 JournalEntry + 2 JournalLine");

    // All three share an asserted_at (same transition), so the
    // predicate-then-args tie-break orders them: JournalEntry before
    // JournalLine, then the two lines by `arguments::text`.
    assert_eq!(claims[0].predicate.as_str(), "JournalEntry");
    assert_eq!(claims[1].predicate.as_str(), "JournalLine");
    assert_eq!(claims[2].predicate.as_str(), "JournalLine");

    // JournalEntry's args round-trip through the codec.
    assert_eq!(
        claims[0].args,
        vec![subj("entry_001"), subj("d_2026_04_15"), ledger_period()]
    );
}

#[tokio::test]
async fn list_audit_rows_returns_committed_transformations_in_order() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // Two committed transformations in causal order: post then close.
    common::propose_pg_with_test_actor(
        &pool,
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj("entry_001"),
            subj("d_2026_04_15"),
            ledger_period(),
            subj("account_cash"),
            subj("account_revenue"),
            dec(100),
        ],
        &double_entry_ledger::all_invariants(),
    )
    .await
    .unwrap();
    common::propose_pg_with_test_actor(
        &pool,
        &double_entry_ledger::close_period(),
        vec![ledger_period()],
        &double_entry_ledger::all_invariants(),
    )
    .await
    .unwrap();

    let audit = list_audit_rows(&pool)
        .await
        .expect("list_audit_rows should not error");

    assert_eq!(audit.len(), 2);

    // Causal order: post first, close second.
    assert_eq!(audit[0].transformation_name, "post_simple_entry");
    assert_eq!(audit[1].transformation_name, "close_period");

    // JSONB columns decoded through the codec.
    let first = &audit[0];
    assert_eq!(
        first.arguments.len(),
        6,
        "post_simple_entry takes 6 parameters"
    );
    assert_eq!(first.arguments[0], subj("entry_001"));
    assert_eq!(
        first.asserted_claims.len(),
        3,
        "1 JournalEntry + 2 JournalLine"
    );
    assert_eq!(first.retracted_claims.len(), 0);
    assert_eq!(first.emitted_intents.len(), 1);
    assert_eq!(first.emitted_intents[0].name, "JournalEntryPosted");
    assert_eq!(first.invariant_epoch, 1);

    // invariants_checked: every invariant active at admission, named with
    // its version. The ledger example has three invariants.
    assert_eq!(first.invariants_checked.len(), 3);
    assert!(
        first
            .invariants_checked
            .iter()
            .any(|c| c.name == "balanced_posted_entry" && c.version == 1)
    );

    // close_period is a no-arg-shape transformation that asserts one
    // PeriodClosed claim and emits one PeriodClosed intent.
    let second = &audit[1];
    assert_eq!(second.arguments, vec![ledger_period()]);
    assert_eq!(second.asserted_claims.len(), 1);
    assert_eq!(second.asserted_claims[0].predicate.as_str(), "PeriodClosed");
    assert_eq!(second.emitted_intents[0].name, "PeriodClosed");
}

#[tokio::test]
async fn list_pending_outbox_returns_intents_in_enqueue_order() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    common::propose_pg_with_test_actor(
        &pool,
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj("entry_001"),
            subj("d_2026_04_15"),
            ledger_period(),
            subj("account_cash"),
            subj("account_revenue"),
            dec(100),
        ],
        &double_entry_ledger::all_invariants(),
    )
    .await
    .unwrap();
    common::propose_pg_with_test_actor(
        &pool,
        &double_entry_ledger::close_period(),
        vec![ledger_period()],
        &double_entry_ledger::all_invariants(),
    )
    .await
    .unwrap();

    let outbox = list_pending_outbox(&pool)
        .await
        .expect("list_pending_outbox should not error");

    assert_eq!(outbox.len(), 2);
    assert_eq!(outbox[0].intent_type, "JournalEntryPosted");
    assert_eq!(outbox[1].intent_type, "PeriodClosed");

    // Fresh-enqueue rows have `attempt_count = 0` and no
    // `last_attempt_at`; a retried row would have both set.
    for row in &outbox {
        assert_eq!(row.status, "pending");
        assert_eq!(row.attempt_count, 0);
        assert!(row.last_attempt_at.is_none());
        assert!(!row.idempotency_key.is_empty());
    }

    // arguments decode through the codec.
    assert_eq!(outbox[0].arguments, vec![subj("entry_001")]);
    assert_eq!(outbox[1].arguments, vec![ledger_period()]);
}

#[tokio::test]
async fn list_derived_trial_balance_over_pg_ledger_state() {
    // Two ledger entries through the PG adapter, then trial-balance
    // enumeration over the durable state: claims persisted by
    // `propose_against_pg` reach `list_derived`, through
    // `enumerate_derived`, to one row per account at the expected
    // debit-minus-credit balance. Pinning the full rows (not just the
    // count) catches a dropped row, a wrong balance, or unstable order.
    let pool = test_pool().await;
    reset_db(&pool).await;

    let invariants = double_entry_ledger::all_invariants();

    // Entry 1: cash debit 100, revenue credit 100.
    common::propose_pg_with_test_actor(
        &pool,
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj("entry_001"),
            subj("d_2026_04_15"),
            ledger_period(),
            subj("account_cash"),
            subj("account_revenue"),
            dec(100),
        ],
        &invariants,
    )
    .await
    .expect("entry 1 should commit");

    // Entry 2: cash debit 50, revenue credit 50. Same accounts, so the
    // two rows accumulate rather than producing four distinct rows.
    common::propose_pg_with_test_actor(
        &pool,
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj("entry_002"),
            subj("d_2026_04_16"),
            ledger_period(),
            subj("account_cash"),
            subj("account_revenue"),
            dec(50),
        ],
        &invariants,
    )
    .await
    .expect("entry 2 should commit");

    let rows = list_derived(&pool, &double_entry_ledger::trial_balance_row())
        .await
        .expect("list_derived should not error");

    // Two accounts -> two rows. Structural Subject ordering sorts
    // `account_cash` before `account_revenue`, so the order is stable.
    assert_eq!(
        rows,
        vec![
            claim_instance("TrialBalanceRow", &[subj("account_cash"), dec(150)]),
            claim_instance("TrialBalanceRow", &[subj("account_revenue"), dec(-150)]),
        ],
        "trial balance over two posted entries must list each account once \
         with debits-minus-credits balance"
    );

    // list_derived must not mutate admitted state: still exactly the
    // 6 claims the two entries asserted, no leaked TrialBalanceRow.
    let claims = list_claims(&pool).await.unwrap();
    assert_eq!(
        claims.len(),
        6,
        "list_derived must not write claims back to the table"
    );
    assert!(
        !claims
            .iter()
            .any(|c| c.predicate.as_str() == "TrialBalanceRow"),
        "derived rows must not be admitted as claims"
    );
}

#[tokio::test]
async fn list_derived_on_empty_state_returns_no_rows() {
    // With no JournalLine claims, the `domain` enumerates no key
    // bindings, so the derived extension is empty - structural (zero
    // bindings -> zero rows), not an error.
    let pool = test_pool().await;
    reset_db(&pool).await;

    let rows = list_derived(&pool, &double_entry_ledger::trial_balance_row())
        .await
        .expect("list_derived against an empty state should not error");
    assert!(
        rows.is_empty(),
        "empty domain produces no derived rows, got {rows:?}"
    );
}

#[tokio::test]
async fn list_derived_ignores_claims_outside_its_predicate_footprint() {
    // Black-box equivalence test for predicate-scoped loading.
    // `trial_balance_row` references only `JournalLine`, so under a
    // pile of unrelated `IndependentlyVerifiedRevenue` noise
    // `list_derived` must return byte-identical rows to the noise-free
    // state. If `predicates_referenced_by_derived` ever missed a
    // predicate the kernel reads, the read path would silently skip
    // needed claims and this equivalence would fail - the runtime
    // safety net complementing the analysis's compile-time exhaustive
    // `match`.
    let pool = test_pool().await;
    reset_db(&pool).await;

    // Real ledger fixture: 1 entry, 2 journal lines.
    let invariants = double_entry_ledger::all_invariants();
    common::propose_pg_with_test_actor(
        &pool,
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj("entry_001"),
            subj("d_2026_04_15"),
            ledger_period(),
            subj("account_cash"),
            subj("account_revenue"),
            dec(100),
        ],
        &invariants,
    )
    .await
    .expect("entry should commit");

    // Noise: 200 claims of `IndependentlyVerifiedRevenue`, a predicate
    // `trial_balance_row()` never references.
    let noise: Vec<ClaimInstance> = (0..200)
        .map(|i| {
            claim(
                "IndependentlyVerifiedRevenue",
                vec![
                    subj(&format!("noise_asset_{i}")),
                    subj("p_noise"),
                    dec(i),
                    subj(&format!("noise_ver_{i}")),
                ],
            )
        })
        .collect();
    insert_pre_state(&pool, noise).await;

    // Sanity: the noise is present and dominates the claims table.
    let total_claims = list_claims(&pool).await.unwrap();
    assert!(
        total_claims.len() > 200,
        "fixture sanity: noise should have been inserted alongside the ledger \
         claims, got {} total",
        total_claims.len()
    );

    let rows = list_derived(&pool, &double_entry_ledger::trial_balance_row())
        .await
        .expect("list_derived under noise should not error");

    // One cash/revenue entry yields exactly two rows; the noise must
    // not change that.
    assert_eq!(
        rows,
        vec![
            claim_instance("TrialBalanceRow", &[subj("account_cash"), dec(100)]),
            claim_instance("TrialBalanceRow", &[subj("account_revenue"), dec(-100)]),
        ],
        "predicate-scoped loading must return the same derived rows whether \
         or not unrelated noise claims are present"
    );
}

#[tokio::test]
async fn rejected_transformation_leaves_audit_and_outbox_empty() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // Period already closed: a normal posting is rejected by
    // `require not PeriodClosed`.
    insert_pre_state(&pool, vec![claim("PeriodClosed", vec![ledger_period()])]).await;

    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj("entry_001"),
            subj("d_2026_04_15"),
            ledger_period(),
            subj("account_cash"),
            subj("account_revenue"),
            dec(100),
        ],
        &double_entry_ledger::all_invariants(),
    )
    .await
    .unwrap();
    assert!(matches!(outcome, PgProposalOutcome::Rejected { .. }));

    // claims still contains only the pre-state PeriodClosed.
    let claims = list_claims(&pool).await.unwrap();
    assert_eq!(claims.len(), 1);
    assert_eq!(claims[0].predicate.as_str(), "PeriodClosed");

    // audit and outbox are empty - rejected transformations leave no
    // governed trace.
    let audit = list_audit_rows(&pool).await.unwrap();
    assert!(audit.is_empty(), "rejected transformation must not audit");

    let outbox = list_pending_outbox(&pool).await.unwrap();
    assert!(
        outbox.is_empty(),
        "rejected transformation must not enqueue intents"
    );
}

// Pins that the Transition actor round-trips unchanged through the
// kernel, the audit-write codec, and list_audit_rows(), and surfaces on
// PgProposalOutcome::Committed so the commit receipt is self-describing.
#[tokio::test]
async fn audit_row_records_actor() {
    use morpholog_core::Transition;
    use morpholog_postgres::propose_against_pg;

    let pool = test_pool().await;
    reset_db(&pool).await;

    let transformation = double_entry_ledger::post_simple_entry();
    let actor = Subject::from("user:jordan");
    let transition = Transition {
        transformation_name: transformation.name.clone(),
        args: vec![
            subj("entry_actor_test"),
            subj("d_2026_05_20"),
            subj("p_actor_test"),
            subj("account_cash"),
            subj("account_revenue"),
            dec(100),
        ],
        actor: actor.clone(),
    };

    let outcome = propose_against_pg(
        &pool,
        &transformation,
        &transition,
        &double_entry_ledger::all_invariants(),
    )
    .await
    .expect("propose_against_pg should not error");

    let PgProposalOutcome::Committed {
        transition_id,
        actor: receipt_actor,
        ..
    } = outcome
    else {
        panic!("expected Committed, got {outcome:?}");
    };

    assert_eq!(
        receipt_actor, actor,
        "Committed receipt must echo the proposed actor"
    );

    let audit_rows = list_audit_rows(&pool).await.unwrap();
    assert_eq!(audit_rows.len(), 1);
    let row = &audit_rows[0];
    assert_eq!(row.transition_id, transition_id);
    assert_eq!(
        row.actor, actor,
        "audit row must persist the actor unchanged"
    );
}

// A non-subject actor can no longer reach the kernel: `Transition.actor`
// is a `Subject` by type, so the old "actor must be a subject" runtime
// check is gone. The one place a non-subject actor could still enter is
// the IO boundary - the audit `actor` column - so the read path validates
// the tag there. This pins that surviving guarantee: an audit row whose
// `actor` JSONB is not a tagged subject surfaces as a typed
// `PgError::InvalidState`, never a silently-decoded nonsense actor.
#[tokio::test]
async fn audit_read_rejects_non_subject_actor() {
    use morpholog_postgres::PgError;

    let pool = test_pool().await;
    reset_db(&pool).await;

    // Hand-write an audit row whose actor column holds a tagged *decimal*
    // rather than a subject - the corruption the read boundary must catch.
    let non_subject_actor = serde_json::json!({ "type": "decimal", "value": "42" });
    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(Uuid::now_v7())
    .bind("hand_written")
    .bind(serde_json::json!([]))
    .bind(non_subject_actor)
    .bind(1_i32)
    .bind(serde_json::json!([]))
    .bind(serde_json::json!([]))
    .bind(serde_json::json!([]))
    .bind(serde_json::json!([]))
    .execute(&pool)
    .await
    .expect("hand-written audit row should insert");

    let err = list_audit_rows(&pool)
        .await
        .expect_err("a non-subject actor column must not decode silently");

    assert!(
        matches!(err, PgError::InvalidState(_)),
        "expected PgError::InvalidState, got {err:?}"
    );
}

/// Emits the identical intent twice, forcing both outbox rows onto the
/// same deterministic idempotency key.
fn double_emit_transformation() -> Transformation {
    use morpholog_core::ir_builder::{emit, subj as lit_subj, transformation};
    transformation(
        "double_emit",
        vec![],
        vec![
            emit("Ping", vec![lit_subj("p")]),
            emit("Ping", vec![lit_subj("p")]),
        ],
    )
}

#[tokio::test]
async fn duplicate_intent_in_one_transformation_surfaces_named_error() {
    use morpholog_postgres::PgError;

    let pool = test_pool().await;
    reset_db(&pool).await;

    let err = common::propose_pg_with_test_actor(&pool, &double_emit_transformation(), vec![], &[])
        .await
        .expect_err("two identical intents must collide on the idempotency key");

    assert!(
        matches!(err, PgError::DuplicateIntent),
        "expected PgError::DuplicateIntent, got {err:?}"
    );

    assert!(
        list_audit_rows(&pool).await.unwrap().is_empty(),
        "the collision must roll back the whole transformation - no audit row"
    );
    assert!(
        list_pending_outbox(&pool).await.unwrap().is_empty(),
        "no outbox row may survive the rollback"
    );
}

// ============================================================
// Approval controls - durable proof that Term::Actor and a decimal Prop::Compare flow
// through propose_against_pg into the audit log and the asserted
// Approval / LimitedApproval claims. One scenario walks both the
// unconditional and quantitative authority shapes.
// ============================================================

#[tokio::test]
async fn approval_controls_full_chain_through_pg() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    let invariants = approval_controls::all_invariants();

    // ---------- Unconditional authority ----------

    // 1. jordan is granted unconditional authority for vendor onboarding.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &approval_controls::grant_approval_authority(),
        vec![subj("jordan"), subj("vendor_onboarding")],
        &invariants,
    )
    .await
    .expect("grant_approval_authority should not error");
    assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));

    // 2. jordan approves; Term::Actor stamps her onto the Approval.
    let outcome = common::propose_pg_as(
        &pool,
        &approval_controls::approve_document(),
        vec![subj("doc_001"), subj("vendor_onboarding")],
        "jordan",
        &invariants,
    )
    .await
    .expect("approve_document should not error");
    let PgProposalOutcome::Committed {
        transition_id: approve_tid,
        actor: receipt_actor,
        asserted_claims,
        ..
    } = outcome
    else {
        panic!("expected Committed, got {outcome:?}");
    };
    assert_eq!(receipt_actor, Subject::from("jordan"));
    assert!(
        asserted_claims
            .iter()
            .any(|c| c.predicate.as_str() == "Approval"
                && c.args == vec![subj("doc_001"), subj("vendor_onboarding"), subj("jordan")])
    );

    // 3. The audit row's actor column matches.
    let audit_rows = list_audit_rows(&pool).await.unwrap();
    let approve_row = audit_rows
        .iter()
        .find(|r| r.transition_id == approve_tid)
        .unwrap();
    assert_eq!(approve_row.actor, Subject::from("jordan"));

    // 4. alice has no authority; her attempt is rejected and leaves no
    // durable trace. Rejected carries no transition_id, so snapshot row
    // counts before and after to assert the negative.
    let audit_before = list_audit_rows(&pool).await.unwrap().len();
    let outbox_before = list_pending_outbox(&pool).await.unwrap().len();
    let outcome = common::propose_pg_as(
        &pool,
        &approval_controls::approve_document(),
        vec![subj("doc_002"), subj("vendor_onboarding")],
        "alice",
        &invariants,
    )
    .await
    .expect("propose should not error");
    assert!(matches!(outcome, PgProposalOutcome::Rejected { .. }));
    assert_eq!(
        list_audit_rows(&pool).await.unwrap().len(),
        audit_before,
        "rejected approval must not write an audit row",
    );
    assert_eq!(
        list_pending_outbox(&pool).await.unwrap().len(),
        outbox_before,
        "rejected approval must not enqueue an outbox intent",
    );

    // ---------- Quantitative authority ----------

    // 5. jordan is granted an invoice limit of 1000.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &approval_controls::grant_approval_limit(),
        vec![subj("jordan"), subj("invoice"), dec(1000)],
        &invariants,
    )
    .await
    .expect("grant_approval_limit should not error");
    assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));

    // 6. jordan approves a 750 invoice; the decimal Prop::Compare admits. Pin the full
    // round-trip: receipt actor, asserted claim, emitted intent
    // staged to the outbox, and the audit row's actor column.
    let outcome = common::propose_pg_as(
        &pool,
        &approval_controls::approve_within_limit(),
        vec![subj("inv_001"), subj("invoice"), dec(750)],
        "jordan",
        &invariants,
    )
    .await
    .expect("approve_within_limit should not error");
    let PgProposalOutcome::Committed {
        transition_id: limit_approve_tid,
        actor: limit_receipt_actor,
        asserted_claims,
        emitted_intents,
        ..
    } = outcome
    else {
        panic!("expected Committed");
    };
    assert_eq!(limit_receipt_actor, Subject::from("jordan"));
    assert!(
        asserted_claims
            .iter()
            .any(|c| c.predicate.as_str() == "LimitedApproval"
                && c.args == vec![subj("inv_001"), subj("invoice"), dec(750), subj("jordan")])
    );
    assert!(
        emitted_intents
            .iter()
            .any(|i| i.name == "DocumentApprovedWithinLimit"),
        "approve_within_limit must emit DocumentApprovedWithinLimit",
    );
    let audit_rows = list_audit_rows(&pool).await.unwrap();
    let limit_row = audit_rows
        .iter()
        .find(|r| r.transition_id == limit_approve_tid)
        .expect("audit row for approve_within_limit must exist");
    assert_eq!(limit_row.actor, Subject::from("jordan"));
    assert!(
        list_pending_outbox(&pool)
            .await
            .unwrap()
            .iter()
            .any(|r| r.intent_type == "DocumentApprovedWithinLimit"),
        "DocumentApprovedWithinLimit intent must be staged to the outbox",
    );

    // 7. An over-limit attempt is rejected at require. Same negative
    // pin as step 4: no audit row, no outbox row.
    let audit_before = list_audit_rows(&pool).await.unwrap().len();
    let outbox_before = list_pending_outbox(&pool).await.unwrap().len();
    let outcome = common::propose_pg_as(
        &pool,
        &approval_controls::approve_within_limit(),
        vec![subj("inv_over"), subj("invoice"), dec(2000)],
        "jordan",
        &invariants,
    )
    .await
    .expect("propose should not error");
    assert!(matches!(outcome, PgProposalOutcome::Rejected { .. }));
    assert_eq!(list_audit_rows(&pool).await.unwrap().len(), audit_before);
    assert_eq!(
        list_pending_outbox(&pool).await.unwrap().len(),
        outbox_before
    );

    // ---------- Durable cross-cuts ----------

    let claims = list_claims(&pool).await.unwrap();
    assert!(claims.iter().any(|c| c.predicate.as_str() == "Approval"
        && c.args == vec![subj("doc_001"), subj("vendor_onboarding"), subj("jordan")]));
    assert!(
        claims
            .iter()
            .any(|c| c.predicate.as_str() == "LimitedApproval"
                && c.args == vec![subj("inv_001"), subj("invoice"), dec(750), subj("jordan")])
    );
    // The two rejected attempts left no trace.
    assert!(!claims.iter().any(|c| c.predicate.as_str() == "Approval"
        && c.args == vec![subj("doc_002"), subj("vendor_onboarding"), subj("alice")]));
    assert!(
        !claims
            .iter()
            .any(|c| c.predicate.as_str() == "LimitedApproval"
                && c.args == vec![subj("inv_over"), subj("invoice"), dec(2000), subj("jordan")])
    );

    // ---------- Require-vs-invariant: history survives revocation ----------

    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &approval_controls::revoke_approval_authority(),
        vec![subj("jordan"), subj("vendor_onboarding")],
        &invariants,
    )
    .await
    .expect("revoke should not error");
    assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));

    let claims_after_revoke = list_claims(&pool).await.unwrap();
    assert!(
        claims_after_revoke
            .iter()
            .any(|c| c.predicate.as_str() == "Approval"
                && c.args == vec![subj("doc_001"), subj("vendor_onboarding"), subj("jordan")])
    );
    assert!(
        !claims_after_revoke
            .iter()
            .any(|c| c.predicate.as_str() == "MayApprove"
                && c.args == vec![subj("jordan"), subj("vendor_onboarding")])
    );
}

// ============================================================
// Example 5: insurance claim settlement
// ============================================================

/// Walks the full insurance_claim_settlement chain through
/// `propose_against_pg`: policy issuance, claim reporting, authority
/// grant, a first under-cap settlement (admitted), a boundary-equality
/// settlement that exactly fills the aggregate (admitted), an over-cap
/// attempt (rejected, no audit/outbox), and the `PolicyLimitUsage`
/// derived claim read back. Pins the addition-based aggregate under
/// durable commit semantics.
#[tokio::test]
async fn insurance_claim_settlement_full_chain_through_pg() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    let invariants = insurance_claim_settlement::all_invariants();

    // 1. Issue a £100k aggregate policy.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &insurance_claim_settlement::issue_policy(),
        vec![subj("policy_001"), dec(100_000)],
        &invariants,
    )
    .await
    .expect("issue_policy should not error");
    assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));

    // 2. Grant alex £100k of settlement authority.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &insurance_claim_settlement::grant_settlement_authority(),
        vec![subj("alex"), dec(100_000)],
        &invariants,
    )
    .await
    .expect("grant_settlement_authority should not error");
    assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));

    // 3. Report two claims against the policy.
    for (claim_id, amount) in [("claim_001", 60_000_i64), ("claim_002", 40_000)] {
        let outcome = common::propose_pg_with_test_actor(
            &pool,
            &insurance_claim_settlement::report_claim(),
            vec![subj(claim_id), subj("policy_001"), dec(amount)],
            &invariants,
        )
        .await
        .expect("report_claim should not error");
        assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));
    }

    // 4. First settlement: £60k under the £100k aggregate. Admitted.
    // Pin receipt actor, durable claim, audit row, and outbox intent.
    let outcome = common::propose_pg_as(
        &pool,
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_001"), subj("settlement_001"), dec(60_000)],
        "alex",
        &invariants,
    )
    .await
    .expect("first authorise_settlement should not error");
    let PgProposalOutcome::Committed {
        transition_id: first_tid,
        actor: first_receipt_actor,
        asserted_claims,
        emitted_intents,
        ..
    } = outcome
    else {
        panic!("expected Committed");
    };
    assert_eq!(first_receipt_actor, Subject::from("alex"));
    assert!(
        asserted_claims
            .iter()
            .any(|c| c.predicate.as_str() == "SettlementAuthorised"
                && c.args
                    == vec![
                        subj("claim_001"),
                        subj("settlement_001"),
                        dec(60_000),
                        subj("alex"),
                    ])
    );
    assert!(
        asserted_claims
            .iter()
            .any(|c| c.predicate.as_str() == "SettlementPaid"
                && c.args
                    == vec![
                        subj("policy_001"),
                        subj("claim_001"),
                        subj("settlement_001"),
                        dec(60_000),
                    ])
    );
    assert!(
        emitted_intents
            .iter()
            .any(|i| i.name == "ClaimPaymentRequested"),
        "authorise_settlement must emit ClaimPaymentRequested",
    );
    let audit_rows = list_audit_rows(&pool).await.unwrap();
    let first_row = audit_rows
        .iter()
        .find(|r| r.transition_id == first_tid)
        .unwrap();
    assert_eq!(first_row.actor, Subject::from("alex"));
    assert!(
        list_pending_outbox(&pool)
            .await
            .unwrap()
            .iter()
            .any(|r| r.intent_type == "ClaimPaymentRequested"),
        "ClaimPaymentRequested intent must be staged to the outbox",
    );

    // 5. PolicyHeadroom now reflects 100k - 60k = 40k remaining,
    // enforced by the conservation invariant at commit.
    let claims_after_first = list_claims(&pool).await.unwrap();
    assert!(
        claims_after_first
            .iter()
            .any(|c| c.predicate.as_str() == "PolicyHeadroom"
                && c.args == vec![subj("policy_001"), dec(40_000)]),
        "PolicyHeadroom should be 40k after the £60k settlement; \
         got: {:?}",
        claims_after_first
            .iter()
            .filter(|c| c.predicate.as_str() == "PolicyHeadroom")
            .collect::<Vec<_>>()
    );

    // 6. Second settlement: £40k. Cumulative 60 + 40 = 100 - the
    // exact aggregate. Boundary equality admits. Headroom should
    // land at exactly 0.
    let outcome = common::propose_pg_as(
        &pool,
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_002"), subj("settlement_002"), dec(40_000)],
        "alex",
        &invariants,
    )
    .await
    .expect("boundary-fill authorise_settlement should not error");
    assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));
    let claims_after_second = list_claims(&pool).await.unwrap();
    assert!(
        claims_after_second
            .iter()
            .any(|c| c.predicate.as_str() == "PolicyHeadroom"
                && c.args == vec![subj("policy_001"), dec(0)]),
        "PolicyHeadroom should be 0 after the policy is exhausted"
    );

    // 7. A third settlement would push cumulative past the aggregate:
    // rejected at admission, leaving no durable trace.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &insurance_claim_settlement::report_claim(),
        vec![subj("claim_003"), subj("policy_001"), dec(30_000)],
        &invariants,
    )
    .await
    .expect("report_claim 003 should not error");
    assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));

    let audit_before = list_audit_rows(&pool).await.unwrap().len();
    let outbox_before = list_pending_outbox(&pool).await.unwrap().len();
    let outcome = common::propose_pg_as(
        &pool,
        &insurance_claim_settlement::authorise_settlement(),
        vec![subj("claim_003"), subj("settlement_003"), dec(30_000)],
        "alex",
        &invariants,
    )
    .await
    .expect("over-cap propose should not error");
    assert!(matches!(outcome, PgProposalOutcome::Rejected { .. }));
    assert_eq!(
        list_audit_rows(&pool).await.unwrap().len(),
        audit_before,
        "rejected settlement must not write an audit row",
    );
    assert_eq!(
        list_pending_outbox(&pool).await.unwrap().len(),
        outbox_before,
        "rejected settlement must not enqueue an outbox intent",
    );

    // 8. Derived `PolicyLimitUsage` matches the cumulative paid.
    let usage_rows = list_derived(&pool, &insurance_claim_settlement::policy_limit_usage())
        .await
        .unwrap();
    assert_eq!(
        usage_rows.len(),
        1,
        "exactly one policy has admitted settlements"
    );
    assert_eq!(
        usage_rows[0].args,
        vec![subj("policy_001"), dec(100_000)],
        "PolicyLimitUsage should show £100k consumed",
    );
}

/// `load_scoped_state` - the read-only pre-state load behind `explain` -
/// must apply the same predicate scope as `propose_against_pg`: claims a
/// transformation could never read are not fetched, so an explanation
/// runs the kernel against exactly the state a real proposal would see.
#[tokio::test]
async fn load_scoped_state_loads_only_in_scope_predicates() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // One claim the netting transformation actually reads, and one of a
    // predicate nothing in the programme references.
    insert_pre_state(
        &pool,
        vec![
            claim("ApprovedSettlementLine", vec![subj("l1")]),
            claim("UnrelatedNoise", vec![subj("x")]),
        ],
    )
    .await;

    let state = load_scoped_state(
        &pool,
        &settlement_netting::create_net_settlement(),
        &settlement_netting::all_invariants(),
    )
    .await
    .expect("load_scoped_state should not error");

    assert!(
        common::has_claim(&state, "ApprovedSettlementLine", &[subj("l1")]),
        "in-scope claim should be loaded"
    );
    assert!(
        !common::has_claim(&state, "UnrelatedNoise", &[subj("x")]),
        "out-of-scope claim must not be loaded"
    );
}
