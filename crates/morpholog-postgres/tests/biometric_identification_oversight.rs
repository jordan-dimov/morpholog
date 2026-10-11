//! The biometric-identification-oversight example (EU AI Act Articles
//! 12 / 14(5)) through `propose_against_pg`.
//!
//! Covers what the in-memory tests cannot: the whole lifecycle commits
//! through PostgreSQL, timestamps and durations round-trip, and as-of
//! replay recovers the oversight in force at a past transition, so a
//! revocation today does not rewrite who was authorised then, and each
//! name reaches the record only over a login granted it. Gate
//! rejections are tested in memory.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_examples::biometric_identification_oversight as bio;
use morpholog_postgres::{
    PgError, PgPool, PgProposalOutcome, Proposal, list_claims, list_claims_at, propose_against_pg,
    reconstruct_state_at,
};
use uuid::Uuid;

mod common;
use common::{
    drop_roles_if_present, expect_committed, gateway_pool, propose_pg_as, recreate_gateway_roles,
    session_is_superuser, subj,
};
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
    let outcome = propose_pg_as(pool, &common::pg_program(bio::program()), t, args, actor)
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

/// The full lifecycle: deploy, assess a version, assign two overseers,
/// start a use, record a machine match, two distinct overseers verify,
/// a decision commits. Returns the decision's transition id.
async fn run_to_decision(pool: &PgPool) -> Uuid {
    // One login stands in for every party here; the separate-logins test
    // below is the one about who may speak for whom.
    let me: String = sqlx::query_scalar("SELECT session_user::text")
        .fetch_one(pool)
        .await
        .unwrap();
    commit_as(
        pool,
        &bio::deploy_system(),
        vec![
            subj("cam_system"),
            subj("city_operator"),
            subj(&me),
            subj(&me),
        ],
        "city_operator",
    )
    .await;
    for person in ["anna", "ben"] {
        commit_as(
            pool,
            &bio::restrict_verifier_identity(),
            vec![subj(person), subj("cam_system")],
            "city_operator",
        )
        .await;
        commit_as(
            pool,
            &bio::authorise_verifier_login(),
            vec![subj(person), subj(&me), subj("cam_system")],
            "city_operator",
        )
        .await;
    }
    commit_as(
        pool,
        &bio::place_version_in_service(),
        vec![
            subj("cam_system"),
            subj("model_v1"),
            ts("2026-10-01T00:00:00Z"),
            ts("2026-10-31T23:59:59Z"),
        ],
        "city_operator",
    )
    .await;
    commit_as(
        pool,
        &bio::assign_oversight(),
        vec![subj("anna"), subj("cam_system")],
        "city_operator",
    )
    .await;
    commit_as(
        pool,
        &bio::assign_oversight(),
        vec![subj("ben"), subj("cam_system")],
        "city_operator",
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

    // Anna's authority is revoked: her training lapsed. It governs only
    // the future.
    commit_as(
        &pool,
        &bio::revoke_oversight(),
        vec![subj("anna"), subj("cam_system")],
        "city_operator",
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

    // The Article 86 enquiry: as of the decision, anna's oversight WAS
    // in force, whatever was revoked since.
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

type Args = Vec<morpholog_core::EvalValue>;

fn proposal(t: &morpholog_core::Transformation, args: Args, actor: &str) -> Proposal {
    Proposal::gateway(&morpholog_core::Transition {
        transformation_name: t.name.clone(),
        args,
        actor: actor.into(),
    })
}

async fn commit(pool: &PgPool, t: morpholog_core::Transformation, args: Args, actor: &str) {
    let p = common::pg_program(bio::program());
    expect_committed(
        propose_against_pg(pool, &p, &proposal(&t, args, actor))
            .await
            .unwrap(),
    );
}

async fn refused_as(
    pool: &PgPool,
    t: morpholog_core::Transformation,
    args: Args,
    actor: &str,
    login: &str,
) {
    let p = common::pg_program(bio::program());
    let outcome = propose_against_pg(pool, &p, &proposal(&t, args, actor)).await;
    assert!(
        matches!(&outcome, Err(PgError::ActorAssertionUnauthorised { actor: a, login_role })
            if a.as_str() == actor && login_role == login),
        "{login} speaking as {actor}: {outcome:?}"
    );
}

async fn row_counts(pool: &PgPool) -> Vec<i64> {
    let mut counts = Vec::new();
    for table in ["audit", "rejections", "outbox"] {
        counts.push(
            sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM morpholog.{table}"
            )))
            .fetch_one(pool)
            .await
            .unwrap(),
        );
    }
    counts
}

async fn grants_of(pool: &PgPool, person: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM morpholog.claims
         WHERE predicate_name = 'ActorAssertionAuthority' AND arguments -> 0 ->> 'value' = $1",
    )
    .bind(person)
    .fetch_one(pool)
    .await
    .unwrap()
}

