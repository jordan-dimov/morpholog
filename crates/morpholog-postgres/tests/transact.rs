//! Several proposals as one decision. Attacker capability: none - the
//! claims under test are the batch's own: every act or none, later acts
//! see earlier acts (state and actor policy alike), audit order is act
//! order, and intents reach the outbox only on commit.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{EvalValue, Program, Subject, Transition};
use morpholog_postgres::{
    PgAtomicOutcome, PgError, PgPool, PgProposalOutcome, Proposal, propose_against_pg,
    propose_all_against_pg,
};
use morpholog_surface::parse_program;
use morpholog_test_support::{dec, subj};
use uuid::Uuid;

mod common;
use common::{reset_db, test_pool};

const FIXTURE: &str = r#"
program transact_fixture

predicate Account(id: Subject)
predicate Balance(account: Subject, figure: Decimal)
    unique by (account)
predicate ActorAssertionRestricted(actor: Subject)
predicate ActorAssertionAuthority(actor: Subject, login_role: Subject)

intent Posted(account: Subject)

transformation open(id):
    admit Account(id)

transformation post(account, figure):
    require Account(account)
    admit Balance(account, figure)
    emit Posted(account)

transformation arm(person, login_role):
    admit ActorAssertionRestricted(person)
    admit ActorAssertionAuthority(person, login_role)

transformation revoke(person, login_role):
    retract ActorAssertionAuthority(person, login_role)
"#;

fn fixture() -> Program {
    let p = parse_program(FIXTURE).expect("parses");
    p.validate().expect("validates");
    p
}

fn act(transformation: &str, actor: &str, args: Vec<EvalValue>) -> Proposal {
    Proposal::gateway(&Transition {
        transformation_name: transformation.into(),
        args,
        actor: Subject::from(actor),
    })
}

async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn a_refused_act_leaves_nothing_of_the_batch_written() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let compiled = common::compiled(fixture());

    let outcome = propose_all_against_pg(
        &pool,
        &compiled,
        &[
            act("open", "teller", vec![subj("a1")]),
            act("post", "teller", vec![subj("ghost"), dec(5)]),
        ],
    )
    .await
    .unwrap();
    let PgAtomicOutcome::Rejected { act, reason, .. } = outcome else {
        panic!("the second act's gate fails");
    };
    assert_eq!(act, 2);
    assert!(reason.contains("require"), "{reason}");

    assert_eq!(
        count(&pool, "SELECT count(*) FROM morpholog.claims").await,
        0
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM morpholog.audit").await,
        0
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM morpholog.outbox").await,
        0
    );
    let recorded: Vec<String> =
        sqlx::query_scalar("SELECT transformation_name FROM morpholog.rejections")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(recorded, vec!["post"], "the refusing act alone is logged");
}

#[tokio::test]
async fn later_acts_see_earlier_acts_and_the_audit_keeps_act_order() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let compiled = common::compiled(fixture());

    let outcome = propose_all_against_pg(
        &pool,
        &compiled,
        &[
            act("open", "teller", vec![subj("a1")]),
            act("post", "teller", vec![subj("a1"), dec(100)]),
            act("open", "teller", vec![subj("a2")]),
        ],
    )
    .await
    .unwrap();
    let PgAtomicOutcome::Committed { acts } = outcome else {
        panic!("every act admitted");
    };
    assert_eq!(
        acts.iter().map(|a| a.row).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert_eq!(acts[1].emitted_intents.len(), 1, "the intent rides its act");
    let ids: Vec<Uuid> = acts.iter().map(|a| a.transition_id).collect();
    assert!(
        ids.windows(2).all(|w| w[0] < w[1]),
        "ids increase in act order: {ids:?}"
    );

    // Canonical replay order is (committed_at, transition_id); every act
    // shares the transaction's committed_at, so the ids carry the order.
    let replayed: Vec<Uuid> = sqlx::query_scalar(
        "SELECT transition_id FROM morpholog.audit ORDER BY committed_at, transition_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(replayed, ids, "audit replay order is act order");
    let stamps: i64 = count(
        &pool,
        "SELECT count(DISTINCT committed_at) FROM morpholog.audit",
    )
    .await;
    assert_eq!(stamps, 1, "one transaction, one committed_at");

    assert_eq!(
        count(&pool, "SELECT count(*) FROM morpholog.claims").await,
        3
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM morpholog.outbox").await,
        1,
        "the intent reached the outbox with the commit"
    );
}

#[tokio::test]
async fn a_refusal_from_the_staged_prefix_names_the_act_and_carries_its_witness() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let compiled = common::compiled(fixture());

    // Act 3 collides with a balance only act 2 staged, and the witness
    // describes that staged state.
    let outcome = propose_all_against_pg(
        &pool,
        &compiled,
        &[
            act("open", "teller", vec![subj("a1")]),
            act("post", "teller", vec![subj("a1"), dec(1)]),
            act("post", "teller", vec![subj("a1"), dec(2)]),
        ],
    )
    .await
    .unwrap();
    let PgAtomicOutcome::Rejected {
        act, rule, witness, ..
    } = outcome
    else {
        panic!("the third act is refused");
    };
    assert_eq!(act, 3);
    assert_eq!(rule.as_deref(), Some("balance_unique_by_account"));
    assert!(!witness.is_empty(), "the witness names the staged balance");
    assert_eq!(
        count(&pool, "SELECT count(*) FROM morpholog.claims").await,
        0
    );
}

#[tokio::test]
async fn later_authorisation_reads_the_policy_as_earlier_acts_left_it() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let compiled = common::compiled(fixture());
    let login_role: String = sqlx::query_scalar("SELECT session_user")
        .fetch_one(&pool)
        .await
        .unwrap();

    // B is armed and authorised for this connection's role.
    let armed = propose_against_pg(
        &pool,
        &compiled,
        &act("arm", "bootstrap", vec![subj("b"), subj(&login_role)]),
    )
    .await
    .unwrap();
    assert!(matches!(armed, PgProposalOutcome::Committed { .. }));
    assert!(matches!(
        propose_against_pg(&pool, &compiled, &act("open", "b", vec![subj("x0")]))
            .await
            .unwrap(),
        PgProposalOutcome::Committed { .. }
    ));

    // Act 1 revokes B; act 2 acts as B against that staged revocation.
    let err = propose_all_against_pg(
        &pool,
        &compiled,
        &[
            act("revoke", "bootstrap", vec![subj("b"), subj(&login_role)]),
            act("open", "b", vec![subj("x1")]),
        ],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, PgError::ActorAssertionUnauthorised { .. }),
        "{err}"
    );
    // The revocation rolled back with the rest.
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM morpholog.claims WHERE predicate_name = 'ActorAssertionAuthority'"
        )
        .await,
        1
    );
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM morpholog.claims WHERE predicate_name = 'Account'"
        )
        .await,
        1,
        "x1 was never admitted"
    );
}

#[tokio::test]
async fn an_empty_or_unknown_batch_is_refused_before_a_transaction_opens() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let compiled = common::compiled(fixture());
    let err = propose_all_against_pg(&pool, &compiled, &[])
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::InvalidState(_)), "{err}");
    let err = propose_all_against_pg(
        &pool,
        &compiled,
        &[
            act("open", "teller", vec![subj("a1")]),
            act("no_such_act", "teller", vec![]),
        ],
    )
    .await
    .unwrap_err();
    assert!(
        matches!(err, PgError::UnknownTransformation { .. }),
        "{err}"
    );
    assert_eq!(
        count(&pool, "SELECT count(*) FROM morpholog.claims").await,
        0
    );
}
