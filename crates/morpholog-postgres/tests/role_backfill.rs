//! Migration 023 decides in SQL what `morpholog.deployment_roles` will hold;
//! `preview_role_backfill` asks the same question without writing. These
//! tests hold the two together on isolated databases built to the shapes
//! an upgrade meets: the fixed pair granted the floor by name, nothing
//! granted, the floor held only through another role, one privilege short,
//! the record table already present with 023 still pending, and that table
//! in a shape the migration refuses. A database ahead of the binary gets
//! no forecast at all.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_postgres::{
    BackfillOutcome, BackfillPhase, DeploymentRoles, apply_migrations, direct_members,
    migration_check, other_sessions, single_connection_pool, with_default_user,
};
use sqlx::PgPool;

fn with_database(url: &str, name: &str) -> String {
    let (base, query) = url
        .split_once('?')
        .map_or((url, None), |(b, q)| (b, Some(q)));
    let swapped = match base.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/{name}"),
        None => base.to_string(),
    };
    match query {
        Some(q) => format!("{swapped}?{q}"),
        None => swapped,
    }
}

async fn run(pool: &PgPool, sql: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(pool)
        .await
        .unwrap_or_else(|e| panic!("{sql}: {e}"));
}

const DATABASE: &str = "morpholog_ci_role_backfill";
const HOLDER: &str = "morpholog_ci_rb_holder";
const OWN_PAIR: [&str; 2] = ["morpholog_ci_rb_writer", "morpholog_ci_rb_reader"];
const MEMBER: &str = "morpholog_ci_rb_member";

/// The cluster's fixed pair, created for the test when absent and dropped
/// again only then: another deployment on this cluster may own them.
struct FixedPair {
    created: bool,
}

async fn ensure_fixed_pair(admin: &PgPool) -> FixedPair {
    let fixed = DeploymentRoles::default();
    let present: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
            .bind(fixed.writer())
            .fetch_one(admin)
            .await
            .unwrap();
    if !present {
        run(
            admin,
            &format!(
                "CREATE ROLE {} NOLOGIN; CREATE ROLE {} NOLOGIN",
                fixed.writer(),
                fixed.reader()
            ),
        )
        .await;
    }
    FixedPair { created: !present }
}

async fn drop_fixed_pair_if_created(admin: &PgPool, pair: &FixedPair) {
    if pair.created {
        let fixed = DeploymentRoles::default();
        run(
            admin,
            &format!("DROP ROLE {}; DROP ROLE {}", fixed.writer(), fixed.reader()),
        )
        .await;
    }
}

/// A database at the head with migration 023 wound back: its table gone and
/// its ledger row removed, as a database from before the record looks.
async fn pre_023_database(admin: &PgPool, base: &str) -> PgPool {
    run(
        admin,
        &format!("DROP DATABASE IF EXISTS {DATABASE} WITH (FORCE)"),
    )
    .await;
    run(admin, &format!("CREATE DATABASE {DATABASE}")).await;
    let pool = single_connection_pool(&with_database(base, DATABASE))
        .await
        .unwrap();
    morpholog_postgres::initialise_schema(&pool).await.unwrap();
    run(
        &pool,
        "DROP TABLE morpholog.deployment_roles; \
         DELETE FROM morpholog.schema_migrations WHERE version = 23",
    )
    .await;
    pool
}

/// The floor 023 looks for, granted to the named roles by name.
fn floor_sql(writer: &str, reader: &str) -> String {
    format!(
        "GRANT USAGE ON SCHEMA morpholog TO {writer}, {reader}; \
         GRANT INSERT, DELETE ON morpholog.claims TO {writer}; \
         GRANT INSERT ON morpholog.audit TO {writer}; \
         GRANT UPDATE ON morpholog.outbox TO {writer}; \
         GRANT SELECT ON morpholog.audit TO {reader}"
    )
}

