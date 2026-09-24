//! Which incarnation of a login role asserted each actor.
//!
//! Attacker capability modelled: `CREATEROLE` - dropping a login role and
//! creating a new one under the same name, so the new role inherits every
//! grant made to the name. Gateway attestation records the role's OID as
//! well as its name; the verifiers report a name seen under a new OID, over
//! rows an intact verdict established, and never change the verdict for it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{
    commit_entry, drop_roles_if_present, make_checkpoint, recreate_roles, reset_db,
    session_is_superuser, test_pool,
};
use morpholog_postgres::{
    AuditAttestation, PackVerdict, PgPool, RebindingScope, RoleRebindings, TreeVerification,
    export_pack, export_selective, list_audit_rows, pack_role_rebindings,
    verify_audit_tree_with_chain, verify_pack, verify_selective,
};
use sqlx::postgres::PgPoolOptions;

/// A login that can reach the governed tables, like a real gateway.
async fn create_gateway(pool: &PgPool, role: &str) {
    recreate_roles(
        pool,
        &[role],
        &[
            &format!("CREATE ROLE {role} LOGIN"),
            &format!("GRANT USAGE ON SCHEMA morpholog TO {role}"),
            &format!("GRANT ALL ON ALL TABLES IN SCHEMA morpholog TO {role}"),
        ],
    )
    .await;
}

