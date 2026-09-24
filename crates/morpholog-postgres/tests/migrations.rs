//! The upgrade path for an existing database.
//!
//! `morpholog init` never migrates, so an existing deployment upgrades by
//! applying the numbered files in `crates/morpholog-core/sql/migrations/`.
//! Each promises "run this and your database matches the head schema", and
//! these tests check that. A missing column shows up on the refusal path:
//! the rejection log insert fails, so a lawful rejection becomes a
//! database error.
//!
//! `reset_db` only truncates, so DDL on the shared `morpholog` schema would
//! leak into every later test. Tests that change the schema work in a
//! scratch schema or database they create and drop, and restore anything
//! shared before asserting.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::test_pool;
use sqlx::{PgPool, Row};

const WITNESS_MIGRATION: &str =
    include_str!("../../morpholog-core/sql/migrations/010_rejections_witness.sql");
const CLAIMS_KEY_MIGRATION: &str =
    include_str!("../../morpholog-core/sql/migrations/012_claims_hash_key.sql");

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

/// The table's primary key as PostgreSQL prints it, or `None` without one.
async fn primary_key(pool: &PgPool, table: &str) -> Option<String> {
    sqlx::query(
        "SELECT pg_get_constraintdef(oid) FROM pg_constraint
         WHERE conrelid = $1::regclass AND contype = 'p'",
    )
    .bind(table)
    .fetch_optional(pool)
    .await
    .expect("reading the primary key")
    .map(|r| r.get::<String, _>(0))
}

/// Put a head-shaped `claims` copy back on the whole-array key.
async fn wind_claims_key_back(pool: &PgPool, table: &str) {
    ddl(
        pool,
        format!("ALTER TABLE {table} DROP COLUMN arguments_hash"),
    )
    .await
    .expect("the head schema must have the digest column for this test to mean anything");
    ddl(
        pool,
        format!("ALTER TABLE {table} ADD PRIMARY KEY (predicate_name, arguments)"),
    )
    .await
    .unwrap();
}

/// Put a head-shaped `derived_claims` copy back on the whole-array key.
async fn wind_derived_key_back(pool: &PgPool, table: &str) {
    ddl(
        pool,
        format!("ALTER TABLE {table} ADD PRIMARY KEY (refresh_id, predicate_name, arguments)"),
    )
    .await
    .unwrap();
}

/// Applying the witness migration to a pre-witness table yields exactly the
/// head schema's shape, and leaves the rows already there alone.
///
/// The migration runs verbatim except that its schema name is rewritten to
/// the scratch schema.
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

    // The pre-witness shape: the head table without the column.
    ddl(
        &pool,
        // INCLUDING ALL keeps NOT NULLs, defaults and checks, which a bare
        // CREATE TABLE AS would drop.
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
/// That path is the post-rollback INSERT in `write_rejection`: with the
/// column absent, a lawful refusal becomes an operational error. Commits
/// keep working, so nothing looks wrong until the first refusal.
///
/// It runs on its own database, so a panic midway cannot leave the shared
/// schema missing a column.
#[tokio::test]
async fn an_unmigrated_database_names_the_remedy_on_the_refusal_path() {
    let Ok(base) = std::env::var("DATABASE_URL") else {
        return;
    };
    // Unique per process: two runs against one server (a developer and CI on
    // the same box, or two jobs) must not drop each other's database.
    let name = format!("morpholog_drift_probe_{}", std::process::id());
    let name = name.as_str();
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
    // The refusal was decided and only its record failed, so the error
    // says that, with the stale-schema diagnosis inside.
    assert!(
        matches!(
            &err,
            morpholog_postgres::PgError::RejectionLogFailure(inner)
                if matches!(**inner, morpholog_postgres::PgError::SchemaBehind { .. })
        ),
        "the refusal path must diagnose a stale schema, got {err:?}"
    );
    // The remedy must be something the reader can run, not a path in the
    // source tree a release user does not have.
    assert!(
        rendered.contains("morpholog migrate"),
        "the message must name the command that fixes it, got: {rendered}"
    );
}

/// The same connection URL, pointing at a different database.
///
/// Only the last path segment changes. Replacing the name as text would
/// also rewrite the scheme when the database is named `postgres`, as in CI.
fn with_database(url: &str, name: &str) -> String {
    // Any query string is carried across untouched: `?sslmode=require` has
    // to survive, and it is not part of the database name.
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
    let program = morpholog_postgres::PgProgram::new(
        morpholog_core::CompiledProgram::new(program).expect("fixture compiles"),
    );
    let post = program
        .core()
        .program()
        .transformations
        .iter()
        .find(|t| t.name == "post")
        .expect("fixture declares post")
        .clone();

    let propose = |args: Vec<morpholog_core::EvalValue>| {
        let program = &program;
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
                program,
                &morpholog_postgres::Proposal::gateway(&transition),
            )
            .await
        }
    };

    // A commit still works against the stale schema, which is why the
    // failure hides until something is refused. Asserted, because if this
    // were refused the refusal below would prove nothing.
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