// The attacker modelled: an application holding its own Morpholog login,
// proposing through the adapter under a name it was not granted - the
// system's, another verifier's, or one it tries to grant itself through a
// second deployment. Not modelled: a gateway writing SQL directly.
#[tokio::test]
async fn each_name_reaches_the_record_only_over_its_own_login() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !session_is_superuser(&pool).await {
        eprintln!("skipping: needs a superuser test role");
        return;
    }
    let logins = [
        "mtest_bio_dep",
        "mtest_bio_sys",
        "mtest_bio_anna",
        "mtest_bio_ben",
        "mtest_bio_rogue",
    ];
    recreate_gateway_roles(&pool, &logins).await;
    let deployer = gateway_pool(logins[0]).await;
    let system = gateway_pool(logins[1]).await;
    let anna = gateway_pool(logins[2]).await;
    let ben = gateway_pool(logins[3]).await;
    let rogue = gateway_pool(logins[4]).await;

    commit(
        &deployer,
        bio::deploy_system(),
        vec![
            subj("cam_system"),
            subj("city_operator"),
            subj(logins[0]),
            subj(logins[1]),
        ],
        "city_operator",
    )
    .await;
    commit(
        &deployer,
        bio::place_version_in_service(),
        vec![
            subj("cam_system"),
            subj("model_v1"),
            ts("2026-10-01T00:00:00Z"),
            ts("2026-10-31T23:59:59Z"),
        ],
        "city_operator",
    )
    .await;
    for (person, login) in [("anna", logins[2]), ("ben", logins[3])] {
        let on_system = vec![subj(person), subj("cam_system")];
        commit(
            &deployer,
            bio::restrict_verifier_identity(),
            on_system.clone(),
            "city_operator",
        )
        .await;
        commit(
            &deployer,
            bio::authorise_verifier_login(),
            vec![subj(person), subj(login), subj("cam_system")],
            "city_operator",
        )
        .await;
        commit(
            &deployer,
            bio::assign_oversight(),
            on_system,
            "city_operator",
        )
        .await;
    }
    commit(
        &system,
        bio::start_use(),
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
    let match_args = |m: &str, at: &str| -> Args {
        vec![
            subj(m),
            subj("use_1"),
            subj("frame_4411"),
            subj("candidate_7"),
            ts(at),
        ]
    };

    // A rogue login cannot put a match on the record under the system's
    // name, and the refusal leaves nothing behind.
    let before = row_counts(&pool).await;
    refused_as(
        &rogue,
        bio::record_match(),
        match_args("match_forged", "2026-10-12T09:15:00Z"),
        "cam_system",
        logins[4],
    )
    .await;
    assert_eq!(
        row_counts(&pool).await,
        before,
        "a refused assertion records nothing"
    );

    commit(
        &system,
        bio::record_match(),
        match_args("match_1", "2026-10-12T09:30:00Z"),
        "cam_system",
    )
    .await;
    let verify = |at: &str| -> Args { vec![subj("match_1"), ts(at)] };
    commit(
        &anna,
        bio::verify_match(),
        verify("2026-10-12T10:00:00Z"),
        "anna",
    )
    .await;
    // Anna's login cannot be Ben's second voice.
    refused_as(
        &anna,
        bio::verify_match(),
        verify("2026-10-12T10:10:00Z"),
        "ben",
        logins[2],
    )
    .await;
    commit(
        &ben,
        bio::verify_match(),
        verify("2026-10-12T10:20:00Z"),
        "ben",
    )
    .await;
    commit(
        &anna,
        bio::decide_on_identification(),
        vec![
            subj("decision_1"),
            subj("match_1"),
            subj("confirmed_identification"),
            ts("2026-10-12T11:00:00Z"),
        ],
        "anna",
    )
    .await;

    // A second deployment cannot hand the rogue an existing name: there
    // is one deployment, so it is refused, and anna's grants stand as
    // they were.
    let anna_grants = grants_of(&pool, "anna").await;
    for deployer_name in ["mallory", "anna"] {
        let outcome = propose_against_pg(
            &rogue,
            &common::pg_program(bio::program()),
            &proposal(
                &bio::deploy_system(),
                vec![
                    subj("camera_b"),
                    subj(deployer_name),
                    subj(logins[4]),
                    subj(logins[4]),
                ],
                deployer_name,
            ),
        )
        .await;
        assert!(
            matches!(
                outcome,
                Ok(PgProposalOutcome::Rejected { .. })
                    | Err(PgError::ActorAssertionUnauthorised { .. })
            ),
            "a second deployment naming {deployer_name}: {outcome:?}"
        );
    }
    assert_eq!(
        grants_of(&pool, "anna").await,
        anna_grants,
        "nobody gained anna's name"
    );

    // Withdrawing anna's only login locks her name rather than freeing it.
    commit(
        &deployer,
        bio::withdraw_verifier_login(),
        vec![subj("anna"), subj(logins[2]), subj("cam_system")],
        "city_operator",
    )
    .await;
    commit(
        &system,
        bio::record_match(),
        match_args("match_2", "2026-10-12T12:00:00Z"),
        "cam_system",
    )
    .await;
    let second: Args = vec![subj("match_2"), ts("2026-10-12T12:30:00Z")];
    refused_as(
        &anna,
        bio::verify_match(),
        second.clone(),
        "anna",
        logins[2],
    )
    .await;
    refused_as(&rogue, bio::verify_match(), second, "anna", logins[4]).await;

    for gateway in [deployer, system, anna, ben, rogue] {
        gateway.close().await;
    }
    drop_roles_if_present(&pool, &logins).await;
}
