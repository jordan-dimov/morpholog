//! A closed deployment: every actor needs a grant, and the root actor is
//! asserted by its root login alone.
//!
//! **The attacker this models.** An application that legitimately holds a
//! Morpholog connection under its own login and proposes through the
//! adapter, naming an actor no grant gives it: another service, an actor
//! never seen before, or the root itself, even under a governed claim that
//! looks like a grant for the root. Also a login dropped and made again
//! under the root login's name.
//!
//! **Not modelled.** A caller running its own SQL with the writer's
//! credentials, which never meets this check (see `actor_policy.rs`).
//!
//! Each simulated gateway takes its login with `SET SESSION AUTHORIZATION`,
//! which needs superuser, hence the skip.
#![expect(
    clippy::unwrap_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

mod common;

use common::{
    expect_committed, gateway_pool, pg_program, propose_pg_as, reset_db, run_sql,
    session_is_superuser, subj, test_pool, with_gateway_roles,
};
use morpholog_core::{EvalValue, Subject, Transition};
use morpholog_postgres::{
    CloseOutcome, DeploymentIdentity, PgError, PgPool, PgProgram, Proposal, actors_without_grant,
    close_deployment, deployment_identity, propose_all_against_pg,
};

const ROOT: &str = "root_admin";

/// Roles belong to the whole cluster, so each test names its own.
fn login(tag: &str, who: &str) -> String {
    format!("mtest_idc_{tag}_{who}")
}

/// The root appoints itself, then grants; anyone may write a note.
const PROGRAMME: &str = "\
program closed_deployment

predicate ActorAssertionAuthority(actor: Subject, login_role: Subject)
predicate Administrator(admin: Subject)
predicate Note(text: Subject, author: Subject)

transformation appoint(admin):
    require not Administrator(_)
    require actor = admin
    admit Administrator(admin)

transformation grant(person, login_role):
    require Administrator(actor)
    admit ActorAssertionAuthority(person, login_role)

transformation note(text):
    admit Note(text, actor)
";

fn program() -> PgProgram {
    pg_program(morpholog_surface::parse_program(PROGRAMME).unwrap())
}

async fn act(
    pool: &PgPool,
    name: &str,
    args: Vec<EvalValue>,
    actor: &str,
) -> Result<morpholog_postgres::PgProposalOutcome, PgError> {
    let p = program();
    let t = p.prepared().transformation(&name.into()).unwrap().clone();
    propose_pg_as(pool, &p, &t, args, actor).await
}

async fn note(pool: &PgPool, text: &str, actor: &str) -> Result<(), PgError> {
    act(pool, "note", vec![subj(text)], actor).await.map(|o| {
        expect_committed(o);
    })
}

/// Close under the root login, then let the root appoint itself and grant
/// `service` to `service_login`.
async fn close_and_grant(
    admin: &PgPool,
    root: &PgPool,
    root_login: &str,
    service: &str,
    service_login: &str,
) {
    assert_eq!(
        close_deployment(admin, &Subject::from(ROOT), root_login)
            .await
            .unwrap(),
        CloseOutcome::Closed
    );
    expect_committed(act(root, "appoint", vec![subj(ROOT)], ROOT).await.unwrap());
    expect_committed(
        act(
            root,
            "grant",
            vec![subj(service), subj(service_login)],
            ROOT,
        )
        .await
        .unwrap(),
    );
}

async fn superuser_pool() -> Option<PgPool> {
    let pool = test_pool().await;
    if !session_is_superuser(&pool).await {
        eprintln!("skipping: SET SESSION AUTHORIZATION needs a superuser");
        return None;
    }
    reset_db(&pool).await;
    Some(pool)
}

#[tokio::test]
async fn a_closed_deployment_admits_its_root_and_whom_the_root_grants() {
    let Some(admin) = superuser_pool().await else {
        return;
    };
    let (root_login, svc_login) = (login("grant", "root"), login("grant", "svc"));
    let logins = [root_login.as_str(), svc_login.as_str()];
    let scenario = {
        let admin = admin.clone();
        let (root_login, svc_login) = (root_login.clone(), svc_login.clone());
        async move {
            let (root, svc) = (
                gateway_pool(&root_login).await,
                gateway_pool(&svc_login).await,
            );
            close_and_grant(&admin, &root, &root_login, "risk_engine", &svc_login).await;
            assert!(matches!(
                deployment_identity(&admin).await.unwrap(),
                DeploymentIdentity::Closed { root_actor, root_login: l }
                    if root_actor.as_str() == ROOT && l == root_login
            ));

            note(&svc, "fills reconciled", "risk_engine").await.unwrap();
            // An actor nobody granted, never seen before.
            assert!(matches!(
                note(&svc, "posing", "lapse_sweep").await,
                Err(PgError::ActorAssertionUnauthorised { .. })
            ));
            // The root, from a login that is not the root login.
            assert!(matches!(
                note(&svc, "posing", ROOT).await,
                Err(PgError::RootActorUnauthorised { .. })
            ));
            // The same refusal inside one atomic batch, at its second act.
            let p = program();
            let batch = [
                Proposal::gateway(&note_transition("first", "risk_engine")),
                Proposal::gateway(&note_transition("second", "lapse_sweep")),
            ];
            assert!(matches!(
                propose_all_against_pg(&svc, &p, &batch).await,
                Err(PgError::ActorAssertionUnauthorised { .. })
            ));
        }
    };
    with_gateway_roles(&admin, &logins, scenario).await;
}

fn note_transition(text: &str, actor: &str) -> Transition {
    Transition {
        transformation_name: "note".into(),
        args: vec![subj(text)],
        actor: Subject::from(actor),
        decided_at: None,
    }
}