/// The URL rewrite, including CI's URL shape.
///
/// Needs no database, so it runs even where the PG suites are skipped.
#[test]
fn with_database_only_moves_the_last_segment() {
    // The CI shape: the database is itself named `postgres`, so a
    // replace-the-name approach corrupts the scheme.
    assert_eq!(
        with_database("postgres://u:p@localhost:5432/postgres", "probe"),
        "postgres://u:p@localhost:5432/probe"
    );
    // The local shape.
    assert_eq!(
        with_database("postgres:///morpholog_dev", "probe"),
        "postgres:///probe"
    );
    // And a name colliding with the user as well as the scheme.
    assert_eq!(
        with_database("postgres://postgres@localhost/postgres", "probe"),
        "postgres://postgres@localhost/probe"
    );
    // A query string is not part of the name and must survive.
    assert_eq!(
        with_database("postgres://u@h:5432/postgres?sslmode=require", "probe"),
        "postgres://u@h:5432/probe?sslmode=require"
    );
}

/// The upgrade an operator actually performs, with only the binary.
///
/// A release ships the binary without the source tree, so the migrations
/// come from the ones compiled in; nothing here reads the repository.
#[tokio::test]
async fn a_legacy_database_upgrades_from_the_binary_alone() {
    let Ok(base) = std::env::var("DATABASE_URL") else {
        return;
    };
    let name = format!("morpholog_upgrade_probe_{}", std::process::id());
    let admin = morpholog_postgres::with_default_user(&with_database(&base, "postgres"));
    let admin_pool = sqlx::PgPool::connect(&admin).await.expect("maintenance db");
    ddl(
        &admin_pool,
        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
    )
    .await
    .unwrap();
    ddl(&admin_pool, format!("CREATE DATABASE {name}"))
        .await
        .unwrap();

    let probe_url = morpholog_postgres::with_default_user(&with_database(&base, &name));
    let outcome = upgrade_probe(&probe_url).await;

    ddl(
        &admin_pool,
        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
    )
    .await
    .unwrap();
    outcome.expect("the upgrade path must work end to end");
}

