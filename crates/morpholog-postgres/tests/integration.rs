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

use morpholog_core::examples::{
    claim_standing, double_entry_ledger, revenue_restatement, settlement_netting,
};
use morpholog_core::{ClaimInstance, EvalValue, IntentInstance, Stmt, Term, Transformation};
use morpholog_postgres::{
    PgProposalOutcome, compute_idempotency_key, list_audit_rows, list_claims, list_derived,
    list_pending_outbox,
};
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

mod common;

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

#[tokio::test]
async fn require_failure_writes_nothing() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // Same pre-state as the happy path, plus an extra Netted(l1) claim
    // so the require check fails before any staging.
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

// ============================================================
// Revenue restatement (Example 2) — durable proof of the
// contested-legitimacy model through propose_against_pg.
//
// Mirrors the in-memory test
// `full_restatement_chain_preserves_history_and_updates_pointer`
// in crates/morpholog-core/src/lib.rs, but runs every step through
// the PostgreSQL adapter and inspects the durable claims/audit/
// outbox rows directly.
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

#[tokio::test]
async fn revenue_restatement_full_chain_preserves_history_and_moves_pointer() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    let invariants = revenue_restatement::all_invariants();

    // 1. Admit initial independent verification at 92.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &revenue_restatement::admit_independent_verification(),
        vec![asset(), period(), dec(92), subj("ver_001")],
        &invariants,
    )
    .await
    .expect("step 1 propose_against_pg should not error");
    let PgProposalOutcome::Committed {
        emitted_intents, ..
    } = outcome
    else {
        panic!("step 1 expected Committed, got {outcome:?}");
    };
    assert_eq!(emitted_intents[0].name, "IndependentVerificationAdmitted");

    // 2. Bank recognises 92, rec_001. I1 holds against the current verification.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &revenue_restatement::recognise_bank_revenue(),
        vec![asset(), period(), dec(92), subj("rec_001")],
        &invariants,
    )
    .await
    .expect("step 2 propose_against_pg should not error");
    let PgProposalOutcome::Committed {
        emitted_intents, ..
    } = outcome
    else {
        panic!("step 2 expected Committed, got {outcome:?}");
    };
    assert_eq!(emitted_intents[0].name, "BankRevenueRecognised");

    // 3. Verifier corrects to 91 (ver_002 supersedes ver_001). The verifier's
    //    transformation body also retracts CurrentBankRecognition, so I1 is
    //    vacuously satisfied (no current pointer remains until the bank
    //    restates).
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &revenue_restatement::correct_independent_verification(),
        vec![asset(), period(), dec(91), subj("ver_002"), subj("ver_001")],
        &invariants,
    )
    .await
    .expect("step 3 propose_against_pg should not error");
    let PgProposalOutcome::Committed {
        retracted_claims,
        emitted_intents,
        ..
    } = outcome
    else {
        panic!("step 3 expected Committed, got {outcome:?}");
    };
    assert_eq!(retracted_claims.len(), 1);
    assert_eq!(retracted_claims[0].predicate, "CurrentBankRecognition");
    assert_eq!(emitted_intents[0].name, "VerificationCorrected");

    // 4. Bank restates to 91 with rec_002. New current pointer; new Supersedes.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &revenue_restatement::restate_bank_revenue(),
        vec![asset(), period(), dec(91), subj("rec_002"), subj("rec_001")],
        &invariants,
    )
    .await
    .expect("step 4 propose_against_pg should not error");
    let PgProposalOutcome::Committed {
        emitted_intents, ..
    } = outcome
    else {
        panic!("step 4 expected Committed, got {outcome:?}");
    };
    assert_eq!(emitted_intents[0].name, "BankRevenueRestated");

    // Final DB shape: 2 IV + 2 BR + 2 Supersedes + 1 Current = 7 claims.
    assert_eq!(count(&pool, "claims").await, 7);
    assert_eq!(count(&pool, "audit").await, 4);
    assert_eq!(count(&pool, "outbox").await, 4);

    // Historical IV remains; new IV present; supersession recorded.
    assert!(
        claim_exists(
            &pool,
            "IndependentlyVerifiedRevenue",
            &[asset(), period(), dec(92), subj("ver_001")],
        )
        .await,
        "historical IV(92, ver_001) must be preserved"
    );
    assert!(
        claim_exists(
            &pool,
            "IndependentlyVerifiedRevenue",
            &[asset(), period(), dec(91), subj("ver_002")],
        )
        .await
    );
    assert!(
        claim_exists(&pool, "Supersedes", &[subj("ver_002"), subj("ver_001")]).await,
        "verification lineage must persist"
    );

    // Historical BR preserved; new BR + supersession recorded.
    assert!(
        claim_exists(
            &pool,
            "BankRecognisedRevenue",
            &[asset(), period(), dec(92), subj("rec_001")],
        )
        .await,
        "historical BR(92, rec_001) must be preserved"
    );
    assert!(
        claim_exists(
            &pool,
            "BankRecognisedRevenue",
            &[asset(), period(), dec(91), subj("rec_002")],
        )
        .await
    );
    assert!(
        claim_exists(&pool, "Supersedes", &[subj("rec_002"), subj("rec_001")]).await,
        "recognition lineage must persist"
    );

    // Current pointer moved to rec_002; rec_001 pointer gone.
    assert!(
        claim_exists(
            &pool,
            "CurrentBankRecognition",
            &[asset(), period(), subj("rec_002")],
        )
        .await,
        "current pointer must be rec_002"
    );
    assert!(
        !claim_exists(
            &pool,
            "CurrentBankRecognition",
            &[asset(), period(), subj("rec_001")],
        )
        .await,
        "old current pointer must have been retracted"
    );

    // Outbox carries one intent per committed step, in causal order.
    let intent_types: Vec<String> = sqlx::query_scalar(
        "SELECT intent_type FROM morpholog.outbox ORDER BY enqueued_at, intent_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        intent_types,
        vec![
            "IndependentVerificationAdmitted".to_string(),
            "BankRevenueRecognised".to_string(),
            "VerificationCorrected".to_string(),
            "BankRevenueRestated".to_string(),
        ]
    );
}

