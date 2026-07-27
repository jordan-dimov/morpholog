//! The upgrade path for an existing database.
//!
//! `morpholog init` is day-zero only: it never drops and never migrates, so
//! an existing deployment gets a new column by applying the numbered file in
//! `crates/morpholog-core/sql/migrations/`. That makes each of those files a
//! claim - "run this and your database matches the head schema" - and a
//! claim needs a check. Without one, a binary that writes a column an
//! operator's table lacks fails on the *refusal* path: the post-rollback
//! insert names a column that is not there, so a lawful rejection surfaces
//! as a database error.
//!
//! **Isolation matters here.** `reset_db` only truncates, so DDL against the
//! shared `morpholog` schema would leave every later test in the run against
//! a shape nobody intended - the failure mode this repo has already been
//! bitten by once.
//!
//! The migration test therefore works in a scratch schema it creates and
//! drops. The drift test cannot: its whole point is what the production
//! query does against the real table, so it removes a column from
//! `morpholog` and puts it back through the shipped migration. That restore
//! runs unconditionally, before any assertion, because an assertion that
//! panics past it would break the rest of the run.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::test_pool;
use sqlx::{PgPool, Row};

const WITNESS_MIGRATION: &str =
    include_str!("../../morpholog-core/sql/migrations/010_rejections_witness.sql");

/// Run one statement whose text this test owns. The scratch schema name is a
/// literal here, never external input.
async fn ddl(pool: &PgPool, sql: String) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
        .execute(pool)
        .await
        .map(|_| ())
}

/// The column shape of one table, as `(name, is_nullable, data_type)`.
async fn columns(pool: &PgPool, schema: &str, table: &str) -> Vec<(String, String, String)> {
    sqlx::query(
        "SELECT column_name, is_nullable, data_type
         FROM information_schema.columns
         WHERE table_schema = $1 AND table_name = $2
         ORDER BY column_name",
    )
    .bind(schema)
    .bind(table)
    .fetch_all(pool)
    .await
    .expect("reading the column shape")
    .into_iter()
    .map(|r| {
        (
            r.get::<String, _>("column_name"),
            r.get::<String, _>("is_nullable"),
            r.get::<String, _>("data_type"),
        )
    })
    .collect()
}

/// Applying the witness migration to a pre-witness table yields exactly the
/// head schema's shape, and leaves the rows already there alone.
///
/// The migration is applied verbatim except for its schema name, rewritten to
/// the scratch schema. That substitution is the one thing this test does not
/// prove; the column, its nullability, the constraint and idempotence are all
/// the shipped file's own DDL.
#[tokio::test]
async fn the_witness_migration_brings_an_old_table_to_the_head_shape() {
    let pool = test_pool().await;
    let scratch = "morpholog_migration_probe";
    ddl(&pool, format!("DROP SCHEMA IF EXISTS {scratch} CASCADE"))
        .await
        .unwrap();
    ddl(&pool, format!("CREATE SCHEMA {scratch}"))
        .await
        .unwrap();

    // The pre-witness shape: the head table with the column removed, which is
    // exactly what an operator who has not migrated is running.
    ddl(
        &pool,
        // INCLUDING ALL so the copy carries NOT NULLs, defaults and checks -
        // a bare CREATE TABLE AS drops them, and the comparison below would
        // then pass on a table nobody runs.
        format!(
            "CREATE TABLE {scratch}.rejections
             (LIKE morpholog.rejections INCLUDING ALL)"
        ),
    )
    .await
    .unwrap();
    ddl(
        &pool,
        format!("ALTER TABLE {scratch}.rejections DROP COLUMN witness"),
    )
    .await
    .expect("the head schema must have the column for this test to mean anything");

    // A refusal recorded before the upgrade, which must survive it.
    ddl(
        &pool,
        format!(
            "INSERT INTO {scratch}.rejections
               (rejection_id, transformation_name, arguments, actor, kind, rule,
                invariant_version, reason, rejected_at)
             VALUES (gen_random_uuid(), 'post', '[]'::jsonb, '{{}}'::jsonb,
                     'invariant', 'entry_unique_by_entry_id', 1, 'historical', now())"
        ),
    )
    .await
    .expect("the old shape accepts an old row");

    let migration =
        WITNESS_MIGRATION.replace("morpholog.rejections", &format!("{scratch}.rejections"));
    // Twice: an operator who re-runs a migration must not be punished for it.
    for _ in 0..2 {
        ddl(&pool, migration.clone())
            .await
            .expect("the migration applies, and applies again");
    }

    assert_eq!(
        columns(&pool, scratch, "rejections").await,
        columns(&pool, "morpholog", "rejections").await,
        "after migrating, the table must match the head schema column for column"
    );

    let surviving: i64 = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM {scratch}.rejections WHERE witness IS NULL"
    )))
    .fetch_one(&pool)
    .await
    .unwrap()
    .get(0);
    assert_eq!(
        surviving, 1,
        "the pre-upgrade row survives, with no witness invented for it"
    );

    // The constraint came with the column, not just the column.
    let empty = ddl(
        &pool,
        format!("UPDATE {scratch}.rejections SET witness = '[]'::jsonb"),
    )
    .await;
    assert!(
        empty.is_err(),
        "an empty witness must be unrepresentable after migrating too"
    );

    ddl(&pool, format!("DROP SCHEMA {scratch} CASCADE"))
        .await
        .unwrap();
}

