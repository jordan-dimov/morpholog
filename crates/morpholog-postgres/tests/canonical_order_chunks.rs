//! The activation's baseline as chunk rows at order 1, its record last.
//! Attacker capability modelled: none beyond the history check's own;
//! these hold the rows the migration writes to their shape, with the
//! chunk bounds made small, and hold replay and the as-of reads to them.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{commit_entry, reset_db, test_pool};
use morpholog_core::ClaimInstance;
use morpholog_core::EvalValue;
use morpholog_postgres::{
    CHUNK_BYTES, CHUNK_CLAIMS, Operation, OperationKind, PgError, PgPool, VerifyOutcome,
    arguments_hash, read_claims_at, testing, verify_replay, write_activation_record_with,
};
use morpholog_test_support::subj;
use uuid::Uuid;

/// A legacy row admitting `X(k)`, and the claim it left, admitted by it.
async fn legacy_claim(pool: &PgPool, k: &str, second: u32) -> Uuid {
    let id = Uuid::now_v7();
    sqlx::query(
        "WITH row AS (
            INSERT INTO morpholog.audit (
                transition_id, transformation_name, arguments, actor, invariant_epoch,
                invariants_checked, asserted_claims, retracted_claims, emitted_intents,
                committed_at, attestation, parameters, model_hash, semantics_version,
                drawn_subjects)
            VALUES ($1, 'admit_x', jsonb_build_array(jsonb_build_object('type', 'subject', 'value', $2::text)),
                '{\"type\": \"subject\", \"value\": \"w\"}', 1, '[]',
                jsonb_build_array(jsonb_build_object('predicate', 'X', 'args',
                    jsonb_build_array(jsonb_build_object('type', 'subject', 'value', $2::text)))),
                '[]', '[]', timestamptz '2026-06-01T00:00:00Z' + $3 * interval '1 second',
                '{\"mode\": \"gateway\", \"authenticated_by\": \"w\"}', '[\"k\"]',
                'sha256:' || repeat('d', 64), 1, '[]')
            RETURNING transition_id)
         INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         SELECT 'X', jsonb_build_array(jsonb_build_object('type', 'subject', 'value', $2::text)), transition_id
         FROM row",
    )
    .bind(id)
    .bind(k)
    .bind(f64::from(second))
    .execute(pool)
    .await
    .unwrap();
    id
}