#[tokio::test]
async fn correct_verification_with_no_prior_rejects_and_writes_nothing() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // No pre-state: there is no IndependentlyVerifiedRevenue for the
    // (asset, period, _, prior_verification_id) tuple, so the first
    // `require` in correct_independent_verification fails. The proposal
    // must be rejected and leave all three tables empty.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &revenue_restatement::correct_independent_verification(),
        vec![asset(), period(), dec(91), subj("ver_002"), subj("ver_001")],
        &revenue_restatement::all_invariants(),
    )
    .await
    .expect("propose_against_pg should not error");

    let PgProposalOutcome::Rejected { reason } = outcome else {
        panic!("expected Rejected, got {outcome:?}");
    };
    assert!(reason.contains("require"), "got reason: {reason}");

    assert_eq!(count(&pool, "claims").await, 0);
    assert_eq!(count(&pool, "audit").await, 0);
    assert_eq!(count(&pool, "outbox").await, 0);
}

// ============================================================
// Claim standing (Example 3) — durable proof of
// admissibility-for-purpose through propose_against_pg.
//
// Mirrors the in-memory chain in
// `crates/morpholog-core/src/lib.rs` but runs every step through
// the PostgreSQL adapter and inspects the durable claims, audit,
// and outbox rows directly.
// ============================================================

