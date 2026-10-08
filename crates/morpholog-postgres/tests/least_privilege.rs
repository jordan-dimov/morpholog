//! The least-privilege floor as PostgreSQL itself enforces it, and the
//! boundary between deployments that share one cluster.
//!
//! The attacker modelled here holds one deployment's group role, through
//! a login granted it, and nothing else: no superuser, no ownership, no
//! role-creation right. Every probe is a statement that reads or writes no
//! rows, so it is refused for privilege or not at all.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_postgres::{
    DeploymentRoles, apply_migrations, databases_also_reached, deployment_roles, initialise_schema,
    provision_least_privilege, with_default_user,
};
use sqlx::PgPool;

/// What a writer does: admit and retract claims, append audit and
/// rejections, lease the outbox, record checkpoints, refresh the read
/// cache.
const WRITES: &[(&str, &str)] = &[
    (
        "claims insert",
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in) \
         SELECT predicate_name, arguments, asserted_in FROM morpholog.claims WHERE false",
    ),
    ("claims delete", "DELETE FROM morpholog.claims WHERE false"),
    (
        "audit insert",
        "INSERT INTO morpholog.audit (transition_id) \
         SELECT transition_id FROM morpholog.audit WHERE false",
    ),
    (
        "rejections insert",
        "INSERT INTO morpholog.rejections (rejection_id) \
         SELECT rejection_id FROM morpholog.rejections WHERE false",
    ),
    (
        "outbox update",
        "UPDATE morpholog.outbox SET locked_by = locked_by WHERE false",
    ),
    (
        "checkpoint insert",
        "INSERT INTO morpholog.audit_checkpoints (checkpoint_id) \
         SELECT checkpoint_id FROM morpholog.audit_checkpoints WHERE false",
    ),
    (
        "read cache delete",
        "DELETE FROM morpholog_read.derived_claims WHERE false",
    ),
];

/// What a reader does: read the claims, the audit log and the read cache.
const READS: &[(&str, &str)] = &[
    ("claims read", "SELECT 1 FROM morpholog.claims LIMIT 0"),
    ("audit read", "SELECT 1 FROM morpholog.audit LIMIT 0"),
    (
        "read cache read",
        "SELECT 1 FROM morpholog_read.derived_claims LIMIT 0",
    ),
];

/// The default names are cluster-global and fixed, so the tests that need
/// them take turns.
static DEFAULT_NAMES: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn base_url() -> Option<String> {
    std::env::var("DATABASE_URL").ok()
}

/// The same connection URL, pointing at another database.
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

/// Scratch databases and roles a test owns: removed before it starts, in
/// case an earlier run died, and again when it ends.
struct Scratch {
    admin: PgPool,
    base: String,
    databases: Vec<String>,
    roles: Vec<String>,
}

impl Scratch {
    async fn new(base: &str, databases: &[&str], roles: &[&str]) -> Self {
        let admin = PgPool::connect(&with_default_user(&with_database(base, "postgres")))
            .await
            .expect("maintenance database");
        let scratch = Self {
            admin,
            base: base.to_string(),
            databases: databases.iter().map(ToString::to_string).collect(),
            roles: roles.iter().map(ToString::to_string).collect(),
        };
        scratch.clean().await;
        for db in &scratch.databases {
            run(&scratch.admin, &format!("CREATE DATABASE {db}")).await;
        }
        scratch
    }

    /// A pool on one of this test's databases, with the schema initialised.
    async fn deployment(&self, db: &str) -> PgPool {
        let pool = PgPool::connect(&with_default_user(&with_database(&self.base, db)))
            .await
            .expect("scratch database");
        initialise_schema(&pool).await.expect("initialise");
        pool
    }

    async fn clean(&self) {
        for db in &self.databases {
            run(
                &self.admin,
                &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"),
            )
            .await;
        }
        for role in &self.roles {
            run(&self.admin, &format!("DROP ROLE IF EXISTS {role}")).await;
        }
    }
}

fn roles(prefix: &str) -> DeploymentRoles {
    DeploymentRoles::with_prefix(prefix).unwrap()
}

/// Run `sql` as `role` and return the SQLSTATE it was refused with, if any.
async fn as_role(pool: &PgPool, role: &str, sql: &str) -> Option<String> {
    let mut conn = pool.acquire().await.unwrap();
    run_on(&mut conn, &format!("SET ROLE {role}")).await;
    let outcome = sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(&mut *conn)
        .await;
    run_on(&mut conn, "RESET ROLE").await;
    match outcome {
        Ok(_) => None,
        Err(sqlx::Error::Database(db)) => Some(db.code().unwrap_or_default().to_string()),
        Err(other) => panic!("{sql}: {other}"),
    }
}

