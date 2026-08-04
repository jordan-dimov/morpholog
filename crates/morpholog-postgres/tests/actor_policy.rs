//! Actor-assertion policy: which login role may propose as which actor.
//!
//! **The attacker this models.** An application that legitimately holds
//! a Morpholog connection and proposes through the adapter, naming an
//! actor it holds no authority for - the operator whose one gateway can
//! sign as two different people, and so satisfy a two-distinct-person
//! rule alone.
//!
//! **What it deliberately does NOT model.** A compromised gateway
//! executing its own SQL. The runtime's writer role holds
//! `INSERT`/`DELETE` on `morpholog.claims` and `INSERT` on
//! `morpholog.audit`, so code holding those credentials writes claims
//! and attestation-shaped audit rows directly and never passes this
//! check at all. The guarantee is about callers reaching the record
//! through the adapter, and it is only as good as the separation
//! between the gateway processes and their credentials.
//!
//! The check reads `session_user`, so each simulated gateway is a pool
//! whose connections take on their own authenticated identity via
//! `SET SESSION AUTHORIZATION`. That needs superuser, hence the skip -
//! and it is the same accepted residue the writer-role census records:
//! `session_user` resists `SET ROLE`, not a superuser.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{compiled, expect_committed, propose_pg_as, reset_db, test_pool};
use morpholog_core::{CompiledProgram, EvalValue, Subject, Transformation, Transition};
use morpholog_postgres::{
    PgError, PgPool, PgProposalOutcome, Proposal, propose_against_pg, propose_against_pg_with_trace,
};
use sqlx::postgres::PgPoolOptions;

const DEPLOYER: &str = "deployer_1";
const SYSTEM: &str = "system_1";
const CHEN: &str = "verifier_chen";
const OKAFOR: &str = "verifier_okafor";

fn program() -> CompiledProgram {
    compiled(morpholog_examples::biometric_identification_oversight::program())
}

fn subj(s: &str) -> EvalValue {
    EvalValue::Subject(Subject::from(s))
}

async fn is_superuser(pool: &PgPool) -> bool {
    let (rolsuper,): (bool,) =
        sqlx::query_as("SELECT rolsuper FROM pg_roles WHERE rolname = session_user")
            .fetch_one(pool)
            .await
            .unwrap();
    rolsuper
}

/// Roles are cluster-global and survive `reset_db`, so each run drops
/// and recreates its own.
async fn recreate_roles(pool: &PgPool, roles: &[&str]) {
    for role in roles {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DO $$ BEGIN
                IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{role}') THEN
                    EXECUTE 'DROP OWNED BY {role}';
                    EXECUTE 'DROP ROLE {role}';
                END IF;
             END $$"
        )))
        .execute(pool)
        .await
        .unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE ROLE {role} LOGIN")))
            .execute(pool)
            .await
            .unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "GRANT ALL ON ALL TABLES IN SCHEMA morpholog TO {role}"
        )))
        .execute(pool)
        .await
        .unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "GRANT USAGE ON SCHEMA morpholog TO {role}"
        )))
        .execute(pool)
        .await
        .unwrap();
    }
}

/// Roles are cluster-global, so every test drops its own on the way
/// out. A leaked role that can write `morpholog.audit` is not inert:
/// it joins the writer-role census and fails an assertion in another
/// suite entirely.
async fn drop_roles(pool: &PgPool, roles: &[&str]) {
    for role in roles {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DO $$ BEGIN
                IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{role}') THEN
                    EXECUTE 'DROP OWNED BY {role}';
                    EXECUTE 'DROP ROLE {role}';
                END IF;
             END $$"
        )))
        .execute(pool)
        .await
        .unwrap();
    }
}