/// An un-migrated database says so, on the path that actually breaks.
///
/// The damaging scenario is not a read: it is the post-rollback INSERT in
/// `write_rejection`, which turns a lawful refusal into an operational error
/// when the column is absent. Commits keep working, so nothing looks wrong
/// until the first refusal - and for an embedder that arrives as an
/// exception where a decided outcome belongs.
///
/// **On its own database.** An earlier version dropped the column from the
/// shared `morpholog` schema and restored it afterwards, which is a
/// contamination risk this module's own doctrine forbids: a panic, a kill,
/// or a future refactor between the two leaves every later test facing a
/// table missing a column. A database created and dropped here cannot reach
/// anything else, whatever happens in between.
#[tokio::test]
async fn an_unmigrated_database_names_the_remedy_on_the_refusal_path() {
    let Ok(base) = std::env::var("DATABASE_URL") else {
        return;
    };
    let name = "morpholog_drift_probe";
    let admin = morpholog_postgres::with_default_user(&with_database(&base, "postgres"));
    let admin_pool = sqlx::PgPool::connect(&admin)
        .await
        .expect("connect to the maintenance database");
    ddl(&admin_pool, format!("DROP DATABASE IF EXISTS {name}"))
        .await
        .unwrap();
    ddl(&admin_pool, format!("CREATE DATABASE {name}"))
        .await
        .expect("create the throwaway database");

    let probe_url = morpholog_postgres::with_default_user(&with_database(&base, name));
    let outcome = drift_probe(&probe_url).await;

    // Drop the database before asserting, so a failed assertion cannot leave
    // it behind for the next run to trip over.
    let pools_closed = sqlx::PgPool::connect(&probe_url).await;
    drop(pools_closed);
    ddl(
        &admin_pool,
        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
    )
    .await
    .unwrap();

    let err = outcome.expect_err("a refusal against a stale schema must fail operationally");
    let rendered = err.to_string();
    assert!(
        matches!(err, morpholog_postgres::PgError::SchemaBehind { .. }),
        "the refusal path must diagnose a stale schema, got {err:?}"
    );
    assert!(
        rendered.contains("sql/migrations/"),
        "the message must name where the remedy lives, got: {rendered}"
    );
}

/// The same connection URL, pointing at a different database.
///
/// Only the last path segment moves. An earlier version used
/// `str::replace` on the database name, which rewrites EVERY occurrence -
/// and CI's URL ends in `/postgres`, so it renamed the scheme too and the
/// connection failed with something that looked nothing like the cause. A
/// local URL whose database name does not collide with the scheme hides
/// that completely.
fn with_database(url: &str, name: &str) -> String {
    match url.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/{name}"),
        None => url.to_string(),
    }
}

/// Provision a database at the PREVIOUS release's shape, commit once, then
/// refuse once. Returns what the refusal produced.
async fn drift_probe(
    url: &str,
) -> Result<morpholog_postgres::PgProposalOutcome, morpholog_postgres::PgError> {
    use morpholog_test_support::{dec, subj};

    let pool = sqlx::PgPool::connect(url)
        .await
        .expect("connect to the probe");
    morpholog_postgres::initialise_schema(&pool)
        .await
        .expect("provision the head schema");
    // Wind it back to the previous release: the column this binary writes.
    ddl(
        &pool,
        "ALTER TABLE morpholog.rejections DROP COLUMN witness".to_string(),
    )
    .await
    .expect("simulate a database from before the migration");

    let program = morpholog_surface::parse_program(DRIFT_FIXTURE).expect("fixture parses");
    program.validate().expect("fixture validates");
    let compiled = morpholog_core::CompiledProgram::new(program).expect("fixture compiles");
    let post = compiled
        .program()
        .transformations
        .iter()
        .find(|t| t.name == "post")
        .expect("fixture declares post")
        .clone();

    let propose = |args: Vec<morpholog_core::EvalValue>| {
        let compiled = &compiled;
        let post = &post;
        let pool = &pool;
        async move {
            let transition = morpholog_core::Transition {
                transformation_name: post.name.clone(),
                args,
                actor: morpholog_core::Subject::from("alex"),
            };
            morpholog_postgres::propose_against_pg(
                pool,
                compiled,
                &morpholog_postgres::Proposal::gateway(&transition),
            )
            .await
        }
    };

    // A commit still works against the stale schema - which is exactly why
    // the failure hides until something is refused. Asserted, not ignored:
    // if this one were refused instead, the refusal below would prove
    // nothing about the path under test.
    let accepted = propose(vec![subj("e1"), dec(100)])
        .await
        .expect("an accepted proposal touches no rejection row");
    assert!(
        matches!(
            accepted,
            morpholog_postgres::PgProposalOutcome::Committed { .. }
        ),
        "the stale schema must not affect the commit path, got {accepted:?}"
    );

    // The same entry id again: refused by the uniqueness discipline, and the
    // refusal is what tries to write the missing column.
    propose(vec![subj("e1"), dec(999)]).await
}

const DRIFT_FIXTURE: &str = r#"
program drift_probe

predicate Entry(entry_id: Subject, amount: Decimal)
    unique by (entry_id)

transformation post(entry_id, amount):
    admit Entry(entry_id, amount)
"#;

/// The URL rewrite, pinned against the shape that broke CI.
///
/// Runs without a database, so the trap stays covered even where the PG
/// suites are skipped.
#[test]
fn with_database_only_moves_the_last_segment() {
    // The CI shape: the database is itself named `postgres`, so a
    // replace-the-name approach corrupts the scheme.
    assert_eq!(
        with_database("postgres://u:p@localhost:5432/postgres", "probe"),
        "postgres://u:p@localhost:5432/probe"
    );
    // The local shape, where that bug is invisible.
    assert_eq!(
        with_database("postgres:///morpholog_dev", "probe"),
        "postgres:///probe"
    );
    // And a name colliding with the user as well as the scheme.
    assert_eq!(
        with_database("postgres://postgres@localhost/postgres", "probe"),
        "postgres://postgres@localhost/probe"
    );
}