async fn run_on(conn: &mut sqlx::PgConnection, sql: &str) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql.to_string()))
        .execute(&mut *conn)
        .await
        .unwrap();
}

/// Each probe as `writer` and `reader`, with its SQLSTATE or `None`.
async fn probes(pool: &PgPool, roles: &DeploymentRoles) -> Vec<(&'static str, Option<String>)> {
    let mut out = Vec::new();
    for (what, sql) in WRITES {
        out.push((*what, as_role(pool, roles.writer(), sql).await));
    }
    for (what, sql) in READS {
        out.push((*what, as_role(pool, roles.reader(), sql).await));
    }
    out
}

fn all(outcome: Option<&str>) -> Vec<(&'static str, Option<String>)> {
    WRITES
        .iter()
        .chain(READS)
        .map(|(what, _)| (*what, outcome.map(str::to_string)))
        .collect()
}

#[tokio::test]
async fn the_writer_appends_audit_but_never_rewrites_it() {
    let Some(base) = base_url() else { return };
    let scratch = Scratch::new(
        &base,
        &["morpholog_ci_lp_append"],
        &[
            "morpholog_ci_lp_append_writer",
            "morpholog_ci_lp_append_reader",
        ],
    )
    .await;
    let pool = scratch.deployment("morpholog_ci_lp_append").await;
    let mine = roles("morpholog_ci_lp_append_");
    provision_least_privilege(&pool, &mine)
        .await
        .expect("first");
    provision_least_privilege(&pool, &mine)
        .await
        .expect("again: the recorded roles, grants reapplied");

    assert_eq!(probes(&pool, &mine).await, all(None));
    assert_eq!(
        as_role(&pool, mine.writer(), "DELETE FROM morpholog.audit")
            .await
            .as_deref(),
        Some("42501"),
        "the audit log is append-only even for the writer"
    );
    pool.close().await;
    scratch.clean().await;
}

/// Two deployments on one cluster: each one's roles act in its own
/// database and carry nothing into the other.
#[tokio::test]
async fn a_deployments_roles_carry_no_authority_into_another() {
    let Some(base) = base_url() else { return };
    let (a, b) = ("morpholog_ci_lp_a", "morpholog_ci_lp_b");
    let scratch = Scratch::new(
        &base,
        &[a, b],
        &[
            "morpholog_ci_lp_a_writer",
            "morpholog_ci_lp_a_reader",
            "morpholog_ci_lp_b_writer",
            "morpholog_ci_lp_b_reader",
        ],
    )
    .await;
    let (pool_a, pool_b) = (scratch.deployment(a).await, scratch.deployment(b).await);
    let (roles_a, roles_b) = (roles("morpholog_ci_lp_a_"), roles("morpholog_ci_lp_b_"));
    provision_least_privilege(&pool_a, &roles_a).await.unwrap();
    provision_least_privilege(&pool_b, &roles_b).await.unwrap();

    assert_eq!(probes(&pool_b, &roles_a).await, all(Some("42501")));
    assert_eq!(probes(&pool_a, &roles_b).await, all(Some("42501")));
    assert_eq!(probes(&pool_a, &roles_a).await, all(None));
    assert_eq!(probes(&pool_b, &roles_b).await, all(None));

    assert_eq!(
        deployment_roles(&pool_a).await.unwrap(),
        Some(roles_a.clone())
    );
    assert_eq!(
        deployment_roles(&pool_b).await.unwrap(),
        Some(roles_b.clone())
    );
    assert!(
        databases_also_reached(&pool_a, &roles_a)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        databases_also_reached(&pool_b, &roles_b)
            .await
            .unwrap()
            .is_empty()
    );

    // A grant on the other database itself, not on anything in it, is
    // reaching it too.
    run(
        &scratch.admin,
        &format!("GRANT CREATE ON DATABASE {b} TO morpholog_ci_lp_a_writer"),
    )
    .await;
    assert_eq!(
        databases_also_reached(&pool_a, &roles_a).await.unwrap(),
        vec![b.to_string()]
    );

    pool_a.close().await;
    pool_b.close().await;
    scratch.clean().await;
}