/// Preview, then the real migration; both must say the same.
async fn preview_agrees_with_the_migration(
    pool: &PgPool,
) -> (BackfillOutcome, Option<(String, String)>) {
    let check = migration_check(pool).await.unwrap();
    assert!(
        check.pending.iter().any(|m| m.version == 23),
        "023 is pending in this fixture"
    );
    let preview = check
        .role_backfill
        .expect("023 pending: a preview is reported");
    assert_eq!(preview.phase, BackfillPhase::Preview);

    let applied = apply_migrations(pool).await.unwrap();
    assert!(applied.applied.iter().any(|m| m.version == 23));
    let observed = applied
        .role_backfill
        .expect("023 applied: what it recorded is reported");
    assert_eq!(observed.phase, BackfillPhase::Observed);

    assert_eq!(
        preview.outcome, observed.outcome,
        "preview {preview:?}, observed {observed:?}"
    );
    assert_eq!(preview.roles(), observed.roles());
    (
        observed.outcome,
        observed
            .roles()
            .map(|(w, r)| (w.to_string(), r.to_string())),
    )
}

#[tokio::test]
async fn the_preview_and_migration_023_agree_on_every_shape_an_upgrade_meets() {
    let Ok(base) = std::env::var("DATABASE_URL") else {
        return;
    };
    let base = with_default_user(&base);
    let admin = PgPool::connect(&with_database(&base, "postgres"))
        .await
        .unwrap();
    for role in [HOLDER, OWN_PAIR[0], OWN_PAIR[1]] {
        run(&admin, &format!("DROP ROLE IF EXISTS {role}")).await;
    }
    let fixed = DeploymentRoles::default();
    let pair = ensure_fixed_pair(&admin).await;

    // The floor granted by name: the migration adopts the fixed pair.
    let pool = pre_023_database(&admin, &base).await;
    run(&pool, &floor_sql(fixed.writer(), fixed.reader())).await;
    let (outcome, roles) = preview_agrees_with_the_migration(&pool).await;
    assert_eq!(outcome, BackfillOutcome::RecordPair);
    assert_eq!(
        roles,
        Some((fixed.writer().to_string(), fixed.reader().to_string()))
    );
    pool.close().await;

    // Nothing granted: no record, no floor.
    let pool = pre_023_database(&admin, &base).await;
    let (outcome, roles) = preview_agrees_with_the_migration(&pool).await;
    assert_eq!(outcome, BackfillOutcome::NoRecord);
    assert_eq!(roles, None);
    pool.close().await;

    // The floor held through another role: the pair can exercise it, and
    // the migration still adopts nothing, since no ACL names them.
    run(&admin, &format!("CREATE ROLE {HOLDER} NOLOGIN")).await;
    let pool = pre_023_database(&admin, &base).await;
    run(&pool, &floor_sql(HOLDER, HOLDER)).await;
    run(
        &pool,
        &format!("GRANT {HOLDER} TO {}, {}", fixed.writer(), fixed.reader()),
    )
    .await;
    let (outcome, _) = preview_agrees_with_the_migration(&pool).await;
    assert_eq!(outcome, BackfillOutcome::NoRecord);
    run(
        &pool,
        &format!(
            "REVOKE {HOLDER} FROM {}, {}",
            fixed.writer(),
            fixed.reader()
        ),
    )
    .await;
    pool.close().await;

    // One privilege short of the floor: not adopted. The case that holds the
    // preview's list to the migration's.
    let pool = pre_023_database(&admin, &base).await;
    run(&pool, &floor_sql(fixed.writer(), fixed.reader())).await;
    run(
        &pool,
        &format!("REVOKE UPDATE ON morpholog.outbox FROM {}", fixed.writer()),
    )
    .await;
    let (outcome, _) = preview_agrees_with_the_migration(&pool).await;
    assert_eq!(outcome, BackfillOutcome::NoRecord);
    pool.close().await;

    // The table already present with its own pair and 023 still in the
    // ledger as pending: the migration leaves it alone, and the preview
    // reports what it holds.
    run(
        &admin,
        &format!(
            "CREATE ROLE {} NOLOGIN; CREATE ROLE {} NOLOGIN",
            OWN_PAIR[0], OWN_PAIR[1]
        ),
    )
    .await;
    let pool = pre_023_database(&admin, &base).await;
    run(
        &pool,
        &format!(
            "CREATE TABLE morpholog.deployment_roles (
                 singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
                 writer_role text NOT NULL,
                 reader_role text NOT NULL);
             INSERT INTO morpholog.deployment_roles (writer_role, reader_role)
             VALUES ('{}', '{}')",
            OWN_PAIR[0], OWN_PAIR[1]
        ),
    )
    .await;
    let (outcome, roles) = preview_agrees_with_the_migration(&pool).await;
    assert_eq!(outcome, BackfillOutcome::RecordPair);
    assert_eq!(
        roles,
        Some((OWN_PAIR[0].to_string(), OWN_PAIR[1].to_string()))
    );
    pool.close().await;

    // The table present in another shape: the migration refuses to guess,
    // and so does the forecast, in the same words.
    let pool = pre_023_database(&admin, &base).await;
    run(
        &pool,
        "CREATE TABLE morpholog.deployment_roles (
             singleton boolean PRIMARY KEY DEFAULT true CHECK (singleton),
             writer_role text NOT NULL,
             reader_role text NOT NULL);
         ALTER TABLE morpholog.deployment_roles DROP CONSTRAINT deployment_roles_singleton_check",
    )
    .await;
    let forecast = migration_check(&pool)
        .await
        .expect_err("another shape cannot be forecast");
    assert!(forecast.to_string().contains("another shape"), "{forecast}");
    let migrated = apply_migrations(&pool)
        .await
        .expect_err("023 refuses another shape");
    assert!(migrated.to_string().contains("another shape"), "{migrated}");
    pool.close().await;

    // A database ahead of this binary with 023 missing: diagnosed as ahead,
    // nothing forecast.
    let pool = pre_023_database(&admin, &base).await;
    run(
        &pool,
        &format!(
            "INSERT INTO morpholog.schema_migrations (version, name) \
             VALUES ({}, 'from_a_newer_morpholog')",
            morpholog_postgres::head_version() + 1
        ),
    )
    .await;
    let check = migration_check(&pool).await.unwrap();
    assert_eq!(check.unknown.len(), 1, "{check:?}");
    assert!(check.pending.iter().any(|m| m.version == 23));
    assert_eq!(
        check.role_backfill, None,
        "no forecast on a database ahead: {check:?}"
    );
    pool.close().await;

    run(
        &admin,
        &format!("DROP DATABASE IF EXISTS {DATABASE} WITH (FORCE)"),
    )
    .await;
    for role in [HOLDER, OWN_PAIR[0], OWN_PAIR[1]] {
        run(&admin, &format!("DROP ROLE IF EXISTS {role}")).await;
    }
    drop_fixed_pair_if_created(&admin, &pair).await;
}