async fn upgrade_probe(url: &str) -> Result<(), String> {
    let pool = sqlx::PgPool::connect(url).await.expect("probe");
    morpholog_postgres::initialise_schema(&pool)
        .await
        .expect("provision");

    // A fresh database is at the head: `init` records the migrations
    // rather than running them. Checked on a database this test created,
    // not a long-lived dev one.
    let fresh = morpholog_postgres::migration_status(&pool)
        .await
        .map_err(|e| format!("status on a fresh database failed: {e}"))?;
    if !fresh.pending.is_empty() {
        return Err(format!(
            "init must leave nothing pending, got {:?}",
            fresh.pending
        ));
    }
    if fresh.recorded_version_after != Some(morpholog_postgres::head_version()) {
        return Err(format!(
            "a fresh database is at the head, got {:?}",
            fresh.recorded_version_after
        ));
    }

    // Wind back to a release before the witness column, the digest key, or
    // the record existed.
    ddl(
        &pool,
        "ALTER TABLE morpholog.rejections DROP COLUMN witness".to_string(),
    )
    .await
    .expect("simulate the older shape");
    wind_claims_key_back(&pool, "morpholog.claims").await;
    ddl(
        &pool,
        "ALTER TABLE morpholog.audit_checkpoints DROP COLUMN witnesses".to_string(),
    )
    .await
    .expect("simulate a database from before checkpoint witnesses");
    ddl(
        &pool,
        "ALTER TABLE morpholog.audit DROP COLUMN parameters".to_string(),
    )
    .await
    .expect("simulate a database from before self-describing rows");
    ddl(
        &pool,
        "DROP FUNCTION morpholog.timestamp_nanos(jsonb)".to_string(),
    )
    .await
    .expect("simulate a database from before the timestamp coordinate");
    // A row that deployment wrote: attested, no names. The migration must
    // carry it forward untouched.
    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents, attestation
         ) VALUES ($1, 'historical', '[]', '{\"type\":\"subject\",\"value\":\"h\"}',
                   1, '[]', '[]', '[]', '[]',
                   '{\"mode\":\"gateway\",\"authenticated_by\":\"h\"}')",
    )
    .bind(uuid::Uuid::now_v7())
    .execute(&pool)
    .await
    .expect("a historical attested row");
    ddl(
        &pool,
        "DROP INDEX morpholog_read.derived_claims_generation_predicate".to_string(),
    )
    .await
    .expect("simulate the older cache shape");
    wind_derived_key_back(&pool, "morpholog_read.derived_claims").await;
    ddl(&pool, "DROP TABLE morpholog.schema_migrations".to_string())
        .await
        .expect("simulate a database from before the record existed");

    // A deploy gate can ask before a workload finds out.
    let before = morpholog_postgres::migration_status(&pool)
        .await
        .map_err(|e| format!("status failed: {e}"))?;
    if before.recorded_version_before.is_some() {
        return Err(format!(
            "a database with no record has no version, got {:?}",
            before.recorded_version_before
        ));
    }
    if before.pending.len() != usize::try_from(morpholog_postgres::head_version()).unwrap() {
        return Err(format!(
            "everything should be pending, got {}",
            before.pending.len()
        ));
    }

    let report = morpholog_postgres::apply_migrations(&pool)
        .await
        .map_err(|e| format!("migrate failed: {e}"))?;
    if report.applied.len() != before.pending.len() {
        return Err(format!(
            "applied {} of {}",
            report.applied.len(),
            before.pending.len()
        ));
    }

    // Current, and re-running changes nothing.
    let after = morpholog_postgres::migration_status(&pool)
        .await
        .map_err(|e| format!("status failed: {e}"))?;
    if !after.pending.is_empty() {
        return Err(format!(
            "still pending after migrating: {:?}",
            after.pending
        ));
    }
    let again = morpholog_postgres::apply_migrations(&pool)
        .await
        .map_err(|e| format!("second migrate failed: {e}"))?;
    if !again.applied.is_empty() {
        return Err(format!(
            "re-running applied {} migrations",
            again.applied.len()
        ));
    }

    // Migration 016: the coordinate function is back, marked as ours.
    let epoch: Option<rust_decimal::Decimal> = sqlx::query_scalar(
        "SELECT morpholog.timestamp_nanos('{\"type\":\"timestamp\",\"value\":\"1970-01-01T00:00:00Z\"}'::jsonb)",
    )
    .fetch_one(&pool)
    .await
    .map_err(|e| format!("timestamp_nanos must exist after migrating: {e}"))?;
    if epoch != Some(rust_decimal::Decimal::ZERO) {
        return Err(format!(
            "timestamp_nanos must give 0 at the epoch, got {epoch:?}"
        ));
    }
    let marker: Option<String> = sqlx::query_scalar(
        "SELECT obj_description('morpholog.timestamp_nanos(jsonb)'::regprocedure, 'pg_proc')",
    )
    .fetch_one(&pool)
    .await
    .map_err(|e| format!("marker lookup failed: {e}"))?;
    if marker.as_deref() != Some("morpholog timestamp coordinate v1") {
        return Err(format!(
            "timestamp_nanos must carry its marker, got {marker:?}"
        ));
    }

    // Migration 014, checked on the migrated table: the old row survives
    // unstamped, the column is nullable, and each named constraint refuses
    // what it is for.
    let stamped = columns(&pool, "morpholog", "audit")
        .await
        .into_iter()
        .find(|(name, _, _)| name == "parameters");
    if stamped
        != Some((
            "parameters".to_string(),
            "YES".to_string(),
            "jsonb".to_string(),
        ))
    {
        return Err(format!(
            "parameters must come back as nullable jsonb, got {stamped:?}"
        ));
    }
    let historical = morpholog_postgres::list_audit_rows(&pool)
        .await
        .map_err(|e| format!("the historical row must still read: {e}"))?;
    if historical.len() != 1 || historical[0].parameters.is_some() {
        return Err(format!(
            "the historical row must survive unstamped, got {historical:?}"
        ));
    }
    let unstamped = sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents, attestation
         ) VALUES ($1, 'stale_binary', '[]', '{\"type\":\"subject\",\"value\":\"s\"}',
                   1, '[]', '[]', '[]', '[]',
                   '{\"mode\":\"gateway\",\"authenticated_by\":\"s\"}')",
    )
    .bind(uuid::Uuid::now_v7())
    .execute(&pool)
    .await;
    match unstamped {
        Err(e) if e.to_string().contains("audit_parameters_required") => {}
        other => {
            return Err(format!(
                "a new unstamped row must be refused by audit_parameters_required, got {other:?}"
            ));
        }
    }
    let wrong_arity = sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents, attestation, parameters
         ) VALUES ($1, 'misshapen', '[]', '{\"type\":\"subject\",\"value\":\"m\"}',
                   1, '[]', '[]', '[]', '[]',
                   '{\"mode\":\"gateway\",\"authenticated_by\":\"m\"}', '[\"extra\"]')",
    )
    .bind(uuid::Uuid::now_v7())
    .execute(&pool)
    .await;
    match wrong_arity {
        Err(e) if e.to_string().contains("audit_parameters_shape") => {}
        other => {
            return Err(format!(
                "names that do not match the arguments must be refused by audit_parameters_shape, got {other:?}"
            ));
        }
    }

    // A lawful refusal now writes its witness instead of failing.
    morpholog_postgres::list_rejection_rows(&pool, 10)
        .await
        .map_err(|e| format!("the rejection log is still unreadable: {e}"))?;
    // The claims key came forward with everything else.
    if primary_key(&pool, "morpholog.claims").await.as_deref()
        != Some("PRIMARY KEY (predicate_name, arguments_hash)")
    {
        return Err("claims must be keyed by the digest after migrating".to_string());
    }
    if primary_key(&pool, "morpholog_read.derived_claims")
        .await
        .is_some()
    {
        return Err("the derived cache must have lost its whole-array key".to_string());
    }

    // Migration 015: the index registry exists, empty, since the command
    // that fills it has not run. Correctness never depends on either table.
    for (table, key) in [
        ("managed_index", "spec_digest"),
        ("index_requirement", "program_identity"),
    ] {
        let names: Vec<String> = columns(&pool, "morpholog", table)
            .await
            .into_iter()
            .map(|(name, _, _)| name)
            .collect();
        if !names.iter().any(|n| n == key) {
            return Err(format!(
                "migration 015 must create morpholog.{table} with {key}; columns: {names:?}"
            ));
        }
    }
    // It refuses a same-named table of another shape, rather than recording
    // the version and failing later. Run with the migration's own SQL on a
    // wrong-shaped twin, then the tables are put back.
    let migration_015 = include_str!("../../morpholog-core/sql/migrations/015_managed_indexes.sql");
    // Two twins: the wrong columns, and the right columns without a
    // constraint the command relies on.
    for (label, twin) in [
        (
            "wrong columns",
            "CREATE TABLE morpholog.managed_index (something_else integer)",
        ),
        (
            "right columns, no unique index_name",
            "CREATE TABLE morpholog.managed_index (
                spec_digest text PRIMARY KEY, index_name text NOT NULL,
                predicate_name text NOT NULL, position integer NOT NULL CHECK (position >= 0),
                representation text NOT NULL, expression_sql text NOT NULL,
                partial_predicate text NOT NULL, registered_at timestamptz NOT NULL DEFAULT now())",
        ),
    ] {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP TABLE IF EXISTS morpholog.index_requirement; DROP TABLE IF EXISTS morpholog.managed_index; {twin}"
        )))
        .execute(&pool)
        .await
        .map_err(|e| format!("shaping the twin ({label}): {e}"))?;
        match sqlx::raw_sql(migration_015).execute(&pool).await {
            Ok(_) => {
                return Err(format!(
                    "migration 015 adopted a managed_index twin ({label})"
                ));
            }
            Err(e) if e.to_string().contains("another shape") => {}
            Err(e) => {
                return Err(format!(
                    "migration 015 refused {label} for the wrong reason: {e}"
                ));
            }
        }
        sqlx::raw_sql("DROP TABLE morpholog.managed_index")
            .execute(&pool)
            .await
            .map_err(|e| format!("removing the twin ({label}): {e}"))?;
    }
    sqlx::raw_sql(migration_015)
        .execute(&pool)
        .await
        .map_err(|e| format!("migration 015 must recreate the registry: {e}"))?;

    Ok(())
}