/// A second deployment cannot take roles a first one holds: the refusal
/// names the first, and nothing is granted or recorded.
#[tokio::test]
async fn roles_another_deployment_holds_are_refused() {
    let Some(base) = base_url() else { return };
    let (a, b) = ("morpholog_ci_lp_held_a", "morpholog_ci_lp_held_b");
    let scratch = Scratch::new(
        &base,
        &[a, b],
        &["morpholog_ci_lp_held_writer", "morpholog_ci_lp_held_reader"],
    )
    .await;
    let (pool_a, pool_b) = (scratch.deployment(a).await, scratch.deployment(b).await);
    let shared = roles("morpholog_ci_lp_held_");
    provision_least_privilege(&pool_a, &shared).await.unwrap();

    let refused = provision_least_privilege(&pool_b, &shared)
        .await
        .expect_err("the roles belong to deployment A")
        .to_string();
    assert!(refused.contains(a), "names the other database: {refused}");
    assert_eq!(deployment_roles(&pool_b).await.unwrap(), None);
    assert!(
        databases_also_reached(&pool_a, &shared)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(probes(&pool_b, &shared).await, all(Some("42501")));

    pool_a.close().await;
    pool_b.close().await;
    scratch.clean().await;
}

/// A role that already exists is never adopted, however plain: a
/// deployment's roles are created by its own provisioning, where the name
/// itself decides between two databases provisioning at once.
#[tokio::test]
async fn an_existing_role_this_database_does_not_record_is_refused() {
    let Some(base) = base_url() else { return };
    let db = "morpholog_ci_lp_exists";
    let (writer, reader) = (
        "morpholog_ci_lp_exists_writer",
        "morpholog_ci_lp_exists_reader",
    );
    let scratch = Scratch::new(&base, &[db], &[writer, reader]).await;
    let pool = scratch.deployment(db).await;
    run(&scratch.admin, &format!("CREATE ROLE {writer} NOLOGIN")).await;

    let refused = provision_least_privilege(&pool, &roles("morpholog_ci_lp_exists_"))
        .await
        .expect_err("a plain pre-created role is still not adopted")
        .to_string();
    assert!(
        refused.contains(&format!("`{writer}` already exists"))
            && refused.contains("does not record it"),
        "{refused}"
    );
    assert_eq!(deployment_roles(&pool).await.unwrap(), None);
    assert_eq!(
        as_role(&pool, writer, WRITES[2].1).await.as_deref(),
        Some("42501"),
        "no grant was made"
    );
    let reader_made =
        sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
            .bind(reader)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!reader_made, "nothing outlives the refusal");

    pool.close().await;
    scratch.clean().await;
}

/// Creating the roles, recording them and granting are one transaction: a
/// grant that fails leaves no role and no record behind.
#[tokio::test]
async fn a_failed_provisioning_leaves_no_role_and_no_record() {
    let Some(base) = base_url() else { return };
    let db = "morpholog_ci_lp_atomic";
    let scratch = Scratch::new(
        &base,
        &[db],
        &[
            "morpholog_ci_lp_atomic_writer",
            "morpholog_ci_lp_atomic_reader",
        ],
    )
    .await;
    let pool = scratch.deployment(db).await;
    // A table the floor grants on is missing, so the grants fail after
    // the roles are created and the record is written.
    run(&pool, "DROP TABLE morpholog.rejections").await;

    provision_least_privilege(&pool, &roles("morpholog_ci_lp_atomic_"))
        .await
        .expect_err("the floor names a table that is gone");
    assert_eq!(deployment_roles(&pool).await.unwrap(), None);
    let created = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM pg_roles WHERE rolname LIKE 'morpholog_ci_lp_atomic_%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(created, 0, "no role outlives the failed provisioning");

    pool.close().await;
    scratch.clean().await;
}