/// A pool that presents itself as `role`: one simulated gateway.
async fn gateway_pool(role: &'static str) -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let url = morpholog_postgres::with_default_user(&url);
    PgPoolOptions::new()
        .max_connections(2)
        .after_connect(move |conn, _| {
            Box::pin(async move {
                sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                    "SET SESSION AUTHORIZATION {role}"
                )))
                .execute(&mut *conn)
                .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap()
}

/// The system on the record, both verifiers holding live oversight.
async fn deploy(pool: &PgPool) {
    let p = program();
    for (name, args) in [
        ("deploy_system", vec![subj(SYSTEM), subj(DEPLOYER)]),
        ("assign_oversight", vec![subj(CHEN), subj(SYSTEM)]),
        ("assign_oversight", vec![subj(OKAFOR), subj(SYSTEM)]),
    ] {
        let t = p.transformation(&name.into()).unwrap().clone();
        let actor = if name == "deploy_system" {
            DEPLOYER
        } else {
            CHEN
        };
        expect_committed(propose_pg_as(pool, &p, &t, args, actor).await.unwrap());
    }
}

async fn arm(pool: &PgPool, person: &str) {
    let p = program();
    let t = p
        .transformation(&"restrict_verifier_identity".into())
        .unwrap()
        .clone();
    expect_committed(
        propose_pg_as(pool, &p, &t, vec![subj(person), subj(SYSTEM)], DEPLOYER)
            .await
            .unwrap(),
    );
}

async fn grant(pool: &PgPool, person: &str, role: &str) {
    let p = program();
    let t = p
        .transformation(&"authorise_verifier_login".into())
        .unwrap()
        .clone();
    expect_committed(
        propose_pg_as(
            pool,
            &p,
            &t,
            vec![subj(person), subj(role), subj(SYSTEM)],
            DEPLOYER,
        )
        .await
        .unwrap(),
    );
}

fn proposal(t: &Transformation, args: Vec<EvalValue>, actor: &str) -> Proposal {
    Proposal::gateway(&Transition {
        transformation_name: t.name.clone(),
        args,
        actor: Subject::from(actor),
    })
}

/// `assign_oversight` under some actor - the simplest governed act to
/// aim at an actor label.
fn oversight_of(person: &str) -> (Transformation, Vec<EvalValue>) {
    let p = program();
    let t = p
        .transformation(&"assign_oversight".into())
        .unwrap()
        .clone();
    (t, vec![subj(person), subj(SYSTEM)])
}

#[tokio::test]
async fn an_unarmed_actor_is_asserted_by_anyone_exactly_as_before() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !is_superuser(&pool).await {
        eprintln!("skipping: needs a superuser test role");
        return;
    }
    recreate_roles(&pool, &["mtest_gw_a"]).await;
    deploy(&pool).await;

    // No ActorAssertionRestricted claim exists, so nothing is armed:
    // the promise to every deployment that adopts nothing.
    let gateway = gateway_pool("mtest_gw_a").await;
    let (t, args) = oversight_of("verifier_new");
    let outcome = propose_against_pg(&gateway, &program(), &proposal(&t, args, CHEN))
        .await
        .expect("an unarmed actor stays assertable by any role");
    assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));

    drop_roles(&pool, &["mtest_gw_a"]).await;
}

#[tokio::test]
async fn an_armed_actor_is_refused_to_an_unauthorised_role_and_records_nothing() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !is_superuser(&pool).await {
        eprintln!("skipping: needs a superuser test role");
        return;
    }
    recreate_roles(&pool, &["mtest_gw_chen", "mtest_gw_rogue"]).await;
    deploy(&pool).await;
    arm(&pool, CHEN).await;
    grant(&pool, CHEN, "mtest_gw_chen").await;

    let audit_before = audit_count(&pool).await;
    let rejections_before = rejection_count(&pool).await;

    let rogue = gateway_pool("mtest_gw_rogue").await;
    let (t, args) = oversight_of("verifier_new");
    let err = propose_against_pg(&rogue, &program(), &proposal(&t, args, CHEN))
        .await
        .expect_err("an unauthorised role may not speak for an armed actor");
    assert!(
        matches!(&err, PgError::ActorAssertionUnauthorised { actor, login_role }
            if actor.as_str() == CHEN && login_role == "mtest_gw_rogue"),
        "{err:?}"
    );

    // Never attributed: refusing an unauthorised assertion must not
    // manufacture a record that the actor attempted anything.
    assert_eq!(audit_count(&pool).await, audit_before, "audit grew");
    assert_eq!(
        rejection_count(&pool).await,
        rejections_before,
        "the rejection log is for business refusals only"
    );
    assert_eq!(outbox_count(&pool).await, 0, "outbox grew");

    drop_roles(&pool, &["mtest_gw_chen", "mtest_gw_rogue"]).await;
}