/// No embedded migration may control transactions.
///
/// The runner applies each migration and writes its version record in one
/// transaction, so the two cannot disagree. A `COMMIT` in a script would
/// end that transaction early, leaving the record outside it.
#[test]
fn no_migration_controls_its_own_transaction() {
    let dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../morpholog-core/sql/migrations");
    let mut checked = 0;
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(&dir).expect("migrations directory") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|e| e != "sql") {
            continue;
        }
        checked += 1;
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let sql = std::fs::read_to_string(&path).expect("read migration");
        for (n, line) in sql.lines().enumerate() {
            let bare = line
                .split("--")
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_uppercase();
            if matches!(bare.as_str(), "BEGIN;" | "COMMIT;" | "ROLLBACK;" | "END;") {
                offenders.push(format!("{name}:{}", n + 1));
            }
        }
    }
    assert!(
        checked > 5,
        "anti-vacuity: found only {checked} migrations to scan"
    );
    assert!(
        offenders.is_empty(),
        "migrations must not open or close transactions - the runner owns them, \
         and a COMMIT here would separate the schema change from its version \
         record. Found at: {}",
        offenders.join(", ")
    );
}

/// A database recording a migration this binary has never seen is not
/// current, and migrating it is refused.
///
/// This is a rollback to an older binary. Nothing is pending, so a naive
/// check would say all is well when the binary cannot know whether the
/// schema is compatible.
#[tokio::test]
async fn a_database_ahead_of_the_binary_is_not_current() {
    let pool = test_pool().await;
    let head = morpholog_postgres::head_version();
    let future = head + 1;

    ddl(
        &pool,
        format!(
            "INSERT INTO morpholog.schema_migrations (version, name)
             VALUES ({future}, 'from_a_newer_morpholog') ON CONFLICT DO NOTHING"
        ),
    )
    .await
    .expect("record a version from the future");

    let status = morpholog_postgres::migration_status(&pool).await;
    let applied = morpholog_postgres::apply_migrations(&pool).await;

    // Clean up before asserting, so a failure cannot leave the shared
    // database claiming to be from the future.
    ddl(
        &pool,
        format!("DELETE FROM morpholog.schema_migrations WHERE version = {future}"),
    )
    .await
    .expect("remove it again");

    let status = status.expect("status still reads");
    assert!(
        status.pending.is_empty(),
        "the trap is that nothing is pending: {:?}",
        status.pending
    );
    assert_eq!(
        status.unknown.iter().map(|m| m.version).collect::<Vec<_>>(),
        vec![future],
        "the unknown version must be named"
    );
    assert!(!status.is_current(), "an ahead database is not current");
    assert!(
        matches!(applied, Err(morpholog_postgres::PgError::InvalidState(_))),
        "migrating an ahead database must be refused, got {applied:?}"
    );
}

