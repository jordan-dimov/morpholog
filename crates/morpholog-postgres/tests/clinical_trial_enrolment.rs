//! Integration test for the clinical-trial-enrolment example
//! through `propose_against_pg`. Confirms that `Value::Date` /
//! `EvalValue::Date` round-trip through the PG JSONB `arguments`
//! columns (claims and audit) without loss, and that the
//! load-bearing `randomise_participant` transformation commits the
//! expected claim, audit row and outbox intent.
//!
//! Scope is the happy path only. Per-gate rejection paths are
//! covered by the in-memory tests in
//! `crates/morpholog-examples/tests/clinical_trial_enrolment.rs`;
//! that surface does not change between in-memory and PG
//! evaluation, so re-running every rejection through PG would buy
//! no extra signal.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{ClaimInstance, EvalValue};
use morpholog_examples::clinical_trial_enrolment::{
    self as cte, ROLE_RANDOMISE_PARTICIPANT, all_invariants, randomise_participant,
};
use morpholog_postgres::{PgPool, PgProposalOutcome, list_claims, list_pending_outbox};
use uuid::Uuid;

mod common;
use common::{date, subj};

async fn test_pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must be set for morpholog-postgres integration tests \
         (e.g. postgres:///morpholog_dev)",
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

/// Helper: run a setup transformation and assert it committed,
/// returning the next test step's pre-state implicitly (PG holds
/// state in tables; no value returned). Panics on rejection.
async fn commit(pool: &PgPool, t: &morpholog_core::Transformation, args: Vec<EvalValue>) -> Uuid {
    let outcome = common::propose_pg_with_test_actor(pool, t, args, &all_invariants())
        .await
        .expect("propose_against_pg should not error");
    match outcome {
        PgProposalOutcome::Committed { transition_id, .. } => transition_id,
        PgProposalOutcome::Rejected { reason } => {
            panic!(
                "expected Committed from `{}`, got Rejected: {reason}",
                t.name
            )
        }
    }
}

#[tokio::test]
async fn randomise_participant_happy_path_through_pg() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let investigator = "dr_smith";
    let trial = "trial_001";
    let proto_v1 = "proto_v1";
    let consent_form = "icf_v1";
    let participant = "p_001";
    let randomised_on = "2026-03-12";

    // Setup chain: every claim the load-bearing require gates on.
    commit(&pool, &cte::open_trial(), vec![subj(trial)]).await;
    commit(
        &pool,
        &cte::approve_protocol_version(),
        vec![
            subj(trial),
            subj(proto_v1),
            date("2026-01-01"),
            date("2026-03-31"),
            subj("ethics_committee_uk"),
            subj("approval_001"),
        ],
    )
    .await;
    commit(
        &pool,
        &cte::approve_consent_form_version(),
        vec![
            subj(trial),
            subj(consent_form),
            date("2026-01-01"),
            date("2026-03-31"),
            subj("ethics_committee_uk"),
            subj("approval_002"),
        ],
    )
    .await;
    commit(
        &pool,
        &cte::delegate_investigator(),
        vec![
            subj(investigator),
            subj(trial),
            subj(ROLE_RANDOMISE_PARTICIPANT),
            date("2026-01-01"),
            date("2026-12-31"),
        ],
    )
    .await;
    commit(
        &pool,
        &cte::record_consent(),
        vec![
            subj(participant),
            subj(trial),
            subj(consent_form),
            date("2026-03-08"),
            subj(investigator),
        ],
    )
    .await;
    commit(
        &pool,
        &cte::record_eligibility_criterion(),
        vec![subj(proto_v1), subj("creatinine_panel"), subj("PASS")],
    )
    .await;
    commit(
        &pool,
        &cte::record_eligibility_assessment(),
        vec![
            subj(participant),
            subj("creatinine_panel"),
            subj("PASS"),
            date("2026-03-09"),
            date("2026-03-23"),
        ],
    )
    .await;

    // Load-bearing call: randomise_participant with the investigator
    // as the actor. The DelegatedInvestigator gate consults Term::Actor;
    // a different actor would reject here.
    let outcome = common::propose_pg_as(
        &pool,
        &randomise_participant(),
        vec![
            subj(participant),
            subj(trial),
            subj(proto_v1),
            date(randomised_on),
        ],
        investigator,
        &all_invariants(),
    )
    .await
    .expect("propose_against_pg must not error");

    let PgProposalOutcome::Committed {
        asserted_claims,
        emitted_intents,
        ..
    } = outcome
    else {
        panic!("expected Committed, got {outcome:?}");
    };
    assert_eq!(
        asserted_claims.len(),
        1,
        "randomise_participant asserts exactly ParticipantRandomised"
    );
    assert_eq!(emitted_intents.len(), 1);
    assert_eq!(emitted_intents[0].name, "ParticipantRandomised");

    // Read the ParticipantRandomised claim back through the read
    // path; the date arg must survive the JSONB round-trip exactly.
    let claims = list_claims(&pool).await.unwrap();
    let admitted: &ClaimInstance = claims
        .iter()
        .find(|c| c.predicate == "ParticipantRandomised")
        .expect("ParticipantRandomised must be admitted");
    assert_eq!(
        admitted.args,
        vec![
            subj(participant),
            subj(trial),
            subj(proto_v1),
            date(randomised_on),
            subj(investigator),
        ],
        "civil-date arg must round-trip through PG JSONB without loss"
    );

    // The outbox row carries the intent.
    let pending = list_pending_outbox(&pool).await.unwrap();
    assert!(
        pending
            .iter()
            .any(|i| i.intent_type == "ParticipantRandomised"),
        "ParticipantRandomised intent must be in the outbox; got: {pending:?}"
    );
}