/// A recorded role that is gone is never recreated: migrating and
/// re-provisioning both refuse, even with nothing to migrate.
#[tokio::test]
async fn a_missing_recorded_role_is_refused_never_recreated() {
    let Some(base) = base_url() else { return };
    let db = "morpholog_ci_lp_gone";
    let scratch = Scratch::new(
        &base,
        &[db],
        &["morpholog_ci_lp_gone_writer", "morpholog_ci_lp_gone_reader"],
    )
    .await;
    let pool = scratch.deployment(db).await;
    let mine = roles("morpholog_ci_lp_gone_");
    provision_least_privilege(&pool, &mine).await.unwrap();
    run(&pool, "DROP OWNED BY morpholog_ci_lp_gone_writer").await;
    run(&scratch.admin, "DROP ROLE morpholog_ci_lp_gone_writer").await;

    let migrate = apply_migrations(&pool)
        .await
        .expect_err("migrate")
        .to_string();
    let again = provision_least_privilege(&pool, &mine)
        .await
        .expect_err("init")
        .to_string();
    for refusal in [&migrate, &again] {
        assert!(
            refusal.contains("`morpholog_ci_lp_gone_writer` does not exist")
                && refusal.contains("never recreates"),
            "{refusal}"
        );
    }
    let recreated = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'morpholog_ci_lp_gone_writer')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!recreated);

    pool.close().await;
    scratch.clean().await;
}

/// Create the default roles the cluster lacks, returning those this test
/// made (and so removes).
async fn ensure_default_roles(admin: &PgPool) -> Vec<&'static str> {
    let mut made = Vec::new();
    for role in ["morpholog_writer", "morpholog_reader"] {
        let exists = sqlx::query_scalar::<_, bool>(
            "SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)",
        )
        .bind(role)
        .fetch_one(admin)
        .await
        .unwrap();
        if !exists {
            run(admin, &format!("CREATE ROLE {role} NOLOGIN")).await;
            made.push(role);
        }
    }
    made
}

/// Every grant on a governed table to a role other than its owner.
async fn foreign_grants(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT c.relname || ' to ' || a.grantee::regrole::text
         FROM pg_class c, aclexplode(c.relacl) a
         WHERE c.relnamespace IN ('morpholog'::regnamespace, 'morpholog_read'::regnamespace)
           AND a.grantee <> c.relowner
         ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Migrating grants only to the roles a database records. A database
/// with no floor gets no grant, whatever roles the cluster holds.
#[tokio::test]
async fn migrate_grants_only_to_recorded_roles() {
    let Some(base) = base_url() else { return };
    let _turn = DEFAULT_NAMES.lock().await;
    let db = "morpholog_ci_lp_nofloor";
    let scratch = Scratch::new(&base, &[db], &[]).await;
    let made = ensure_default_roles(&scratch.admin).await;
    let pool = scratch.deployment(db).await;

    let report = apply_migrations(&pool).await.unwrap();
    assert!(report.is_current(), "{report:?}");
    assert_eq!(foreign_grants(&pool).await, Vec::<String>::new());
    assert_eq!(deployment_roles(&pool).await.unwrap(), None);

    pool.close().await;
    scratch.clean().await;
    for role in made {
        run(&scratch.admin, &format!("DROP ROLE {role}")).await;
    }
}

/// `migrate` re-applies the floor to the recorded roles on every run,
/// with nothing to apply too: a grant withdrawn by hand, or lost in a
/// restore, is back after the next `migrate`.
#[tokio::test]
async fn migrate_re_applies_the_floor_on_every_run() {
    let Some(base) = base_url() else { return };
    let db = "morpholog_ci_lp_refloor";
    let roles = roles("morpholog_ci_lp_refloor_");
    let scratch = Scratch::new(&base, &[db], &[roles.writer(), roles.reader()]).await;
    let pool = scratch.deployment(db).await;
    provision_least_privilege(&pool, &roles).await.unwrap();
    assert_eq!(as_role(&pool, roles.reader(), READS[1].1).await, None);

    run(
        &pool,
        &format!("REVOKE SELECT ON morpholog.audit FROM {}", roles.reader()),
    )
    .await;
    assert_eq!(
        as_role(&pool, roles.reader(), READS[1].1).await.as_deref(),
        Some("42501"),
        "the grant is withdrawn"
    );
    let report = apply_migrations(&pool).await.unwrap();
    assert!(report.applied.is_empty(), "nothing to apply: {report:?}");
    assert_eq!(
        as_role(&pool, roles.reader(), READS[1].1).await,
        None,
        "the floor is back"
    );

    pool.close().await;
    scratch.clean().await;
}

/// The SQL of the install guide's procedure for moving a deployment to its
/// own roles, as written there.
fn documented_move() -> String {
    let guide = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/install.md"),
    )
    .unwrap();
    let section = guide
        .split_once("### Moving a deployment to its own roles")
        .expect("the guide has the procedure")
        .1;
    let block = section.split_once("```sql\n").expect("its SQL").1;
    block.split_once("```").unwrap().0.to_string()
}

