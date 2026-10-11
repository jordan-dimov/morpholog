//! Actor-assertion policy: which login role may propose as which actor.
//!
//! **The attacker this models.** An application that legitimately holds
//! a Morpholog connection and proposes through the adapter, naming an
//! actor it has no authority for - for example one gateway signing as
//! two different people to satisfy a two-person rule alone.
//!
//! **Not modelled.** A compromised gateway running its own SQL. The
//! writer role can insert claims and audit rows directly and never meets
//! this check. The guarantee covers callers that go through the adapter,
//! and is only as good as the separation of gateways' credentials.
//!
//! The check reads `session_user`, so each simulated gateway is a pool
//! that takes its own identity via `SET SESSION AUTHORIZATION`. That
//! needs superuser, hence the skip. A superuser can change
//! `session_user`; `SET ROLE` cannot.
//!
//! The programme here is the smallest that uses the policy, so these
//! tests hold the adapter to it whatever the gallery's rules become.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{
    expect_committed, gateway_pool, pg_program, propose_pg_as, reset_db, session_is_superuser,
    test_pool, with_gateway_roles,
};
use morpholog_core::{EvalValue, Subject, Transformation, Transition};
use morpholog_postgres::PgProgram;
use morpholog_postgres::{
    PgError, PgPool, PgProposalOutcome, Proposal, propose_against_pg, propose_against_pg_with_trace,
};
use sqlx::postgres::PgPoolOptions;

const DEPLOYER: &str = "admin_1";

/// Roles are cluster-wide and these tests run in parallel, so each test
/// uses its own names.
fn deployer_login(tag: &str) -> String {
    format!("mtest_gw_{tag}_dep")
}
const CHEN: &str = "verifier_chen";
const OKAFOR: &str = "verifier_okafor";

/// An administrator, appointed once and restricted to its login, who
/// restricts names and grants logins; and an act anyone may propose.
const POLICY: &str = "\
program actor_policy

predicate ActorAssertionRestricted(actor: Subject)
predicate ActorAssertionAuthority(actor: Subject, login_role: Subject)
predicate Administrator(admin: Subject)
predicate Note(text: Subject, author: Subject)

transformation appoint(admin, login_role):
    require not Administrator(_)
    require actor = admin
    admit Administrator(admin)
    admit ActorAssertionRestricted(admin)
    admit ActorAssertionAuthority(admin, login_role)

transformation restrict(person):
    require Administrator(actor)
    admit ActorAssertionRestricted(person)

transformation grant(person, login_role):
    require Administrator(actor)
    require ActorAssertionRestricted(person)
    admit ActorAssertionAuthority(person, login_role)

transformation withdraw(person, login_role):
    require Administrator(actor)
    require ActorAssertionAuthority(person, login_role)
    retract ActorAssertionAuthority(person, login_role)

transformation note(text):
    admit Note(text, actor)
";

fn program() -> PgProgram {
    pg_program(morpholog_surface::parse_program(POLICY).unwrap())
}

fn subj(s: &str) -> EvalValue {
    EvalValue::Subject(Subject::from(s))
}

/// The administrator on the record, restricted to this pool's login, so
/// every later enrolment act must come through the same pool.
async fn deploy(pool: &PgPool, deployer_login: &str) {
    let p = program();
    let t = p
        .prepared()
        .transformation(&"appoint".into())
        .unwrap()
        .clone();
    expect_committed(
        propose_pg_as(
            pool,
            &p,
            &t,
            vec![subj(DEPLOYER), subj(deployer_login)],
            DEPLOYER,
        )
        .await
        .unwrap(),
    );
}

async fn arm(pool: &PgPool, person: &str) {
    let p = program();
    let t = p
        .prepared()
        .transformation(&"restrict".into())
        .unwrap()
        .clone();
    expect_committed(
        propose_pg_as(pool, &p, &t, vec![subj(person)], DEPLOYER)
            .await
            .unwrap(),
    );
}

async fn grant(pool: &PgPool, person: &str, role: &str) {
    let p = program();
    let t = p
        .prepared()
        .transformation(&"grant".into())
        .unwrap()
        .clone();
    expect_committed(
        propose_pg_as(pool, &p, &t, vec![subj(person), subj(role)], DEPLOYER)
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

/// An act with no gate on its actor, to propose as an actor.
fn note_of(text: &str) -> (Transformation, Vec<EvalValue>) {
    let p = program();
    let t = p.prepared().transformation(&"note".into()).unwrap().clone();
    (t, vec![subj(text)])
}

#[tokio::test]
async fn an_unarmed_actor_is_asserted_by_anyone_exactly_as_before() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !session_is_superuser(&pool).await {
        eprintln!("skipping: needs a superuser test role");
        return;
    }
    let dep = deployer_login("a");
    with_gateway_roles(&pool, &[&dep, "mtest_gw_a"], {
        let dep = dep.clone();
        async move {
            let deployer = gateway_pool(dep.clone()).await;
            deploy(&deployer, &dep).await;

            // No ActorAssertionRestricted claim exists, so nothing is restricted.
            let gateway = gateway_pool("mtest_gw_a").await;
            let (t, args) = note_of("verifier_new");
            let outcome = propose_against_pg(&gateway, &program(), &proposal(&t, args, CHEN))
                .await
                .expect("an unarmed actor stays assertable by any role");
            assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));
        }
    })
    .await;
}

