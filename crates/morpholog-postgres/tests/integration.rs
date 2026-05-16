//! Integration tests for `propose_against_pg`.
//!
//! These tests require a running PostgreSQL 17 server with the
//! `morpholog` schema applied (from `crates/morpholog-core/sql/schema.sql`).
//! The connection string is read from the `DATABASE_URL` environment
//! variable; tests panic if it is not set.
//!
//! Each test calls `reset_db` to TRUNCATE all three tables before
//! running its scenario. Run with `cargo test -- --test-threads=1` so
//! tests do not race on the shared schema.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::examples::settlement_netting;
use morpholog_core::{ClaimInstance, EvalValue, IntentInstance, Stmt, Term, Transformation};
use morpholog_postgres::{PgProposalOutcome, compute_idempotency_key, propose_against_pg};
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

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
    // Pre-state claims need a non-null `asserted_in` UUID. We use a
    // fixed synthetic UUID so the rows are easy to identify if a test
    // inspects them; it carries no semantic meaning.
    let fixture_transition = Uuid::nil();
    for claim in claims {
        let args_json = serde_json::to_value(&claim.args).unwrap();
        sqlx::query(
            "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
             VALUES ($1, $2, $3)",
        )
        .bind(&claim.predicate)
        .bind(&args_json)
        .bind(fixture_transition)
        .execute(pool)
        .await
        .unwrap();
    }
}

fn subj(s: &str) -> EvalValue {
    EvalValue::Subject(s.to_string())
}

fn dec(n: i64) -> EvalValue {
    EvalValue::Decimal(Decimal::new(n, 0))
}

fn claim(predicate: &str, args: Vec<EvalValue>) -> ClaimInstance {
    ClaimInstance {
        predicate: predicate.to_string(),
        args,
    }
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

    let outcome = propose_against_pg(
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

#[tokio::test]
async fn require_failure_writes_nothing() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // Same pre-state as the happy path, plus an extra Netted(l1) claim
    // so the require check fails before any staging.
    let mut claims = netting_pre_state_claims();
    claims.push(claim("Netted", vec![subj("l1")]));
    insert_pre_state(&pool, claims).await;

    let outcome = propose_against_pg(
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

#[tokio::test]
async fn invariant_violation_on_candidate_state_writes_nothing() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // Pre-state has an orphan SettlementLine for l1 (inconsistent legacy
    // data: line is not flagged Netted but is already in another net).
    // The require check on `not Netted(l1)` passes, but the candidate
    // state would then violate no_double_netting.
    let mut claims = netting_pre_state_claims();
    claims.push(claim(
        "SettlementLine",
        vec![subj("l1"), subj("old_net"), dec(60)],
    ));
    insert_pre_state(&pool, claims).await;

    let outcome = propose_against_pg(
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
    //
    // With:
    //   - transition_id = UUID::nil (sixteen 0x00 bytes)
    //   - intent.name   = "TestIntent"
    //   - intent.args   = [EvalValue::Subject("net1")]
    //
    // canonical_json(args) is `[{"type":"subject","value":"net1"}]`.
    //
    // The expected hex value below was computed independently (Python's
    // hashlib.sha256). Do NOT recompute it via the production helper —
    // the point is to detect formula drift, including delimiter changes.
    let transition_id = Uuid::nil();
    let intent = IntentInstance {
        name: "TestIntent".to_string(),
        args: vec![EvalValue::Subject("net1".to_string())],
    };

    let expected = "c32fd9040f866912cfc522571e851ee6240c9e5d19a39db9e50ac7834fd2341f";
    let actual = compute_idempotency_key(transition_id, &intent).unwrap();
    assert_eq!(actual, expected);
}

fn retract_marker_transformation() -> Transformation {
    let var = |s: &str| Term::Var(s.to_string());
    Transformation {
        name: "retract_marker".to_string(),
        parameters: vec!["subject".to_string()],
        body: vec![Stmt::Retract {
            predicate: "Marker".to_string(),
            args: vec![var("subject")],
        }],
    }
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

    let outcome = propose_against_pg(
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
    assert_eq!(retracted_claims[0].predicate, "Marker");
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

    let outcome = propose_against_pg(
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

    // Read all audit JSONB columns and verify they decode through the
    // PR #4 codec back into Rust types with the expected shapes.
    let (args_json, invariants_checked_json, asserted_json, retracted_json, intents_json): (
        serde_json::Value,
        serde_json::Value,
        serde_json::Value,
        serde_json::Value,
        serde_json::Value,
    ) = sqlx::query_as(
        "SELECT arguments, invariants_checked, asserted_claims, retracted_claims, emitted_intents
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

    // invariants_checked: [{name, version}, ...]
    let checked = invariants_checked_json.as_array().unwrap();
    assert_eq!(checked.len(), 3);
    for entry in checked {
        assert!(entry.get("name").and_then(|v| v.as_str()).is_some());
        assert_eq!(entry.get("version").and_then(|v| v.as_u64()), Some(1));
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