#[tokio::test]
async fn the_authorised_role_proceeds() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !is_superuser(&pool).await {
        eprintln!("skipping: needs a superuser test role");
        return;
    }
    recreate_roles(&pool, &["mtest_gw_chen2"]).await;
    deploy(&pool).await;
    arm(&pool, CHEN).await;
    grant(&pool, CHEN, "mtest_gw_chen2").await;

    let gateway = gateway_pool("mtest_gw_chen2").await;
    let (t, args) = oversight_of("verifier_new");
    let outcome = propose_against_pg(&gateway, &program(), &proposal(&t, args, CHEN))
        .await
        .expect("the granted role speaks for its actor");
    assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));

    drop_roles(&pool, &["mtest_gw_chen2"]).await;
}

#[tokio::test]
async fn asking_for_a_trace_does_not_get_round_the_policy() {
    // The traced path opens its own transaction and calls the kernel
    // itself. A check wired into the ordinary path alone would be a
    // gate you walk around by adding --trace.
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !is_superuser(&pool).await {
        eprintln!("skipping: needs a superuser test role");
        return;
    }
    recreate_roles(&pool, &["mtest_gw_trace"]).await;
    deploy(&pool).await;
    arm(&pool, CHEN).await;

    let rogue = gateway_pool("mtest_gw_trace").await;
    let (t, args) = oversight_of("verifier_new");
    let err = propose_against_pg_with_trace(&rogue, &program(), &proposal(&t, args, CHEN))
        .await
        .expect_err("the traced path must take the same gate");
    assert!(
        matches!(&err, PgError::ActorAssertionUnauthorised { .. }),
        "{err:?}"
    );

    drop_roles(&pool, &["mtest_gw_trace"]).await;
}

#[tokio::test]
async fn withdrawing_the_last_grant_locks_the_actor_out_rather_than_freeing_it() {
    // The whole reason the arming claim is separate from the grants.
    // If the grants did the arming, this sequence would hand the name
    // back to every gateway at the moment of revocation.
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !is_superuser(&pool).await {
        eprintln!("skipping: needs a superuser test role");
        return;
    }
    recreate_roles(&pool, &["mtest_gw_last", "mtest_gw_other"]).await;
    deploy(&pool).await;
    arm(&pool, CHEN).await;
    grant(&pool, CHEN, "mtest_gw_last").await;

    let p = program();
    let withdraw = p
        .transformation(&"withdraw_verifier_login".into())
        .unwrap()
        .clone();
    expect_committed(
        propose_pg_as(
            &pool,
            &p,
            &withdraw,
            vec![subj(CHEN), subj("mtest_gw_last"), subj(SYSTEM)],
            DEPLOYER,
        )
        .await
        .unwrap(),
    );

    // The formerly-granted gateway is now refused...
    let former = gateway_pool("mtest_gw_last").await;
    let (t, args) = oversight_of("verifier_new");
    assert!(
        matches!(
            propose_against_pg(&former, &program(), &proposal(&t, args, CHEN)).await,
            Err(PgError::ActorAssertionUnauthorised { .. })
        ),
        "the withdrawn grant must stop working"
    );
    // ...and so is everyone else. No downgrade to open.
    let other = gateway_pool("mtest_gw_other").await;
    let (t, args) = oversight_of("verifier_new2");
    assert!(
        matches!(
            propose_against_pg(&other, &program(), &proposal(&t, args, CHEN)).await,
            Err(PgError::ActorAssertionUnauthorised { .. })
        ),
        "withdrawing the last grant must LOCK the actor, never free it"
    );

    drop_roles(&pool, &["mtest_gw_last", "mtest_gw_other"]).await;
}