#[tokio::test]
async fn direct_members_and_other_sessions_are_read_by_name() {
    let Ok(base) = std::env::var("DATABASE_URL") else {
        return;
    };
    let base = with_default_user(&base);
    let admin = PgPool::connect(&with_database(&base, "postgres"))
        .await
        .unwrap();
    let db = "morpholog_ci_role_members";
    run(
        &admin,
        &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"),
    )
    .await;
    for role in [MEMBER, OWN_PAIR[0], OWN_PAIR[1]] {
        run(&admin, &format!("DROP ROLE IF EXISTS {role}")).await;
    }
    run(&admin, &format!("CREATE DATABASE {db}")).await;
    run(
        &admin,
        &format!(
            "CREATE ROLE {} NOLOGIN; CREATE ROLE {} NOLOGIN; CREATE ROLE {MEMBER} LOGIN; \
             GRANT {} TO {MEMBER}",
            OWN_PAIR[0], OWN_PAIR[1], OWN_PAIR[0]
        ),
    )
    .await;
    let pool = single_connection_pool(&with_database(&base, db))
        .await
        .unwrap();
    let roles = DeploymentRoles::with_prefix("morpholog_ci_rb_").unwrap();

    let members = direct_members(&pool, &roles).await.unwrap();
    assert_eq!(members.writer.len(), 1, "{members:?}");
    assert_eq!(members.writer[0].member, MEMBER);
    assert!(members.writer[0].can_login);
    assert!(members.reader.is_empty());

    // Nobody else yet, then one more session as this user.
    assert_eq!(other_sessions(&pool).await.unwrap(), vec![]);
    let other = PgPool::connect(&with_database(&base, db)).await.unwrap();
    let me: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(&other)
        .await
        .unwrap();
    let sessions = other_sessions(&pool).await.unwrap();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    assert_eq!(sessions[0].role, me);
    assert_eq!(sessions[0].sessions, 1);
    other.close().await;
    pool.close().await;

    run(
        &admin,
        &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"),
    )
    .await;
    for role in [MEMBER, OWN_PAIR[0], OWN_PAIR[1]] {
        run(&admin, &format!("DROP ROLE IF EXISTS {role}")).await;
    }
}