/// Applying the claims-key migration to tables on the whole-array key
/// yields exactly the head shape (columns, key, generated digest), keeps
/// existing rows, and refuses a table it does not recognise.
///
/// Table names are rewritten to the scratch schema; the digest helper is
/// the real `morpholog.claim_digest`, so row hashes match production.
#[tokio::test]
async fn the_claims_key_migration_brings_old_tables_to_the_head_shape() {
    let pool = test_pool().await;
    let scratch = "morpholog_claims_key_probe";
    ddl(&pool, format!("DROP SCHEMA IF EXISTS {scratch} CASCADE"))
        .await
        .unwrap();
    ddl(&pool, format!("CREATE SCHEMA {scratch}"))
        .await
        .unwrap();
    ddl(
        &pool,
        format!("CREATE TABLE {scratch}.claims (LIKE morpholog.claims INCLUDING ALL)"),
    )
    .await
    .unwrap();
    wind_claims_key_back(&pool, &format!("{scratch}.claims")).await;
    ddl(
        &pool,
        format!(
            "CREATE TABLE {scratch}.derived_claims
             (LIKE morpholog_read.derived_claims INCLUDING ALL)"
        ),
    )
    .await
    .unwrap();
    // INCLUDING ALL copied the lookup index under a generated name; the
    // pre-migration cache had only its key.
    ddl(
        &pool,
        format!("DROP INDEX {scratch}.derived_claims_refresh_id_predicate_name_idx"),
    )
    .await
    .expect("the copied lookup index has PostgreSQL's generated name");
    wind_derived_key_back(&pool, &format!("{scratch}.derived_claims")).await;

    // Rows from before the upgrade, which must survive it, with awkward
    // text the digest must carry intact.
    let hostile = r#"[{"type":"subject","value":"He said \"no\" \\ ünïcode"}]"#;
    ddl(
        &pool,
        format!(
            "INSERT INTO {scratch}.claims (predicate_name, arguments, asserted_in)
             VALUES ('Statement', '{hostile}'::jsonb, gen_random_uuid())"
        ),
    )
    .await
    .expect("the old shape accepts an old row");
    ddl(
        &pool,
        format!(
            "INSERT INTO {scratch}.derived_claims (refresh_id, predicate_name, arguments)
             VALUES (gen_random_uuid(), 'Summary', '{hostile}'::jsonb)"
        ),
    )
    .await
    .expect("the old cache shape accepts an old row");

    let migration = CLAIMS_KEY_MIGRATION
        .replace("morpholog.claims", &format!("{scratch}.claims"))
        .replace(
            "morpholog_read.derived_claims",
            &format!("{scratch}.derived_claims"),
        );
    // Twice: an operator who re-runs a migration must not be punished for it.
    for _ in 0..2 {
        ddl(&pool, migration.clone())
            .await
            .expect("the migration applies, and applies again");
    }

    for table in ["claims", "derived_claims"] {
        let head_schema = if table == "claims" {
            "morpholog"
        } else {
            "morpholog_read"
        };
        assert_eq!(
            columns(&pool, scratch, table).await,
            columns(&pool, head_schema, table).await,
            "after migrating, {table} must match the head schema column for column"
        );
        assert_eq!(
            primary_key(&pool, &format!("{scratch}.{table}")).await,
            primary_key(&pool, &format!("{head_schema}.{table}")).await,
            "after migrating, {table} must carry the head schema's key"
        );
    }

    let digest_is_production: bool = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT bool_and(arguments_hash = morpholog.claim_digest(arguments))
         FROM {scratch}.claims"
    )))
    .fetch_one(&pool)
    .await
    .unwrap()
    .get(0);
    assert!(
        digest_is_production,
        "the pre-upgrade row survives, keyed by the digest production computes"
    );
    let surviving: i64 = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT count(*) FROM {scratch}.derived_claims"
    )))
    .fetch_one(&pool)
    .await
    .unwrap()
    .get(0);
    assert_eq!(surviving, 1, "the cache row survives losing its key");

    // A table in neither shape is refused, not guessed at.
    ddl(
        &pool,
        format!(
            "ALTER TABLE {scratch}.claims DROP COLUMN arguments_hash,
             ADD PRIMARY KEY (predicate_name, asserted_in)"
        ),
    )
    .await
    .unwrap();
    let refused = ddl(&pool, migration.clone()).await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| e.to_string().contains("refusing to guess")),
        "a drifted table must be refused by name, got {refused:?}"
    );

    // The head key over a column that only uses the helper with the wrong
    // expression, so a retract could never find a row. Refused.
    ddl(
        &pool,
        format!(
            "ALTER TABLE {scratch}.claims DROP CONSTRAINT claims_pkey,
             ADD COLUMN arguments_hash bytea NOT NULL GENERATED ALWAYS AS
                 (morpholog.claim_digest(arguments) || '\\x00'::bytea) STORED,
             ADD PRIMARY KEY (predicate_name, arguments_hash)"
        ),
    )
    .await
    .unwrap();
    let refused = ddl(&pool, migration.clone()).await;
    assert!(
        refused
            .as_ref()
            .is_err_and(|e| e.to_string().contains("refusing to guess")),
        "a transformed digest expression must be refused, got {refused:?}"
    );

    ddl(&pool, format!("DROP SCHEMA {scratch} CASCADE"))
        .await
        .unwrap();
}