#[tokio::test]
async fn an_armed_actor_is_refused_to_an_unauthorised_role_and_records_nothing() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !session_is_superuser(&pool).await {
        eprintln!("skipping: needs a superuser test role");
        return;
    }
    let dep = deployer_login("chen");
    with_gateway_roles(&pool, &[&dep, "mtest_gw_chen", "mtest_gw_rogue"], {
        let pool = pool.clone();
        let dep = dep.clone();
        async move {
            let deployer = gateway_pool(dep.clone()).await;
            deploy(&deployer, &dep).await;
            arm(&deployer, CHEN).await;
            grant(&deployer, CHEN, "mtest_gw_chen").await;

            let audit_before = audit_count(&pool).await;
            let rejections_before = rejection_count(&pool).await;
            let outbox_before = outbox_count(&pool).await;

            let rogue = gateway_pool("mtest_gw_rogue").await;
            let (t, args) = note_of("verifier_new");
            let err = propose_against_pg(&rogue, &program(), &proposal(&t, args, CHEN))
                .await
                .expect_err("an unauthorised role may not speak for an armed actor");
            assert!(
                matches!(&err, PgError::ActorAssertionUnauthorised { actor, login_role }
                    if actor.as_str() == CHEN && login_role == "mtest_gw_rogue"),
                "{err:?}"
            );

            // A refused assertion must not record that the actor attempted anything.
            assert_eq!(audit_count(&pool).await, audit_before, "audit grew");
            assert_eq!(
                rejection_count(&pool).await,
                rejections_before,
                "the rejection log is for business refusals only"
            );
            assert_eq!(outbox_count(&pool).await, outbox_before, "outbox grew");
        }
    })
    .await;
}

#[tokio::test]
async fn the_authorised_role_proceeds() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !session_is_superuser(&pool).await {
        eprintln!("skipping: needs a superuser test role");
        return;
    }
    let dep = deployer_login("chen2");
    with_gateway_roles(&pool, &[&dep, "mtest_gw_chen2"], {
        let dep = dep.clone();
        async move {
            let deployer = gateway_pool(dep.clone()).await;
            deploy(&deployer, &dep).await;
            arm(&deployer, CHEN).await;
            grant(&deployer, CHEN, "mtest_gw_chen2").await;

            let gateway = gateway_pool("mtest_gw_chen2").await;
            let (t, args) = note_of("verifier_new");
            let outcome = propose_against_pg(&gateway, &program(), &proposal(&t, args, CHEN))
                .await
                .expect("the granted role speaks for its actor");
            assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));
        }
    })
    .await;
}