#[tokio::test]
async fn claim_standing_full_chain_through_pg() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    let invariants = claim_standing::all_invariants();
    let bank = subj(claim_standing::BANK_DEBT_SERVICE);
    let investor = subj(claim_standing::INVESTOR_REPORTING);

    // 1. Admit IV at 91.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &claim_standing::admit_independent_verification(),
        vec![asset(), period(), dec(91), subj("ver_001")],
        &invariants,
    )
    .await
    .expect("step 1 propose_against_pg should not error");
    let PgProposalOutcome::Committed {
        emitted_intents, ..
    } = outcome
    else {
        panic!("step 1 expected Committed, got {outcome:?}");
    };
    assert_eq!(emitted_intents[0].name, "IndependentVerificationAdmitted");

    // 2. Bank credit committee grants debt-service-coverage standing.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &claim_standing::grant_standing(),
        vec![
            subj("ver_001"),
            bank.clone(),
            subj("credit_committee"),
            subj("grant_001"),
        ],
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
    assert_eq!(
        asserted_claims.len(),
        2,
        "StandingGrantedBy + AdmissibleFor"
    );
    assert_eq!(emitted_intents[0].name, "StandingGranted");

    // 3. Bank admits a debt-service decision against the verification.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &claim_standing::admit_debt_service_revenue(),
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
    let PgProposalOutcome::Committed {
        emitted_intents, ..
    } = outcome
    else {
        panic!("step 3 expected Committed, got {outcome:?}");
    };
    assert_eq!(emitted_intents[0].name, "DebtServiceRevenueAdmitted");

    // 4. Investor relations office grants investor-reporting standing on the
    //    same verification. Parallel admissibility — bank standing is still
    //    active, and now investor standing too.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &claim_standing::grant_standing(),
        vec![
            subj("ver_001"),
            investor.clone(),
            subj("investor_relations_office"),
            subj("grant_002"),
        ],
        &invariants,
    )
    .await
    .expect("step 4 propose_against_pg should not error");
    let PgProposalOutcome::Committed { .. } = outcome else {
        panic!("step 4 expected Committed, got {outcome:?}");
    };

    // 5. Investor relations admits an investor report against the verification.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &claim_standing::admit_investor_reported_revenue(),
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
    let PgProposalOutcome::Committed {
        emitted_intents, ..
    } = outcome
    else {
        panic!("step 5 expected Committed, got {outcome:?}");
    };
    assert_eq!(emitted_intents[0].name, "InvestorReportedRevenueAdmitted");

    // 6. Bank revokes its standing. AdmissibleFor(ver_001, bank) is retracted;
    //    the historical decision_001 must survive; investor standing is
    //    untouched.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &claim_standing::revoke_standing(),
        vec![subj("ver_001"), bank.clone(), subj("revoke_001")],
        &invariants,
    )
    .await
    .expect("step 6 propose_against_pg should not error");
    let PgProposalOutcome::Committed {
        retracted_claims,
        emitted_intents,
        ..
    } = outcome
    else {
        panic!("step 6 expected Committed, got {outcome:?}");
    };
    assert_eq!(retracted_claims.len(), 1);
    assert_eq!(retracted_claims[0].predicate, "AdmissibleFor");
    assert_eq!(emitted_intents[0].name, "StandingRevocationAdmitted");

    // Final DB shape: 1 IV + 2 StandingGrantedBy + 1 AdmissibleFor (investor
    // only) + 1 StandingRevoked + 1 DebtServiceRevenue + 1
    // InvestorReportedRevenue = 7 claims.
    assert_eq!(count(&pool, "claims").await, 7);
    assert_eq!(count(&pool, "audit").await, 6);
    assert_eq!(count(&pool, "outbox").await, 6);

    // Underlying IV unchanged.
    assert!(
        claim_exists(
            &pool,
            "IndependentlyVerifiedRevenue",
            &[asset(), period(), dec(91), subj("ver_001")],
        )
        .await
    );

    // Both grant provenances preserved.
    assert!(
        claim_exists(
            &pool,
            "StandingGrantedBy",
            &[
                subj("ver_001"),
                bank.clone(),
                subj("credit_committee"),
                subj("grant_001"),
            ],
        )
        .await,
        "bank grant provenance must persist after revocation"
    );
    assert!(
        claim_exists(
            &pool,
            "StandingGrantedBy",
            &[
                subj("ver_001"),
                investor.clone(),
                subj("investor_relations_office"),
                subj("grant_002"),
            ],
        )
        .await
    );

    // Revocation recorded as its own append-only claim.
    assert!(
        claim_exists(
            &pool,
            "StandingRevoked",
            &[subj("ver_001"), bank.clone(), subj("revoke_001")],
        )
        .await
    );

    // Active admissibility: investor present, bank gone.
    assert!(
        claim_exists(&pool, "AdmissibleFor", &[subj("ver_001"), investor.clone()],).await,
        "investor standing must remain active"
    );
    assert!(
        !claim_exists(&pool, "AdmissibleFor", &[subj("ver_001"), bank.clone()]).await,
        "bank standing must have been retracted"
    );

    // Historical decisions both survive.
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
        "the bank's historical decision must survive revocation"
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

    // Outbox carries one intent per committed transformation, in causal
    // order.
    let intent_types: Vec<String> = sqlx::query_scalar(
        "SELECT intent_type FROM morpholog.outbox ORDER BY enqueued_at, intent_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        intent_types,
        vec![
            "IndependentVerificationAdmitted".to_string(),
            "StandingGranted".to_string(),
            "DebtServiceRevenueAdmitted".to_string(),
            "StandingGranted".to_string(),
            "InvestorReportedRevenueAdmitted".to_string(),
            "StandingRevocationAdmitted".to_string(),
        ]
    );
}

