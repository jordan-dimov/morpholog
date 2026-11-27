//! Kinds of actor on a closed deployment: a login granted a kind may
//! assert any actor of that kind, and a registrar enrols only the actors
//! its own act mints.
//!
//! **The attacker this models.** An application with its own login that
//! proposes through the adapter and tries to reach an identity it was not
//! given: by asserting an actor of another kind, by classifying an existing
//! actor (a service) into the kind its login may assert, by enrolling into
//! a kind it is not registrar for, by granting itself a kind, or by doing
//! any of these in the second act of an atomic batch. The programme here
//! gates none of its policy acts, so every refusal is the runtime's.
//!
//! **Not modelled.** A caller running its own SQL with the writer's
//! credentials (see `actor_policy.rs`).
//!
//! Each simulated gateway takes its login with `SET SESSION AUTHORIZATION`,
//! which needs superuser, hence the skip.
#![expect(
    clippy::unwrap_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

mod common;

use common::{
    expect_committed, gateway_pool, pg_program, propose_pg_as, reset_db, session_is_superuser,
    subj, test_pool,
};
use morpholog_core::{EvalValue, Subject, Transition};
use morpholog_postgres::{
    PgError, PgPool, PgProgram, PgProposalOutcome, Proposal, close_deployment,
    propose_all_against_pg,
};

const ROOT: &str = "root_admin";
const HOST: &str = "inventory_host";
const SERVICE: &str = "risk_engine";

/// Policy acts with no gates of their own: the runtime decides who may.
const PROGRAMME: &str = "\
program actor_kinds

predicate ActorAssertionRestricted(actor: Subject)
predicate ActorAssertionAuthority(actor: Subject, login_role: Subject)
predicate ActorKind(actor: Subject, kind: Subject)
predicate ActorKindAuthority(kind: Subject, login_role: Subject)
predicate ActorKindEnrolment(kind: Subject, actor: Subject)
predicate Account(user: Subject, email: Subject)
predicate Note(text: Subject, author: Subject)

transformation arm(person):
    admit ActorAssertionRestricted(person)

transformation grant(person, login_role):
    admit ActorAssertionAuthority(person, login_role)

transformation grant_kind(kind, login_role):
    admit ActorKindAuthority(kind, login_role)

transformation appoint_registrar(kind, registrar):
    admit ActorKindEnrolment(kind, registrar)

transformation classify(person, kind):
    admit ActorKind(person, kind)

transformation classify_twice(person, kind):
    admit ActorKind(person, kind)
    admit ActorKind(person, kind)

transformation reclassify(person, from, to):
    retract ActorKind(person, from)
    admit ActorKind(person, to)

transformation sign_up(email, kind):
    let user = new Subject()
    admit Account(user, email)
    admit ActorKind(user, kind)

transformation sign_up_twice(email, first, second):
    let user = new Subject()
    admit Account(user, email)
    admit ActorKind(user, first)
    admit ActorKind(user, second)

transformation note(text):
    admit Note(text, actor)
";

fn program() -> PgProgram {
    pg_program(morpholog_surface::parse_program(PROGRAMME).unwrap())
}

fn transition(name: &str, args: &[&str], actor: &str) -> Transition {
    Transition {
        transformation_name: name.into(),
        args: args.iter().map(|a| subj(a)).collect(),
        actor: Subject::from(actor),
        decided_at: None,
    }
}

async fn act(
    pool: &PgPool,
    name: &str,
    args: &[&str],
    actor: &str,
) -> Result<PgProposalOutcome, PgError> {
    let p = program();
    let t = p.prepared().transformation(&name.into()).unwrap().clone();
    let args: Vec<EvalValue> = args.iter().map(|a| subj(a)).collect();
    propose_pg_as(pool, &p, &t, args, actor).await
}

async fn committed(pool: &PgPool, name: &str, args: &[&str], actor: &str) {
    expect_committed(act(pool, name, args, actor).await.unwrap());
}