/// A pool whose sessions present themselves as `role`.
async fn pool_as(role: &str) -> PgPool {
    let role = role.to_string();
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let url = morpholog_postgres::with_default_user(&url);
    PgPoolOptions::new()
        .max_connections(1)
        .after_connect(move |conn, _| {
            let role = role.clone();
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

async fn role_oid(pool: &PgPool, role: &str) -> u32 {
    let oid: sqlx::postgres::types::Oid =
        sqlx::query_scalar("SELECT oid FROM pg_roles WHERE rolname = $1")
            .bind(role)
            .fetch_one(pool)
            .await
            .unwrap();
    oid.0
}

fn oids(rows: &[morpholog_postgres::AuditRow]) -> Vec<Option<u32>> {
    rows.iter()
        .map(|r| match &r.attestation {
            Some(AuditAttestation::Gateway {
                authenticated_by_oid,
                ..
            }) => *authenticated_by_oid,
            None => None,
        })
        .collect()
}

/// A role dropped and created again between two commits is reported by
/// the live verifier and by complete-prefix and selective packs, while
/// every verdict stays intact.
#[tokio::test]
async fn a_recreated_role_is_reported_and_the_tree_stays_intact() {
    let pool = test_pool().await;
    if !session_is_superuser(&pool).await {
        return;
    }
    reset_db(&pool).await;
    const ROLE: &str = "mtest_rebind_gw";
    create_gateway(&pool, ROLE).await;
    let before = role_oid(&pool, ROLE).await;
    let first = commit_entry(&pool_as(ROLE).await, "rebind_1").await;

    create_gateway(&pool, ROLE).await;
    let after = role_oid(&pool, ROLE).await;
    assert_ne!(before, after, "a recreated role gets a new OID");
    let second = commit_entry(&pool_as(ROLE).await, "rebind_2").await;
    make_checkpoint(&pool).await;

    let (tree, _, rebindings) = verify_audit_tree_with_chain(&pool, None, None)
        .await
        .unwrap();
    assert!(matches!(tree, TreeVerification::Intact { .. }), "{tree:?}");
    let RoleRebindings::Evaluated {
        scope,
        rows_with_oid,
        changes,
        ..
    } = rebindings
    else {
        panic!("an intact tree is evaluated: {rebindings:?}");
    };
    assert_eq!(scope, RebindingScope::CompletePrefix);
    assert_eq!(rows_with_oid, 2);
    assert_eq!(changes.len(), 1, "{changes:?}");
    let change = &changes[0];
    assert_eq!(change.role, ROLE);
    assert_eq!((change.previous_oid, change.new_oid), (before, after));
    assert_eq!(change.last_observed_transition, first);
    assert_eq!(change.first_observed_transition, second);

    let pack = export_pack(&pool, None).await.unwrap();
    let verdict = PackVerdict::Prefix(verify_pack(&pack, None).unwrap());
    assert!(verdict.is_intact(), "{verdict:?}");
    let from_pack = pack_role_rebindings(&pack.rows, &verdict);
    assert!(
        matches!(&from_pack, RoleRebindings::Evaluated { changes, .. } if changes.len() == 1),
        "{from_pack:?}"
    );
    // The verifier accepts rows in any file order, so the finding reads
    // them in log order too.
    let mut shuffled = pack.clone();
    shuffled.rows.reverse();
    let verdict = PackVerdict::Prefix(verify_pack(&shuffled, None).unwrap());
    assert!(verdict.is_intact(), "{verdict:?}");
    assert_eq!(
        pack_role_rebindings(&shuffled.rows, &verdict),
        from_pack,
        "a shuffled pack reports the same change, in the same direction"
    );

    let selective = export_selective(&pool, None, &[first, second])
        .await
        .unwrap();
    let verdict = PackVerdict::Selective(verify_selective(&selective, None).unwrap());
    assert!(verdict.is_intact(), "{verdict:?}");
    let from_selective = pack_role_rebindings(&selective.rows, &verdict);
    assert!(
        matches!(&from_selective, RoleRebindings::Evaluated {
            scope: RebindingScope::Selective, changes, ..
        } if changes.len() == 1),
        "{from_selective:?}"
    );

    drop_roles_if_present(&pool, &[ROLE]).await;
}

/// Editing a row's recorded OID breaks the tree, and no finding is drawn
/// from rows the verdict did not establish.
#[tokio::test]
async fn a_tampered_oid_breaks_the_tree_and_reports_nothing() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    commit_entry(&pool, "tamper_1").await;
    make_checkpoint(&pool).await;
    sqlx::query(
        "UPDATE morpholog.audit
            SET attestation = jsonb_set(attestation, '{authenticated_by_oid}', '1')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let (tree, _, rebindings) = verify_audit_tree_with_chain(&pool, None, None)
        .await
        .unwrap();
    assert!(
        matches!(tree, TreeVerification::Tampered { .. }),
        "{tree:?}"
    );
    assert_eq!(rebindings, RoleRebindings::NotEvaluated);

    let pack = export_pack(&pool, None).await.unwrap();
    let verdict = PackVerdict::Prefix(verify_pack(&pack, None).unwrap());
    assert!(!verdict.is_intact(), "{verdict:?}");
    assert_eq!(
        pack_role_rebindings(&pack.rows, &verdict),
        RoleRebindings::NotEvaluated,
        "a pack that failed verification supports no finding"
    );
}

/// A session whose role was dropped and created again under it cannot
/// propose: it never records the new role's OID as its own.
#[tokio::test]
async fn a_session_whose_role_was_recreated_fails_closed() {
    let pool = test_pool().await;
    if !session_is_superuser(&pool).await {
        return;
    }
    reset_db(&pool).await;
    const ROLE: &str = "mtest_rebind_live";
    create_gateway(&pool, ROLE).await;
    let old_session = pool_as(ROLE).await;
    commit_entry(&old_session, "live_1").await;

    create_gateway(&pool, ROLE).await;
    let compiled = common::compiled(morpholog_examples::double_entry_ledger::program());
    let proposal = common::attested(&morpholog_core::Transition {
        transformation_name: "post_simple_entry".into(),
        args: common::ledger_args("live_2"),
        actor: morpholog_core::Subject::from("alex"),
    });
    let outcome = morpholog_postgres::propose_against_pg(&old_session, &compiled, &proposal).await;
    assert!(
        outcome.is_err(),
        "the old session must fail closed: {outcome:?}"
    );

    let rows = list_audit_rows(&pool).await.unwrap();
    assert_eq!(rows.len(), 1, "nothing was recorded after the role changed");
    drop(old_session);
    drop_roles_if_present(&pool, &[ROLE]).await;
}

/// Every act of one atomic decision is asserted by the same role
/// incarnation, whichever actor each names.
#[tokio::test]
async fn every_act_of_a_transact_carries_the_same_role_oid() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let compiled = common::compiled(morpholog_examples::double_entry_ledger::program());
    let acts: Vec<_> = [("t_1", "alex"), ("t_2", "blair")]
        .iter()
        .map(|(entry, actor)| {
            common::attested(&morpholog_core::Transition {
                transformation_name: "post_simple_entry".into(),
                args: common::ledger_args(entry),
                actor: morpholog_core::Subject::from(*actor),
            })
        })
        .collect();
    let outcome = morpholog_postgres::propose_all_against_pg(&pool, &compiled, &acts)
        .await
        .unwrap();
    assert!(
        matches!(
            outcome,
            morpholog_postgres::PgAtomicOutcome::Committed { .. }
        ),
        "{outcome:?}"
    );
    let me: String = sqlx::query_scalar("SELECT session_user")
        .fetch_one(&pool)
        .await
        .unwrap();
    let expected = Some(role_oid(&pool, &me).await);
    assert_eq!(
        oids(&list_audit_rows(&pool).await.unwrap()),
        [expected, expected]
    );
}
