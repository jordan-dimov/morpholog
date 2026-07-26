//! Durable integration test for the biometric-identification-oversight
//! example (EU AI Act Articles 12 / 14(5)) through `propose_against_pg`.
//!
//! Scope is what the in-memory tests cannot show: that the whole
//! statute-shaped lifecycle commits through PostgreSQL (Timestamp /
//! Duration round-trip the JSONB columns, the machine actor and the
//! human verifiers persist to `audit.actor`), and - the example's
//! title concept - that **as-of replay** reconstructs the oversight
//! that was in force at a past transition, so a revocation today does
//! not rewrite who was authorised when a past decision was made. The
//! per-gate rejection paths stay in the in-memory suite; the surface
//! does not change between in-memory and PG evaluation.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_examples::biometric_identification_oversight as bio;
use morpholog_postgres::{
    PgPool, PgProposalOutcome, list_claims, list_claims_at, reconstruct_state_at,
};
use uuid::Uuid;

mod common;
use common::{propose_pg_as, subj};
use common::{reset_db, test_pool};
use morpholog_test_support::ts;

/// Commit a transformation as `actor`, asserting it landed and
/// returning the transition id (so a later step can address it as-of).
async fn commit_as(
    pool: &PgPool,
    t: &morpholog_core::Transformation,
    args: Vec<morpholog_core::EvalValue>,
    actor: &str,
) -> Uuid {
    let outcome = propose_pg_as(pool, &common::compiled(bio::program()), t, args, actor)
        .await
        .expect("propose_against_pg should not error");
    match outcome {
        PgProposalOutcome::Committed { transition_id, .. } => transition_id,
        PgProposalOutcome::Rejected { reason, .. } => {
            panic!(
                "expected Committed from `{}`, got Rejected: {reason}",
                t.name
            )
        }
    }
}

/// The full governed lifecycle, durably: deploy, assess a version,
/// assign two overseers, start a use, the machine records a match,
/// two distinct overseers verify, a decision commits. Returns the
/// transition id immediately after the decision - the moment whose
/// oversight an Article 86 enquiry would reconstruct.
async fn run_to_decision(pool: &PgPool) -> Uuid {
    commit_as(
        pool,
        &bio::deploy_system(),
        vec![subj("cam_system"), subj("city_operator")],
        "compliance_office",
    )
    .await;
    commit_as(
        pool,
        &bio::place_version_in_service(),
        vec![
            subj("cam_system"),
            subj("model_v1"),
            ts("2026-10-01T00:00:00Z"),
            ts("2026-10-31T23:59:59Z"),
        ],
        "compliance_office",
    )
    .await;
    commit_as(
        pool,
        &bio::assign_oversight(),
        vec![subj("anna"), subj("cam_system")],
        "compliance_office",
    )
    .await;
    commit_as(
        pool,
        &bio::assign_oversight(),
        vec![subj("ben"), subj("cam_system")],
        "compliance_office",
    )
    .await;
    commit_as(
        pool,
        &bio::start_use(),
        vec![
            subj("use_1"),
            subj("cam_system"),
            subj("model_v1"),
            subj("missing_persons_register"),
            ts("2026-10-12T08:00:00Z"),
        ],
        "cam_system",
    )
    .await;
    commit_as(
        pool,
        &bio::record_match(),
        vec![
            subj("match_1"),
            subj("use_1"),
            subj("frame_4411"),
            subj("candidate_7"),
            ts("2026-10-12T09:30:00Z"),
        ],
        "cam_system",
    )
    .await;
    commit_as(
        pool,
        &bio::verify_match(),
        vec![subj("match_1"), ts("2026-10-12T10:00:00Z")],
        "anna",
    )
    .await;
    commit_as(
        pool,
        &bio::verify_match(),
        vec![subj("match_1"), ts("2026-10-12T10:20:00Z")],
        "ben",
    )
    .await;
    commit_as(
        pool,
        &bio::decide_on_identification(),
        vec![
            subj("decision_1"),
            subj("match_1"),
            subj("confirmed_identification"),
            ts("2026-10-12T11:00:00Z"),
        ],
        "anna",
    )
    .await
}

#[tokio::test]
async fn the_lifecycle_commits_durably_and_revocation_does_not_rewrite_the_past() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    let at_decision = run_to_decision(&pool).await;

    // The decision is on the durable record, with its match and both
    // verifications.
    let now = list_claims(&pool).await.unwrap();
    let has = |pred: &str| now.iter().any(|c| c.predicate.as_str() == pred);
    assert!(has("IdentificationDecision"), "the decision committed");
    assert_eq!(
        now.iter()
            .filter(|c| c.predicate.as_str() == "MatchVerified")
            .count(),
        2,
        "two verifications stand on the record"
    );

    // Anna's authority is revoked - her training lapsed. This commits
    // against current state (the decision already stands, so no
    // invariant is troubled), and it governs only the future.
    commit_as(
        &pool,
        &bio::revoke_oversight(),
        vec![subj("anna"), subj("cam_system")],
        "compliance_office",
    )
    .await;

    // Current state: anna no longer holds oversight.
    let after = list_claims(&pool).await.unwrap();
    let anna_holds_now = after.iter().any(|c| {
        c.predicate.as_str() == "OversightAssigned" && c.args.first() == Some(&subj("anna"))
    });
    assert!(
        !anna_holds_now,
        "revocation removed anna's current authority"
    );

    // The Article 86 enquiry: as-of the decision's transition, anna's
    // oversight WAS in force - the as-of replay reconstructs the
    // authority that held then, untouched by today's revocation. This
    // is the signal the in-memory tests cannot produce: real
    // time-travel over the audit log, not a snapshot of "now".
    let then = list_claims_at(&pool, at_decision).await.unwrap();
    let anna_held_then = then.iter().any(|c| {
        c.predicate.as_str() == "OversightAssigned" && c.args.first() == Some(&subj("anna"))
    });
    assert!(
        anna_held_then,
        "as-of the decision, anna's oversight was in force"
    );
    // And the decision itself was already on the record at that moment.
    let reconstructed = reconstruct_state_at(&pool, at_decision).await.unwrap();
    assert!(
        reconstructed
            .claims()
            .iter()
            .any(|c| c.predicate.as_str() == "IdentificationDecision"),
        "the decision was a valid record at its own transition, and stays one"
    );
}