#[tokio::test]
async fn set_role_does_not_change_who_the_policy_thinks_you_are() {
    // session_user is what the policy reads precisely because SET ROLE
    // cannot touch it.
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !is_superuser(&pool).await {
        eprintln!("skipping: needs a superuser test role");
        return;
    }
    recreate_roles(&pool, &["mtest_gw_real", "mtest_gw_borrowed"]).await;
    deploy(&pool).await;
    arm(&pool, CHEN).await;
    grant(&pool, CHEN, "mtest_gw_borrowed").await;
    // Membership, so the SET ROLE below genuinely succeeds: the point
    // is not that borrowing is blocked, it is that borrowing does not
    // change who the policy judges.
    sqlx::raw_sql(sqlx::AssertSqlSafe(
        "GRANT mtest_gw_borrowed TO mtest_gw_real".to_string(),
    ))
    .execute(&pool)
    .await
    .unwrap();

    let url = std::env::var("DATABASE_URL").unwrap();
    let url = morpholog_postgres::with_default_user(&url);
    let borrowing = PgPoolOptions::new()
        .max_connections(1)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::raw_sql(sqlx::AssertSqlSafe(
                    "SET SESSION AUTHORIZATION mtest_gw_real; SET ROLE mtest_gw_borrowed"
                        .to_string(),
                ))
                .execute(&mut *conn)
                .await?;
                Ok(())
            })
        })
        .connect(&url)
        .await
        .unwrap();

    let (t, args) = oversight_of("verifier_new");
    let err = propose_against_pg(&borrowing, &program(), &proposal(&t, args, CHEN))
        .await
        .expect_err("SET ROLE must not borrow another role's authority");
    assert!(
        matches!(&err, PgError::ActorAssertionUnauthorised { login_role, .. }
            if login_role == "mtest_gw_real"),
        "the policy must judge the authenticated login, not the assumed role: {err:?}"
    );

    drop_roles(&pool, &["mtest_gw_real", "mtest_gw_borrowed"]).await;
}

#[tokio::test]
async fn two_verifiers_through_one_gateway_cannot_both_be_asserted() {
    // Article 14(5) is the point of the whole rung: a decision needs
    // two distinct verifiers, and one operator with one connection
    // must not be able to be both of them.
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !is_superuser(&pool).await {
        eprintln!("skipping: needs a superuser test role");
        return;
    }
    recreate_roles(&pool, &["mtest_gw_one", "mtest_gw_two"]).await;
    deploy(&pool).await;
    arm(&pool, CHEN).await;
    arm(&pool, OKAFOR).await;
    grant(&pool, CHEN, "mtest_gw_one").await;
    grant(&pool, OKAFOR, "mtest_gw_two").await;

    let one = gateway_pool("mtest_gw_one").await;
    // Chen's own gateway speaks for Chen.
    let (t, args) = oversight_of("verifier_new");
    expect_committed(
        propose_against_pg(&one, &program(), &proposal(&t, args, CHEN))
            .await
            .expect("Chen's own gateway speaks for Chen"),
    );
    // The same gateway cannot then be Okafor - which is exactly the
    // move that made the two-person rule decorative.
    let (t, args) = oversight_of("verifier_new2");
    assert!(
        matches!(
            propose_against_pg(&one, &program(), &proposal(&t, args, OKAFOR)).await,
            Err(PgError::ActorAssertionUnauthorised { .. })
        ),
        "one gateway must not be able to play both verifiers"
    );
    // Okafor's own gateway can.
    let two = gateway_pool("mtest_gw_two").await;
    let (t, args) = oversight_of("verifier_new3");
    expect_committed(
        propose_against_pg(&two, &program(), &proposal(&t, args, OKAFOR))
            .await
            .expect("Okafor's own gateway speaks for Okafor"),
    );

    drop_roles(&pool, &["mtest_gw_one", "mtest_gw_two"]).await;
}

async fn audit_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM morpholog.audit")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn rejection_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM morpholog.rejections")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn outbox_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM morpholog.outbox")
        .fetch_one(pool)
        .await
        .unwrap()
}