/// Digests stored under another definition of the helper are refused,
/// whether or not the real helper has since been put back. Replacing a
/// function does not recompute stored values, so a retract could never
/// find such a row.
///
/// On its own database, because it rewrites the shared helper.
#[tokio::test]
async fn digests_stored_under_a_foreign_helper_are_refused() {
    let Ok(base) = std::env::var("DATABASE_URL") else {
        return;
    };
    let name = format!("morpholog_digest_probe_{}", std::process::id());
    let admin = morpholog_postgres::with_default_user(&with_database(&base, "postgres"));
    let admin_pool = sqlx::PgPool::connect(&admin).await.expect("maintenance db");
    ddl(
        &admin_pool,
        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
    )
    .await
    .unwrap();
    ddl(&admin_pool, format!("CREATE DATABASE {name}"))
        .await
        .unwrap();

    let probe_url = morpholog_postgres::with_default_user(&with_database(&base, &name));
    let outcome = foreign_helper_probe(&probe_url).await;

    ddl(
        &admin_pool,
        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
    )
    .await
    .unwrap();
    outcome.expect("foreign digests must be refused, and the repaired table accepted");
}

async fn foreign_helper_probe(url: &str) -> Result<(), String> {
    const REAL: &str = "CREATE OR REPLACE FUNCTION morpholog.claim_digest(args jsonb) RETURNS bytea
        LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
        RETURN sha256(convert_to(args::text, 'UTF8'))";
    const FOREIGN: &str =
        "CREATE OR REPLACE FUNCTION morpholog.claim_digest(args jsonb) RETURNS bytea
        LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE
        RETURN sha256(convert_to(args::text || 'x', 'UTF8'))";
    let pool = sqlx::PgPool::connect(url).await.expect("probe");
    morpholog_postgres::initialise_schema(&pool)
        .await
        .expect("provision");
    // A row keyed under a foreign helper, then the migration asked again.
    ddl(&pool, FOREIGN.to_string()).await.unwrap();
    ddl(
        &pool,
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         VALUES ('Statement', '[{\"type\":\"subject\",\"value\":\"s\"}]'::jsonb, gen_random_uuid())"
            .to_string(),
    )
    .await
    .unwrap();
    ddl(
        &pool,
        "DELETE FROM morpholog.schema_migrations WHERE version = 12".to_string(),
    )
    .await
    .unwrap();

    let with_foreign = morpholog_postgres::apply_migrations(&pool).await;
    match with_foreign {
        Err(e) if e.to_string().contains("another definition") => {}
        other => {
            return Err(format!(
                "a foreign helper must be refused by name, got {other:?}"
            ));
        }
    }

    // The helper put back by hand: the definition is right, the stored
    // digest is not.
    ddl(&pool, REAL.to_string()).await.unwrap();
    let with_stale = morpholog_postgres::apply_migrations(&pool).await;
    match with_stale {
        Err(e) if e.to_string().contains("disagree") => {}
        other => {
            return Err(format!(
                "a stale stored digest must be refused, got {other:?}"
            ));
        }
    }
    let status = morpholog_postgres::migration_status(&pool)
        .await
        .map_err(|e| e.to_string())?;
    if status.is_current() {
        return Err("a refused migration must not be recorded".to_string());
    }

    // The repair is a rebuild of the column, after which the row is
    // reachable and the migration accepts the table.
    ddl(
        &pool,
        "ALTER TABLE morpholog.claims DROP COLUMN arguments_hash,
         ADD COLUMN arguments_hash bytea NOT NULL GENERATED ALWAYS AS
             (morpholog.claim_digest(arguments)) STORED,
         ADD PRIMARY KEY (predicate_name, arguments_hash)"
            .to_string(),
    )
    .await
    .unwrap();
    morpholog_postgres::apply_migrations(&pool)
        .await
        .map_err(|e| format!("the repaired table must be accepted: {e}"))?;
    let reachable: i64 = sqlx::query(
        "SELECT count(*) FROM morpholog.claims
         WHERE arguments_hash = morpholog.claim_digest(arguments)",
    )
    .fetch_one(&pool)
    .await
    .unwrap()
    .get(0);
    if reachable != 1 {
        return Err(format!(
            "the repaired row must be keyed by the real digest, got {reachable}"
        ));
    }
    Ok(())
}