/// The subject `sign_up` minted for `email`.
async fn account(pool: &PgPool, email: &str) -> String {
    sqlx::query_scalar(
        "SELECT arguments -> 0 ->> 'value' FROM morpholog.claims
         WHERE predicate_name = 'Account' AND arguments -> 1 ->> 'value' = $1",
    )
    .bind(email)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// The three logins of one test: the root's, the host's and the service's.
struct Logins {
    root: String,
    host: String,
    service: String,
}

impl Logins {
    fn new(tag: &str) -> Self {
        let name = |who: &str| format!("mtest_kind_{tag}_{who}");
        Self {
            root: name("root"),
            host: name("host"),
            service: name("svc"),
        }
    }
}

/// Close under the root; the root grants kind `user` to the host's login,
/// makes the host the registrar for `user`, and gives the host and the
/// service exact grants for themselves.
async fn deploy(admin: &PgPool, root: &PgPool, logins: &Logins) {
    close_deployment(admin, &Subject::from(ROOT), &logins.root)
        .await
        .unwrap();
    committed(root, "grant_kind", &["user", &logins.host], ROOT).await;
    committed(root, "appoint_registrar", &["user", HOST], ROOT).await;
    committed(root, "grant", &[HOST, &logins.host], ROOT).await;
    committed(root, "grant", &[SERVICE, &logins.service], ROOT).await;
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

fn unauthorised(outcome: &Result<PgProposalOutcome, PgError>) -> bool {
    matches!(outcome, Err(PgError::ActorAssertionUnauthorised { .. }))
}

fn policy_refused(outcome: &Result<PgProposalOutcome, PgError>) -> bool {
    matches!(outcome, Err(PgError::PolicyWriteUnauthorised { .. }))
}

/// Run `scenario` with the three logins of `tag`, made and dropped around it.
async fn with_logins<F, Fut>(tag: &str, scenario: F)
where
    F: FnOnce(PgPool, PgPool, PgPool, PgPool, Logins) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let Some(admin) = superuser_pool().await else {
        return;
    };
    let logins = Logins::new(tag);
    let names = [
        logins.root.clone(),
        logins.host.clone(),
        logins.service.clone(),
    ];
    let borrowed: Vec<&str> = names.iter().map(String::as_str).collect();
    common::recreate_gateway_roles(&admin, &borrowed).await;
    let (root, host, service) = (
        gateway_pool(&logins.root).await,
        gateway_pool(&logins.host).await,
        gateway_pool(&logins.service).await,
    );
    let outcome = tokio::spawn(scenario(admin.clone(), root, host, service, logins)).await;
    common::drop_roles_if_present(&admin, &borrowed).await;
    reset_db(&admin).await;
    if let Err(failure) = outcome {
        std::panic::resume_unwind(failure.into_panic());
    }
}

#[tokio::test]
async fn a_host_enrols_a_new_user_and_asserts_it_but_never_a_service() {
    with_logins("enrol", |admin, root, host, service, logins| async move {
        deploy(&admin, &root, &logins).await;
        committed(&host, "sign_up", &["ana@example.com", "user"], HOST).await;
        let ana = account(&admin, "ana@example.com").await;

        committed(&host, "note", &["ordered"], &ana).await;
        assert!(unauthorised(
            &act(&host, "note", &["posing"], SERVICE).await
        ));
        assert!(unauthorised(
            &act(&service, "note", &["posing"], &ana).await
        ));
        committed(&service, "note", &["fills reconciled"], SERVICE).await;
    })
    .await;
}

#[tokio::test]
async fn no_one_but_the_root_classifies_an_existing_actor() {
    with_logins("forge", |admin, root, host, service, logins| async move {
        deploy(&admin, &root, &logins).await;
        committed(&host, "sign_up", &["ana@example.com", "user"], HOST).await;
        let ana = account(&admin, "ana@example.com").await;

        // The registrar, classifying the service into the kind its login
        // may assert: the impersonation path the one-kind rule misses.
        assert!(policy_refused(
            &act(&host, "classify", &[SERVICE, "user"], HOST).await
        ));
        // The service, classifying itself.
        assert!(policy_refused(
            &act(&service, "classify", &[SERVICE, "user"], SERVICE).await
        ));
        // A user, enrolling as a service.
        assert!(policy_refused(
            &act(&host, "classify", &[&ana, "service"], &ana).await
        ));
        // The registrar, enrolling into a kind it is not registrar for.
        assert!(policy_refused(
            &act(&host, "sign_up", &["eve@example.com", "service"], HOST).await
        ));
        // The registrar, granting itself another kind.
        assert!(policy_refused(
            &act(&host, "grant_kind", &["service", &logins.host], HOST).await
        ));
        // The same forgery as the first act of a batch whose second act
        // would use it.
        let p = program();
        let batch = [
            Proposal::gateway(&transition("classify", &[SERVICE, "user"], HOST)),
            Proposal::gateway(&transition("note", &["posing"], SERVICE)),
        ];
        assert!(matches!(
            propose_all_against_pg(&host, &p, &batch).await,
            Err(PgError::PolicyWriteUnauthorised { .. })
        ));
        assert!(unauthorised(
            &act(&host, "note", &["posing"], SERVICE).await
        ));
    })
    .await;
}

#[tokio::test]
async fn no_act_leaves_an_actor_two_kinds_and_no_kind_reaches_the_root() {
    with_logins("two", |admin, root, host, service, logins| async move {
        deploy(&admin, &root, &logins).await;
        committed(&root, "classify", &[SERVICE, "service"], ROOT).await;
        // Not even the root's: the logins of both kinds could assert it.
        assert!(matches!(
            act(&root, "classify", &[SERVICE, "user"], ROOT).await,
            Err(PgError::ActorKindAmbiguous { kinds: 2, .. })
        ));
        // Changing the kind in one act leaves one.
        committed(&root, "reclassify", &[SERVICE, "service", "user"], ROOT).await;
        committed(&host, "note", &["now a user"], SERVICE).await;
        // A registrar's new actor, enrolled into both its kinds in one act.
        committed(&root, "appoint_registrar", &["service", HOST], ROOT).await;
        assert!(matches!(
            act(
                &host,
                "sign_up_twice",
                &["twice@example.com", "user", "service"],
                HOST
            )
            .await,
            Err(PgError::ActorKindAmbiguous { kinds: 2, .. })
        ));
        let _ = service;

        committed(&root, "classify", &[ROOT, "user"], ROOT).await;
        assert!(matches!(
            act(&host, "note", &["posing"], ROOT).await,
            Err(PgError::RootActorUnauthorised { .. })
        ));
    })
    .await;
}

/// The root moves a user to another kind between two of the user's acts
/// in one batch, on a login granted the first kind only. The later act is
/// judged against the moved kind and refused, and nothing commits.
#[tokio::test]
async fn a_kind_the_root_changes_mid_batch_reaches_the_actor_s_later_act() {
    with_logins("mid", |admin, root, host, _service, logins| async move {
        deploy(&admin, &root, &logins).await;
        committed(&root, "grant_kind", &["user", &logins.root], ROOT).await;
        committed(&host, "sign_up", &["ana@example.com", "user"], HOST).await;
        let ana = account(&admin, "ana@example.com").await;
        let p = program();
        let batch = [
            Proposal::gateway(&transition("note", &["first"], &ana)),
            Proposal::gateway(&transition("reclassify", &[&ana, "user", "staff"], ROOT)),
            Proposal::gateway(&transition("note", &["second"], &ana)),
        ];
        assert!(matches!(
            propose_all_against_pg(&root, &p, &batch).await,
            Err(PgError::ActorAssertionUnauthorised { actor, .. }) if actor.as_str() == ana
        ));
        committed(&root, "note", &["still a user"], &ana).await;
    })
    .await;
}

/// The acceptance side: open, policy writes are the programme's to gate
/// and kinds grant nothing, as before. Closing refuses an actor that
/// holds two kinds.
#[tokio::test]
async fn an_open_deployment_leaves_policy_writes_to_the_programme() {
    with_logins("open", |admin, _root, host, _service, logins| async move {
        committed(&host, "classify", &[SERVICE, "user"], HOST).await;
        committed(&host, "classify", &[SERVICE, "service"], HOST).await;
        committed(&host, "grant_kind", &["user", &logins.host], HOST).await;
        committed(&host, "note", &["anyone"], SERVICE).await;
        // Armed, the service needs an exact grant: open, a kind grants
        // nothing.
        committed(&host, "arm", &[SERVICE], HOST).await;
        assert!(unauthorised(
            &act(&host, "note", &["posing"], SERVICE).await
        ));
        assert!(matches!(
            close_deployment(&admin, &Subject::from(ROOT), &logins.root).await,
            Err(PgError::ActorKindAmbiguous { kinds: 2, .. })
        ));
        committed(&host, "reclassify", &[SERVICE, "service", "user"], HOST).await;
        close_deployment(&admin, &Subject::from(ROOT), &logins.root)
            .await
            .unwrap();
    })
    .await;
}

/// One kind per actor counts kinds, not admissions: admitting a kind the
/// actor already holds is a no-op, as for any claim.
#[tokio::test]
async fn admitting_a_kind_the_actor_holds_again_changes_nothing() {
    with_logins("again", |admin, root, host, _service, logins| async move {
        deploy(&admin, &root, &logins).await;
        committed(&root, "classify", &[SERVICE, "user"], ROOT).await;
        committed(&root, "classify", &[SERVICE, "user"], ROOT).await;
        committed(&root, "classify_twice", &[SERVICE, "user"], ROOT).await;
        committed(&host, "note", &["still one kind"], SERVICE).await;
        assert!(matches!(
            act(&root, "classify", &[SERVICE, "service"], ROOT).await,
            Err(PgError::ActorKindAmbiguous { kinds: 2, .. })
        ));
    })
    .await;
}

/// A deployment that was open when these names were not yet reserved can
/// hold a kind grant of another shape. Once closed, it grants nothing; a
/// grant of the reserved shape does.
#[tokio::test]
async fn a_misshapen_kind_grant_from_an_open_past_grants_nothing() {
    with_logins("legacy", |admin, root, host, _service, logins| async move {
        committed(&host, "classify", &["legacy_user", "user"], HOST).await;
        let extra = serde_json::json!([
            {"type": "subject", "value": "user"},
            {"type": "subject", "value": logins.host},
            {"type": "subject", "value": "extra"},
        ]);
        sqlx::query(
            "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
             VALUES ('ActorKindAuthority', $1, gen_random_uuid())",
        )
        .bind(extra)
        .execute(&admin)
        .await
        .unwrap();
        close_deployment(&admin, &Subject::from(ROOT), &logins.root)
            .await
            .unwrap();

        assert!(unauthorised(
            &act(&host, "note", &["posing"], "legacy_user").await
        ));
        committed(&root, "grant_kind", &["user", &logins.host], ROOT).await;
        committed(&host, "note", &["granted now"], "legacy_user").await;
    })
    .await;
}