#[tokio::test]
async fn decision_after_revocation_rejects_and_writes_nothing() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // Pre-state: IV admitted, bank standing was granted and then revoked
    // (so the historical StandingGrantedBy and StandingRevoked claims are
    // present, but no active AdmissibleFor). This is the shape the chain
    // test above leaves behind after step 6 for the bank purpose.
    insert_pre_state(
        &pool,
        vec![
            claim(
                "IndependentlyVerifiedRevenue",
                vec![asset(), period(), dec(91), subj("ver_001")],
            ),
            claim(
                "StandingGrantedBy",
                vec![
                    subj("ver_001"),
                    subj(claim_standing::BANK_DEBT_SERVICE),
                    subj("credit_committee"),
                    subj("grant_001"),
                ],
            ),
            claim(
                "StandingRevoked",
                vec![
                    subj("ver_001"),
                    subj(claim_standing::BANK_DEBT_SERVICE),
                    subj("revoke_001"),
                ],
            ),
        ],
    )
    .await;

    // A new debt-service decision must be rejected: the AdmissibleFor
    // require fails because revocation retracted it.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &claim_standing::admit_debt_service_revenue(),
        vec![
            asset(),
            period(),
            dec(91),
            subj("decision_002"),
            subj("ver_001"),
        ],
        &claim_standing::all_invariants(),
    )
    .await
    .expect("propose_against_pg should not error");

    let PgProposalOutcome::Rejected { reason } = outcome else {
        panic!("expected Rejected, got {outcome:?}");
    };
    assert!(reason.contains("require"), "got reason: {reason}");

    // Pre-state had 3 claims; no audit or outbox rows must appear.
    assert_eq!(count(&pool, "claims").await, 3, "pre-state unchanged");
    assert_eq!(count(&pool, "audit").await, 0);
    assert_eq!(count(&pool, "outbox").await, 0);
}

// ============================================================
// Double-entry ledger (Example 4) — durable proof of balanced
// posting, period close, and restatement through
// propose_against_pg.
//
// Mirrors the in-memory chain in
// `crates/morpholog-core/tests/double_entry_ledger.rs` but runs
// every step through the PostgreSQL adapter and inspects the
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

    // 3. Restate the entry with a corrected amount (101 instead of
    //    100). The restatement transformation does not check
    //    PeriodClosed; restatement is the closed-period path.
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

    // Pre-state: the period has already been closed. (In practice
    // this happens earlier in the chain; for this rejection test
    // we set it up directly.)
    insert_pre_state(&pool, vec![claim("PeriodClosed", vec![ledger_period()])]).await;

    // A normal posting against the closed period must be rejected by
    // `require not PeriodClosed`, with no writes to claims, audit,
    // or outbox.
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
// Read API — current-state inspection helpers
//
// The helpers are deliberately boring: current state only, no
// as-of, no derived claims, no filtering beyond the natural
// `status = 'pending'` filter on outbox. The tests below pin
// the helpers against scenarios already exercised by the
// propose_against_pg tests above; the goal is to verify the
// codec round-trips and the orderings are stable, not to
// re-prove the kernel semantics.
// ============================================================