#[tokio::test]
async fn asking_for_a_trace_does_not_get_round_the_policy() {
    // The traced path has its own transaction. A check only on the
    // ordinary path could be bypassed by adding --trace.
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !session_is_superuser(&pool).await {
        eprintln!("skipping: needs a superuser test role");
        return;
    }
    let dep = deployer_login("trace");
    with_gateway_roles(&pool, &[&dep, "mtest_gw_trace"], {
        let dep = dep.clone();
        async move {
            let deployer = gateway_pool(dep.clone()).await;
            deploy(&deployer, &dep).await;
            arm(&deployer, CHEN).await;

            let rogue = gateway_pool("mtest_gw_trace").await;
            let (t, args) = note_of("verifier_new");
            let err = propose_against_pg_with_trace(&rogue, &program(), &proposal(&t, args, CHEN))
                .await
                .expect_err("the traced path must take the same gate");
            assert!(
                matches!(&err, PgError::ActorAssertionUnauthorised { .. }),
                "{err:?}"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn withdrawing_the_last_grant_locks_the_actor_out_rather_than_freeing_it() {
    // Why the restricting claim is separate from the grants: if the
    // grants did the restricting, revoking the last grant would open the
    // name to every gateway.
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !session_is_superuser(&pool).await {
        eprintln!("skipping: needs a superuser test role");
        return;
    }
    let dep = deployer_login("last");
    with_gateway_roles(&pool, &[&dep, "mtest_gw_last", "mtest_gw_other"], {
        let dep = dep.clone();
        async move {
            let deployer = gateway_pool(dep.clone()).await;
            deploy(&deployer, &dep).await;
            arm(&deployer, CHEN).await;
            grant(&deployer, CHEN, "mtest_gw_last").await;

            let p = program();
            let withdraw = p
                .prepared()
                .transformation(&"withdraw".into())
                .unwrap()
                .clone();
            expect_committed(
                propose_pg_as(
                    &deployer,
                    &p,
                    &withdraw,
                    vec![subj(CHEN), subj("mtest_gw_last")],
                    DEPLOYER,
                )
                .await
                .unwrap(),
            );

            // The formerly-granted gateway is now refused...
            let former = gateway_pool("mtest_gw_last").await;
            let (t, args) = note_of("verifier_new");
            assert!(
                matches!(
                    propose_against_pg(&former, &program(), &proposal(&t, args, CHEN)).await,
                    Err(PgError::ActorAssertionUnauthorised { .. })
                ),
                "the withdrawn grant must stop working"
            );
            // ...and so is everyone else. No downgrade to open.
            let other = gateway_pool("mtest_gw_other").await;
            let (t, args) = note_of("verifier_new2");
            assert!(
                matches!(
                    propose_against_pg(&other, &program(), &proposal(&t, args, CHEN)).await,
                    Err(PgError::ActorAssertionUnauthorised { .. })
                ),
                "withdrawing the last grant must LOCK the actor, never free it"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn set_role_does_not_change_who_the_policy_thinks_you_are() {
    // session_user is what the policy reads precisely because SET ROLE
    // cannot touch it.
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !session_is_superuser(&pool).await {
        eprintln!("skipping: needs a superuser test role");
        return;
    }
    let dep = deployer_login("setrole");
    with_gateway_roles(&pool, &[&dep, "mtest_gw_real", "mtest_gw_borrowed"], {
        let pool = pool.clone();
        let dep = dep.clone();
        async move {
            let deployer = gateway_pool(dep.clone()).await;
            deploy(&deployer, &dep).await;
            arm(&deployer, CHEN).await;
            grant(&deployer, CHEN, "mtest_gw_borrowed").await;
            // Membership, so the SET ROLE below succeeds: borrowing a role is
            // allowed, it just does not change who the policy judges.
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

            let (t, args) = note_of("verifier_new");
            let err = propose_against_pg(&borrowing, &program(), &proposal(&t, args, CHEN))
                .await
                .expect_err("SET ROLE must not borrow another role's authority");
            assert!(
                matches!(&err, PgError::ActorAssertionUnauthorised { login_role, .. }
                    if login_role == "mtest_gw_real"),
                "the policy must judge the authenticated login, not the assumed role: {err:?}"
            );
        }
    })
    .await;
}

#[tokio::test]
async fn two_verifiers_through_one_gateway_cannot_both_be_asserted() {
    // Article 14(5): a decision needs two distinct verifiers, and one
    // operator with one connection must not be both.
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !session_is_superuser(&pool).await {
        eprintln!("skipping: needs a superuser test role");
        return;
    }
    let dep = deployer_login("one");
    with_gateway_roles(&pool, &[&dep, "mtest_gw_one", "mtest_gw_two"], {
        let dep = dep.clone();
        async move {
            let deployer = gateway_pool(dep.clone()).await;
            deploy(&deployer, &dep).await;
            arm(&deployer, CHEN).await;
            arm(&deployer, OKAFOR).await;
            grant(&deployer, CHEN, "mtest_gw_one").await;
            grant(&deployer, OKAFOR, "mtest_gw_two").await;

            let one = gateway_pool("mtest_gw_one").await;
            // Chen's own gateway speaks for Chen.
            let (t, args) = note_of("verifier_new");
            expect_committed(
                propose_against_pg(&one, &program(), &proposal(&t, args, CHEN))
                    .await
                    .expect("Chen's own gateway speaks for Chen"),
            );
            // The same gateway cannot then be Okafor.
            let (t, args) = note_of("verifier_new2");
            assert!(
                matches!(
                    propose_against_pg(&one, &program(), &proposal(&t, args, OKAFOR)).await,
                    Err(PgError::ActorAssertionUnauthorised { .. })
                ),
                "one gateway must not be able to play both verifiers"
            );
            // Okafor's own gateway can.
            let two = gateway_pool("mtest_gw_two").await;
            let (t, args) = note_of("verifier_new3");
            expect_committed(
                propose_against_pg(&two, &program(), &proposal(&t, args, OKAFOR))
                    .await
                    .expect("Okafor's own gateway speaks for Okafor"),
            );
        }
    })
    .await;
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

#[tokio::test]
async fn a_rogue_gateway_cannot_escalate_through_an_unrestricted_deployer() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    if !session_is_superuser(&pool).await {
        eprintln!("skipping");
        return;
    }
    let dep = deployer_login("esc");
    with_gateway_roles(&pool, &[&dep, "mtest_gw_esc"], {
        let dep = dep.clone();
        async move {
            let deployer = gateway_pool(dep.clone()).await;
            deploy(&deployer, &dep).await;
            arm(&deployer, CHEN).await;
            arm(&deployer, OKAFOR).await;

            // The rogue never touches SQL. It simply proposes as the DEPLOYER,
            // whom nobody armed, and grants itself both verifiers.
            let rogue = gateway_pool("mtest_gw_esc").await;
            let p = program();
            let t = p
                .prepared()
                .transformation(&"grant".into())
                .unwrap()
                .clone();
            for person in [CHEN, OKAFOR] {
                let pr = proposal(&t, vec![subj(person), subj("mtest_gw_esc")], DEPLOYER);
                let r = propose_against_pg(&rogue, &program(), &pr).await;
                assert!(
                    matches!(r, Err(PgError::ActorAssertionUnauthorised { .. })),
                    "granting authority through an unrestricted deployer must be refused, got {r:?}"
                );
            }
        }
    })
    .await;
}