/// The guide's procedure ends a sharing: run on one of two deployments
/// that share roles, it leaves each with roles of its own and neither
/// reaching the other.
#[tokio::test]
async fn the_documented_move_ends_a_shared_floor() {
    let Some(base) = base_url() else { return };
    let (a, b) = ("morpholog_ci_lp_move_a", "morpholog_ci_lp_move_b");
    let scratch = Scratch::new(
        &base,
        &[a, b],
        &[
            "morpholog_ci_lp_move_writer",
            "morpholog_ci_lp_move_reader",
            "morpholog_ci_lp_moved_writer",
            "morpholog_ci_lp_moved_reader",
        ],
    )
    .await;
    let (pool_a, pool_b) = (scratch.deployment(a).await, scratch.deployment(b).await);
    let shared = roles("morpholog_ci_lp_move_");
    provision_least_privilege(&pool_a, &shared).await.unwrap();
    // B shares A's roles, as two deployments provisioned before the check
    // and recorded by the migration do.
    run(
        &pool_b,
        "INSERT INTO morpholog.deployment_roles (writer_role, reader_role)
             VALUES ('morpholog_ci_lp_move_writer', 'morpholog_ci_lp_move_reader');
         GRANT USAGE ON SCHEMA morpholog, morpholog_read
             TO morpholog_ci_lp_move_writer, morpholog_ci_lp_move_reader;
         GRANT SELECT, INSERT, DELETE ON morpholog.claims TO morpholog_ci_lp_move_writer;
         GRANT SELECT, INSERT ON morpholog.audit TO morpholog_ci_lp_move_writer;
         GRANT SELECT ON ALL TABLES IN SCHEMA morpholog TO morpholog_ci_lp_move_reader",
    )
    .await;
    assert_eq!(
        databases_also_reached(&pool_a, &shared).await.unwrap(),
        vec![b.to_string()],
        "the sharing `migrate` warns about"
    );

    let procedure = documented_move()
        .replace("morpholog_writer", shared.writer())
        .replace("morpholog_reader", shared.reader());
    run(&pool_b, &procedure).await;
    let own = roles("morpholog_ci_lp_moved_");
    provision_least_privilege(&pool_b, &own)
        .await
        .expect("the guide's init after the guide's SQL");

    assert!(
        databases_also_reached(&pool_a, &shared)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        databases_also_reached(&pool_b, &own)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(probes(&pool_b, &shared).await, all(Some("42501")));
    assert_eq!(probes(&pool_b, &own).await, all(None));
    assert_eq!(probes(&pool_a, &shared).await, all(None));

    pool_a.close().await;
    pool_b.close().await;
    scratch.clean().await;
}

/// Two deployments creating the same roles at once: the second creation
/// waits for the first and fails on the name, never sharing the roles.
#[tokio::test]
async fn concurrent_creation_of_the_same_roles_shares_nothing() {
    let Some(base) = base_url() else { return };
    let (a, b) = ("morpholog_ci_lp_race2_a", "morpholog_ci_lp_race2_b");
    let (writer, reader) = (
        "morpholog_ci_lp_race2_writer",
        "morpholog_ci_lp_race2_reader",
    );
    let scratch = Scratch::new(&base, &[a, b], &[writer, reader]).await;
    let (pool_a, pool_b) = (scratch.deployment(a).await, scratch.deployment(b).await);
    let shared = roles("morpholog_ci_lp_race2_");

    let mut a_tx = pool_a.begin().await.unwrap();
    for step in [
        format!("CREATE ROLE {writer} NOLOGIN"),
        format!("CREATE ROLE {reader} NOLOGIN"),
        format!("GRANT USAGE ON SCHEMA morpholog TO {writer}, {reader}"),
    ] {
        sqlx::raw_sql(sqlx::AssertSqlSafe(step))
            .execute(&mut *a_tx)
            .await
            .unwrap();
    }
    let b_provisioning = {
        let (pool_b, shared) = (pool_b.clone(), shared.clone());
        tokio::spawn(async move { provision_least_privilege(&pool_b, &shared).await })
    };
    for _ in 0..100 {
        let waiting = sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM pg_stat_activity WHERE datname = $1 AND wait_event_type = 'Lock'",
        )
        .bind(b)
        .fetch_one(&scratch.admin)
        .await
        .unwrap();
        if waiting > 0 || b_provisioning.is_finished() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    a_tx.commit().await.unwrap();

    b_provisioning
        .await
        .unwrap()
        .expect_err("B must not take roles A created first");
    assert_eq!(deployment_roles(&pool_b).await.unwrap(), None);
    assert!(
        databases_also_reached(&pool_a, &shared)
            .await
            .unwrap()
            .is_empty()
    );

    pool_a.close().await;
    pool_b.close().await;
    scratch.clean().await;
}

/// Binding a dropped schema's roles again restores roles that still
/// exist; a recorded role that is gone is refused, never recreated.
#[tokio::test]
async fn a_rebind_never_recreates_a_missing_role() {
    let Some(base) = base_url() else { return };
    let db = "morpholog_ci_lp_rebind";
    let (writer, reader) = (
        "morpholog_ci_lp_rebind_writer",
        "morpholog_ci_lp_rebind_reader",
    );
    let scratch = Scratch::new(&base, &[db], &[writer, reader]).await;
    let pool = scratch.deployment(db).await;
    provision_least_privilege(&pool, &roles("morpholog_ci_lp_rebind_"))
        .await
        .unwrap();

    let dropped = morpholog_postgres::drop_schema(&pool).await.unwrap();
    let recorded = dropped
        .roles
        .expect("the drop hands back the recorded roles");
    assert_eq!(*recorded.roles(), roles("morpholog_ci_lp_rebind_"));
    run(&pool, &format!("DROP OWNED BY {writer}")).await;
    run(&scratch.admin, &format!("DROP ROLE {writer}")).await;
    initialise_schema(&pool).await.unwrap();

    let refused = morpholog_postgres::rebind_least_privilege(&pool, recorded)
        .await
        .expect_err("the writer is gone")
        .to_string();
    assert!(
        refused.contains(&format!("`{writer}` does not exist"))
            && refused.contains("never recreates"),
        "{refused}"
    );
    let recreated =
        sqlx::query_scalar::<_, bool>("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
            .bind(writer)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!recreated);
    assert_eq!(deployment_roles(&pool).await.unwrap(), None);

    pool.close().await;
    scratch.clean().await;
}

/// What a dropped schema hands back binds again only in the database it
/// came from: another database cannot take its roles with it.
#[tokio::test]
async fn recorded_roles_bind_again_only_where_they_were_recorded() {
    let Some(base) = base_url() else { return };
    let (a, b) = ("morpholog_ci_lp_carry_a", "morpholog_ci_lp_carry_b");
    let scratch = Scratch::new(
        &base,
        &[a, b],
        &[
            "morpholog_ci_lp_carry_writer",
            "morpholog_ci_lp_carry_reader",
        ],
    )
    .await;
    let (pool_a, pool_b) = (scratch.deployment(a).await, scratch.deployment(b).await);
    let own = roles("morpholog_ci_lp_carry_");
    provision_least_privilege(&pool_a, &own).await.unwrap();
    let recorded = morpholog_postgres::drop_schema(&pool_a)
        .await
        .unwrap()
        .roles
        .expect("A recorded its roles");

    let refused = morpholog_postgres::rebind_least_privilege(&pool_b, recorded)
        .await
        .expect_err("A's roles are not B's to bind")
        .to_string();
    assert!(
        refused.contains("recorded by another database"),
        "{refused}"
    );
    assert_eq!(deployment_roles(&pool_b).await.unwrap(), None);
    assert_eq!(probes(&pool_b, &own).await, all(Some("42501")));
    assert!(
        databases_also_reached(&pool_a, &own)
            .await
            .unwrap()
            .is_empty()
    );

    // From the database it describes, the value binds again. A's schema
    // went with its record, so give A back the record it had and reset it.
    initialise_schema(&pool_a).await.unwrap();
    run(
        &pool_a,
        "INSERT INTO morpholog.deployment_roles (writer_role, reader_role)
         VALUES ('morpholog_ci_lp_carry_writer', 'morpholog_ci_lp_carry_reader')",
    )
    .await;
    let recorded = morpholog_postgres::drop_schema(&pool_a)
        .await
        .unwrap()
        .roles
        .unwrap();
    initialise_schema(&pool_a).await.unwrap();
    morpholog_postgres::rebind_least_privilege(&pool_a, recorded)
        .await
        .expect("A binds its own roles again");
    assert_eq!(probes(&pool_a, &own).await, all(None));

    pool_a.close().await;
    pool_b.close().await;
    scratch.clean().await;
}