/// The cutover as `testing::activate` runs it, with the chunk bounds
/// chosen.
async fn activate_with(
    pool: &PgPool,
    chunk_claims: usize,
    chunk_bytes: usize,
) -> Result<(), PgError> {
    let mut tx = pool.begin().await.unwrap();
    sqlx::raw_sql(
        "ALTER TABLE morpholog.audit ENABLE TRIGGER audit_order;
         LOCK TABLE morpholog.claims, morpholog.audit IN EXCLUSIVE MODE;",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    write_activation_record_with(&mut tx, chunk_claims, chunk_bytes).await?;
    tx.commit().await.unwrap();
    Ok(())
}

/// The rows of order 1 in ordinal order: name, ordinal, operations.
async fn first_order(pool: &PgPool) -> Vec<(String, i32, i32)> {
    sqlx::query_as(
        "SELECT transformation_name, act_ordinal, jsonb_array_length(operations)::int
         FROM morpholog.audit WHERE audit_order = 1 ORDER BY act_ordinal",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn record_arguments(pool: &PgPool) -> serde_json::Value {
    sqlx::query_scalar(
        "SELECT arguments FROM morpholog.audit WHERE transformation_name = 'morpholog.activate'",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn transition_at(pool: &PgPool, ordinal: i32) -> Uuid {
    sqlx::query_scalar(
        "SELECT transition_id FROM morpholog.audit WHERE audit_order = 1 AND act_ordinal = $1",
    )
    .bind(ordinal)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn x(k: &str) -> ClaimInstance {
    ClaimInstance {
        predicate: "X".into(),
        args: vec![subj(k)],
    }
}

/// Seven claims in chunks of three: three chunks, the record at ordinal
/// three naming them, replay and provenance through every chunk, and a
/// chunk's id refused as an as-of coordinate.
#[tokio::test]
async fn the_baseline_is_written_in_chunks_the_record_names() {
    let pool = test_pool().await;
    common::drain_open_transactions(&pool).await;
    testing::reset_before_activation(&pool).await.unwrap();
    let keys = ["a", "b", "c", "d", "e", "f", "g"];
    let mut admitted = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        admitted.push((x(k), legacy_claim(&pool, k, i as u32).await));
    }
    activate_with(&pool, 3, CHUNK_BYTES).await.unwrap();

    let rows = first_order(&pool).await;
    assert_eq!(
        rows,
        [
            ("morpholog.baseline".to_string(), 0, 3),
            ("morpholog.baseline".to_string(), 1, 3),
            ("morpholog.baseline".to_string(), 2, 1),
            ("morpholog.activate".to_string(), 3, 0),
        ]
    );
    let arguments = record_arguments(&pool).await;
    assert_eq!(arguments[0]["value"], "7", "{arguments}");
    assert_eq!(arguments[2]["value"], "3", "{arguments}");
    assert!(matches!(
        verify_replay(&pool, &mut |_| {}).await.unwrap(),
        VerifyOutcome::Consistent {
            transitions: 11,
            claims: 7
        }
    ));

    let record = transition_at(&pool, 3).await;
    let mut reads = read_claims_at(&pool, record).await.unwrap();
    reads.sort_by(|a, b| format!("{:?}", a.claim.args).cmp(&format!("{:?}", b.claim.args)));
    assert_eq!(reads.len(), 7);
    for (claim, by) in &admitted {
        let read = reads
            .iter()
            .find(|r| r.claim == *claim)
            .expect("every claim is read");
        assert_eq!(read.asserted_in, *by, "the claim keeps its own admitter");
    }

    let chunk = transition_at(&pool, 1).await;
    match read_claims_at(&pool, chunk).await.unwrap_err() {
        PgError::BaselineChunk {
            chunk: refused,
            record: named,
        } => assert_eq!((refused, named), (chunk, record)),
        other => panic!("a chunk is not an as-of coordinate: {other:?}"),
    }

    let next = commit_entry(&pool, "after").await;
    assert_eq!(read_claims_at(&pool, next).await.unwrap().len(), 7 + 3);
    assert!(matches!(
        verify_replay(&pool, &mut |_| {}).await.unwrap(),
        VerifyOutcome::Consistent {
            transitions: 12,
            ..
        }
    ));
}

/// Wide claims meet the byte cap before the claim cap: a chunk closes
/// early and the next claim opens the next one, through many fetches of
/// at most a chunk's worth. A claim no chunk can hold is refused, and the
/// cutover with it.
#[tokio::test]
async fn the_byte_cap_closes_a_chunk_early() {
    let pool = test_pool().await;
    common::drain_open_transactions(&pool).await;
    testing::reset_before_activation(&pool).await.unwrap();
    for i in 0..13u32 {
        legacy_claim(&pool, &format!("{i:02}{}", "k".repeat(300)), i).await;
    }
    let refused = activate_with(&pool, CHUNK_CLAIMS, 100).await.unwrap_err();
    assert!(
        matches!(&refused, PgError::InvalidState(m) if m.contains("does not fit a baseline chunk")),
        "{refused:?}"
    );
    assert!(
        first_order(&pool).await.is_empty(),
        "nothing of a refused cutover stays"
    );

    activate_with(&pool, CHUNK_CLAIMS, 900).await.unwrap();
    let rows = first_order(&pool).await;
    let sizes: Vec<i32> = rows.iter().map(|(_, _, n)| *n).collect();
    assert_eq!(sizes, [2, 2, 2, 2, 2, 2, 1, 0], "{rows:?}");
    assert_eq!(rows[7].0, "morpholog.activate");
    assert!(matches!(
        verify_replay(&pool, &mut |_| {}).await.unwrap(),
        VerifyOutcome::Consistent {
            transitions: 21,
            claims: 13
        }
    ));
}

/// The cap is the serialised array's exact size: two claims whose array
/// is the cap fit one chunk; one byte less splits them.
#[tokio::test]
async fn the_byte_cap_is_the_arrays_exact_size() {
    let pool = test_pool().await;
    common::drain_open_transactions(&pool).await;
    testing::reset_before_activation(&pool).await.unwrap();
    let a = legacy_claim(&pool, "a", 0).await;
    let b = legacy_claim(&pool, "b", 1).await;
    let op = |k: &str, by: Uuid| Operation {
        op: OperationKind::Ins,
        predicate: "X".into(),
        args: vec![subj(k)],
        asserted_in: Some(by),
    };
    let exact = serde_json::to_vec(&[op("a", a), op("b", b)]).unwrap().len();

    activate_with(&pool, CHUNK_CLAIMS, exact).await.unwrap();
    let sizes: Vec<i32> = first_order(&pool)
        .await
        .iter()
        .map(|(_, _, n)| *n)
        .collect();
    assert_eq!(sizes, [2, 0]);

    testing::reset_before_activation(&pool).await.unwrap();
    let a = legacy_claim(&pool, "a", 0).await;
    let b = legacy_claim(&pool, "b", 1).await;
    let exact = serde_json::to_vec(&[op("a", a), op("b", b)]).unwrap().len();
    activate_with(&pool, CHUNK_CLAIMS, exact - 1).await.unwrap();
    let sizes: Vec<i32> = first_order(&pool)
        .await
        .iter()
        .map(|(_, _, n)| *n)
        .collect();
    assert_eq!(sizes, [1, 1, 0]);
}

/// The verifier's key for a claim is the table's: `arguments_hash` as
/// PostgreSQL computes it, over every kind of value and the characters
/// its JSON output escapes.
#[tokio::test]
async fn the_verifiers_claim_key_is_the_tables() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let shapes: Vec<Vec<EvalValue>> = [
        serde_json::json!([{"type": "subject", "value": "plain"}]),
        serde_json::json!([{"type": "subject", "value": "q\"uote \\ back\nnew\ttab \u{1} é ☃"}]),
        serde_json::json!([{"type": "decimal", "value": "-12.50"}, {"type": "bool", "value": true}]),
        serde_json::json!([{"type": "date", "value": "2026-02-28"}, {"type": "timestamp", "value": "2026-10-24T14:00:00Z"}]),
        serde_json::json!([{"type": "duration", "value": "PT6H"}]),
        serde_json::json!([{"type": "quantity", "value": {"amount": "25000", "unit": "USD"}}]),
        serde_json::json!([{"type": "collection", "value": [{"type": "subject", "value": "z"}, {"type": "collection", "value": []}]}]),
        serde_json::json!([]),
    ]
    .into_iter()
    .map(|v| serde_json::from_value(v).unwrap())
    .collect();
    let claims: Vec<ClaimInstance> = shapes
        .iter()
        .enumerate()
        .map(|(i, args)| ClaimInstance {
            predicate: format!("P{i}").into(),
            args: args.clone(),
        })
        .collect();
    common::seed_claims(&pool, &claims).await;
    let stored: Vec<(String, serde_json::Value, Vec<u8>)> = sqlx::query_as(
        "SELECT predicate_name, arguments, arguments_hash FROM morpholog.claims ORDER BY predicate_name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(stored.len(), shapes.len());
    for (predicate, arguments, hash) in stored {
        let args: Vec<EvalValue> = serde_json::from_value(arguments).unwrap();
        assert_eq!(
            arguments_hash(&args).unwrap().to_vec(),
            hash,
            "{predicate}: {args:?}"
        );
    }
}

/// A table with no claims activates with the record alone, at ordinal 0,
/// naming no chunks.
#[tokio::test]
async fn an_empty_table_activates_with_the_record_alone() {
    let pool = test_pool().await;
    common::drain_open_transactions(&pool).await;
    testing::reset_before_activation(&pool).await.unwrap();
    legacy_claim(&pool, "a", 0).await;
    sqlx::query("DELETE FROM morpholog.claims")
        .execute(&pool)
        .await
        .unwrap();
    activate_with(&pool, CHUNK_CLAIMS, CHUNK_BYTES)
        .await
        .unwrap();
    assert_eq!(
        first_order(&pool).await,
        [("morpholog.activate".to_string(), 0, 0)]
    );
    assert_eq!(record_arguments(&pool).await[2]["value"], "0");
    let record = transition_at(&pool, 0).await;
    assert!(read_claims_at(&pool, record).await.unwrap().is_empty());
    match verify_replay(&pool, &mut |_| {}).await.unwrap() {
        VerifyOutcome::Indeterminate {
            legacy_only_in_replay,
            legacy_only_in_baseline,
            ..
        } => {
            assert_eq!(legacy_only_in_replay.len(), 1);
            assert!(legacy_only_in_baseline.is_empty());
        }
        other => panic!("the legacy replay admitted X and the baseline holds nothing: {other:?}"),
    }
}

/// A database `init` creates has no activation: its first transition is
/// an ordinary row at order 1, replayed and read as one.
#[tokio::test]
async fn a_fresh_database_begins_with_an_ordinary_row_at_the_first_order() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let first = commit_entry(&pool, "first").await;
    let (order, ordinal): (i64, i32) = sqlx::query_as(
        "SELECT audit_order, act_ordinal FROM morpholog.audit WHERE transition_id = $1",
    )
    .bind(first)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((order, ordinal), (1, 0));
    assert!(matches!(
        verify_replay(&pool, &mut |_| {}).await.unwrap(),
        VerifyOutcome::Consistent { transitions: 1, .. }
    ));
    assert_eq!(read_claims_at(&pool, first).await.unwrap().len(), 3);
}

/// The trigger keeps the reserved names to the first order, chunks then
/// record, whatever a session inserts by hand.
#[tokio::test]
async fn the_reserved_names_cannot_land_outside_the_activation() {
    let pool = test_pool().await;
    common::drain_open_transactions(&pool).await;
    testing::reset_before_activation(&pool).await.unwrap();
    legacy_claim(&pool, "a", 0).await;
    activate_with(&pool, CHUNK_CLAIMS, CHUNK_BYTES)
        .await
        .unwrap();
    for name in ["morpholog.baseline", "morpholog.activate"] {
        let error = sqlx::query(
            "INSERT INTO morpholog.audit (
                transition_id, transformation_name, arguments, actor, invariant_epoch,
                invariants_checked, asserted_claims, retracted_claims, emitted_intents, attestation,
                parameters, model_hash, semantics_version, drawn_subjects, operations)
             VALUES (gen_random_uuid(), $1, '[]', '{\"type\": \"subject\", \"value\": \"w\"}', 1,
                '[]', '[]', '[]', '[]',
                '{\"mode\": \"migration\", \"authenticated_by\": \"w\", \"authenticated_by_oid\": 10}',
                '[]', 'sha256:' || repeat('d', 64), 1, '[]', '[]')",
        )
        .bind(name)
        .execute(&pool)
        .await
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("chunks then record"), "{name}: {message}");
    }
}