#[tokio::test]
async fn list_claims_returns_admitted_claims_in_stable_order() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // Post a simple journal entry: cash 100 debit, revenue 100 credit.
    // Three claims will land: 1 JournalEntry, 2 JournalLine.
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

    // The three claims, in insertion order. Since they were all admitted
    // by the same transition (same asserted_at to microsecond resolution),
    // the predicate-then-args tie-break determines order:
    //   JournalEntry < JournalLine alphabetically by predicate_name.
    // Within JournalLine, the two rows are ordered by `arguments::text`.
    assert_eq!(claims[0].predicate, "JournalEntry");
    assert_eq!(claims[1].predicate, "JournalLine");
    assert_eq!(claims[2].predicate, "JournalLine");

    // Verify the JournalEntry's args round-trip through the codec.
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
    assert_eq!(second.asserted_claims[0].predicate, "PeriodClosed");
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

    // Every pending row has the expected default fields. A row that
    // has never been attempted has `attempt_count = 0` and
    // `last_attempt_at = None`; a row that has been retried (e.g. by
    // a delivery worker that failed) would have `attempt_count > 0`
    // and a non-NULL `last_attempt_at`. The four tests here exercise
    // only the fresh-enqueue path, so both assertions are tight.
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
    // Posts two simple ledger entries through the PG adapter, then
    // enumerates the trial-balance derived claim against the durable
    // state. Proves the round trip: claims persisted by
    // `propose_against_pg` are visible to `list_derived`, are passed
    // through the sync kernel's `enumerate_derived`, and produce one
    // row per distinct account with the expected debit-minus-credit
    // balance.
    //
    // Pinning behaviour rather than just the count: a regression that
    // dropped a row, computed the wrong balance, or ordered rows
    // non-deterministically would all fail here.
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

    // Two distinct accounts -> two rows. Structural Subject ordering
    // (natural string order on the inner UUID/name) sorts
    // `account_cash` before `account_revenue`, so the order is stable.
    assert_eq!(
        rows,
        vec![
            ClaimInstance {
                predicate: "TrialBalanceRow".to_string(),
                args: vec![subj("account_cash"), dec(150)],
            },
            ClaimInstance {
                predicate: "TrialBalanceRow".to_string(),
                args: vec![subj("account_revenue"), dec(-150)],
            },
        ],
        "trial balance over two posted entries must list each account once \
         with debits-minus-credits balance"
    );

    // Sanity: list_derived must not have mutated admitted state. The
    // claims table still contains exactly what the two entries asserted
    // (1 JournalEntry + 2 JournalLine per entry = 6 claims), and no
    // TrialBalanceRow has leaked into it.
    let claims = list_claims(&pool).await.unwrap();
    assert_eq!(
        claims.len(),
        6,
        "list_derived must not write claims back to the table"
    );
    assert!(
        !claims.iter().any(|c| c.predicate == "TrialBalanceRow"),
        "derived rows must not be admitted as claims"
    );
}