#[tokio::test]
async fn no_claim_grants_the_root_to_another_login() {
    let Some(admin) = superuser_pool().await else {
        return;
    };
    let (root_login, svc_login) = (login("noclaim", "root"), login("noclaim", "svc"));
    let logins = [root_login.as_str(), svc_login.as_str()];
    let scenario = {
        let admin = admin.clone();
        let (root_login, svc_login) = (root_login.clone(), svc_login.clone());
        async move {
            let (root, svc) = (
                gateway_pool(&root_login).await,
                gateway_pool(&svc_login).await,
            );
            close_and_grant(&admin, &root, &root_login, ROOT, &svc_login).await;
            assert!(matches!(
                note(&svc, "posing", ROOT).await,
                Err(PgError::RootActorUnauthorised { .. })
            ));
            note(&root, "still the root", ROOT).await.unwrap();
        }
    };
    with_gateway_roles(&admin, &logins, scenario).await;
}

/// The acceptance side: open, an actor nobody restricted is asserted by
/// any login, as before.
#[tokio::test]
async fn an_open_deployment_decides_as_before() {
    let Some(admin) = superuser_pool().await else {
        return;
    };
    let svc_login = login("open", "svc");
    let logins = [svc_login.as_str()];
    let scenario = {
        let admin = admin.clone();
        let svc_login = svc_login.clone();
        async move {
            assert_eq!(
                deployment_identity(&admin).await.unwrap(),
                DeploymentIdentity::Open
            );
            let svc = gateway_pool(&svc_login).await;
            note(&svc, "anyone", "lapse_sweep").await.unwrap();
            note(&svc, "even this", ROOT).await.unwrap();
        }
    };
    with_gateway_roles(&admin, &logins, scenario).await;
}

#[tokio::test]
async fn a_login_made_again_under_the_root_login_name_is_not_the_root() {
    let Some(admin) = superuser_pool().await else {
        return;
    };
    let root_login = login("again", "root");
    let logins = [root_login.as_str()];
    let scenario = {
        let admin = admin.clone();
        let root_login = root_login.clone();
        async move {
            close_deployment(&admin, &Subject::from(ROOT), &root_login)
                .await
                .unwrap();
            run_sql(
                &admin,
                &format!("DROP OWNED BY {root_login}; DROP ROLE {root_login}"),
            )
            .await;
            common::recreate_gateway_roles(&admin, &[root_login.as_str()]).await;
            let root = gateway_pool(&root_login).await;
            assert!(matches!(
                note(&root, "an impostor", ROOT).await,
                Err(PgError::RootActorUnauthorised { .. })
            ));
        }
    };
    with_gateway_roles(&admin, &logins, scenario).await;
}

#[tokio::test]
async fn a_deployment_without_its_identity_row_refuses_every_proposal() {
    let Some(admin) = superuser_pool().await else {
        return;
    };
    run_sql(&admin, "DELETE FROM morpholog.deployment_identity").await;
    assert!(matches!(
        note(&admin, "anything", "anyone").await,
        Err(PgError::DeploymentIdentityMissing)
    ));
    assert!(matches!(
        deployment_identity(&admin).await,
        Err(PgError::DeploymentIdentityMissing)
    ));
    // Closing writes the row back, closed, never open.
    let me: String = sqlx::query_scalar("SELECT session_user::text")
        .fetch_one(&admin)
        .await
        .unwrap();
    close_deployment(&admin, &Subject::from(ROOT), &me)
        .await
        .unwrap();
    assert!(matches!(
        deployment_identity(&admin).await.unwrap(),
        DeploymentIdentity::Closed { .. }
    ));
    reset_db(&admin).await;
}

#[tokio::test]
async fn closing_again_under_the_same_root_changes_nothing_and_another_is_refused() {
    let Some(admin) = superuser_pool().await else {
        return;
    };
    let me: String = sqlx::query_scalar("SELECT session_user::text")
        .fetch_one(&admin)
        .await
        .unwrap();
    let root = Subject::from(ROOT);
    assert!(matches!(
        close_deployment(&admin, &root, "mtest_idc_no_such_login").await,
        Err(PgError::RootLoginMissing(_))
    ));
    assert_eq!(
        deployment_identity(&admin).await.unwrap(),
        DeploymentIdentity::Open
    );
    assert_eq!(
        close_deployment(&admin, &root, &me).await.unwrap(),
        CloseOutcome::Closed
    );
    assert_eq!(
        close_deployment(&admin, &root, &me).await.unwrap(),
        CloseOutcome::AlreadyClosed
    );
    assert!(matches!(
        close_deployment(&admin, &Subject::from("someone_else"), &me).await,
        Err(PgError::DeploymentRootDiffers { .. })
    ));
    reset_db(&admin).await;
}

#[tokio::test]
async fn the_known_actors_without_a_grant_are_listed_before_closing() {
    let Some(admin) = superuser_pool().await else {
        return;
    };
    note(&admin, "a", "alice").await.unwrap();
    note(&admin, "b", "bob").await.unwrap();
    note(&admin, "r", ROOT).await.unwrap();
    expect_committed(
        act(&admin, "appoint", vec![subj(ROOT)], ROOT)
            .await
            .unwrap(),
    );
    expect_committed(
        act(
            &admin,
            "grant",
            vec![subj("alice"), subj("svc_login")],
            ROOT,
        )
        .await
        .unwrap(),
    );
    let ungranted = actors_without_grant(&admin, &Subject::from(ROOT))
        .await
        .unwrap();
    assert_eq!(ungranted, vec![Subject::from("bob")]);
}