#[tokio::test]
async fn a_witnesses_column_of_another_shape_is_refused() {
    let Ok(base) = std::env::var("DATABASE_URL") else {
        return;
    };
    let name = format!("morpholog_witness_shape_probe_{}", std::process::id());
    let admin = morpholog_postgres::with_default_user(&with_database(&base, "postgres"));
    let admin_pool = sqlx::PgPool::connect(&admin).await.expect("maintenance db");
    ddl(
        &admin_pool,
        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
    )
    .await
    .unwrap();
    ddl(&admin_pool, format!("CREATE DATABASE {name}"))
        .await
        .unwrap();

    let probe_url = morpholog_postgres::with_default_user(&with_database(&base, &name));
    let outcome = witness_shape_probe(&probe_url).await;

    ddl(
        &admin_pool,
        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
    )
    .await
    .unwrap();
    outcome.expect("a foreign witnesses column must be refused, and the repaired one accepted");
}

async fn witness_shape_probe(url: &str) -> Result<(), String> {
    let pool = sqlx::PgPool::connect(url).await.expect("probe");
    morpholog_postgres::initialise_schema(&pool)
        .await
        .expect("provision");
    // Someone's own `witnesses` column, then the migration asked again.
    ddl(
        &pool,
        "ALTER TABLE morpholog.audit_checkpoints DROP COLUMN witnesses".to_string(),
    )
    .await
    .unwrap();
    ddl(
        &pool,
        "ALTER TABLE morpholog.audit_checkpoints ADD COLUMN witnesses text".to_string(),
    )
    .await
    .unwrap();
    ddl(
        &pool,
        "DELETE FROM morpholog.schema_migrations WHERE version = 13".to_string(),
    )
    .await
    .unwrap();
    match morpholog_postgres::apply_migrations(&pool).await {
        Err(e) if e.to_string().contains("another shape") => {}
        other => {
            return Err(format!(
                "a witnesses column of another shape must be refused by name, got {other:?}"
            ));
        }
    }
    // The foreign column removed: the migration adds the real one.
    ddl(
        &pool,
        "ALTER TABLE morpholog.audit_checkpoints DROP COLUMN witnesses".to_string(),
    )
    .await
    .unwrap();
    morpholog_postgres::apply_migrations(&pool)
        .await
        .map_err(|e| format!("the repaired table must migrate: {e}"))?;
    let shape: Option<String> = sqlx::query_scalar(
        "SELECT format_type(atttypid, atttypmod) FROM pg_attribute
         WHERE attrelid = 'morpholog.audit_checkpoints'::regclass AND attname = 'witnesses'",
    )
    .fetch_optional(&pool)
    .await
    .unwrap();
    if shape.as_deref() != Some("jsonb") {
        return Err(format!("the real column must be jsonb, got {shape:?}"));
    }
    Ok(())
}