#[tokio::test]
async fn list_derived_on_empty_state_returns_no_rows() {
    // Mirrors the in-memory empty-state test: with no JournalLine
    // claims admitted, the `domain` enumerates no key bindings, so
    // the derived extension is empty. Pins that the empty case is
    // structural (zero domain bindings -> zero rows), not an error.
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
    // Black-box equivalence test for predicate-scoped loading. The
    // `trial_balance_row` derived claim references only the
    // `JournalLine` predicate. After this test inserts both the
    // ledger fixture AND a large pile of unrelated
    // `IndependentlyVerifiedRevenue` claims, `list_derived` must:
    //
    // 1. Return the exact same trial-balance rows as it would
    //    against a state containing only the ledger fixture (the
    //    noise must not change the answer).
    // 2. Implicitly skip the noise rows during the PG fetch step
    //    (the predicate footprint analysis names only "JournalLine",
    //    so the WHERE clause in `list_claims_for_predicates`
    //    excludes everything else - we cannot directly observe that
    //    here without an instrumented harness, but the correctness
    //    of (1) under heavy noise is the load-bearing property).
    //
    // If `predicates_referenced_by_derived` ever started missing a
    // predicate the kernel actually reads, (1) would fail because
    // the read path would silently skip needed claims. This is the
    // safety net for the analysis at runtime; the exhaustive `match`
    // is the safety net at compile time.
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

    // Noise: 200 unrelated claims of a predicate the trial-balance
    // derived claim does not reference. The predicate
    // (`IndependentlyVerifiedRevenue`) is borrowed from the
    // revenue-restatement example so the predicate exists
    // semantically, but the rows are not referenced anywhere in
    // `trial_balance_row()`'s body.
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

    // Sanity: the noise really is present and dominates the claims
    // table.
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

    // The trial balance for one cash/revenue entry is exactly two
    // rows. Under the noise, the answer must be byte-identical.
    assert_eq!(
        rows,
        vec![
            ClaimInstance {
                predicate: "TrialBalanceRow".to_string(),
                args: vec![subj("account_cash"), dec(100)],
            },
            ClaimInstance {
                predicate: "TrialBalanceRow".to_string(),
                args: vec![subj("account_revenue"), dec(-100)],
            },
        ],
        "predicate-scoped loading must return the same derived rows whether \
         or not unrelated noise claims are present"
    );
}

#[tokio::test]
async fn rejected_transformation_leaves_audit_and_outbox_empty() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    // Pre-state: period is already closed. A normal posting against it
    // is rejected by `require not PeriodClosed`.
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
    assert_eq!(claims[0].predicate, "PeriodClosed");

    // audit and outbox are empty — rejected transformations leave no
    // governed trace.
    let audit = list_audit_rows(&pool).await.unwrap();
    assert!(audit.is_empty(), "rejected transformation must not audit");

    let outbox = list_pending_outbox(&pool).await.unwrap();
    assert!(
        outbox.is_empty(),
        "rejected transformation must not enqueue intents"
    );
}

// Pins that the actor supplied on a Transition round-trips through the
// kernel, the audit-write codec, and list_audit_rows() unchanged.
// Also pins that the actor surfaces on PgProposalOutcome::Committed so
// the commit receipt is self-describing.
#[tokio::test]
async fn audit_row_records_actor() {
    use morpholog_core::Transition;
    use morpholog_postgres::propose_against_pg;

    let pool = test_pool().await;
    reset_db(&pool).await;

    let transformation = double_entry_ledger::post_simple_entry();
    let actor = EvalValue::Subject("user:jordan".to_string());
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

// Pins that a Transition whose `actor` is not a Subject is rejected at
// the kernel boundary as a typed error - not silently committed with a
// nonsense actor encoded into audit. Future authority work assumes
// actors are subject-shaped; this guard makes that contract testable.
#[tokio::test]
async fn propose_rejects_non_subject_actor() {
    use morpholog_core::Transition;
    use morpholog_postgres::{PgError, propose_against_pg};

    let pool = test_pool().await;
    reset_db(&pool).await;

    let transformation = double_entry_ledger::post_simple_entry();
    let transition = Transition {
        transformation_name: transformation.name.clone(),
        args: vec![
            subj("entry_bad_actor"),
            subj("d_2026_05_20"),
            subj("p_bad_actor"),
            subj("account_cash"),
            subj("account_revenue"),
            dec(100),
        ],
        actor: dec(42), // decimal where a subject is required
    };

    let err = propose_against_pg(
        &pool,
        &transformation,
        &transition,
        &double_entry_ledger::all_invariants(),
    )
    .await
    .expect_err("non-subject actor must surface as Kernel(TypeMismatch), not commit");

    assert!(
        matches!(err, PgError::Kernel(_)),
        "expected PgError::Kernel(TypeMismatch), got {err:?}"
    );

    let audit_rows = list_audit_rows(&pool).await.unwrap();
    assert!(audit_rows.is_empty(), "rejected proposal must not audit");
}
