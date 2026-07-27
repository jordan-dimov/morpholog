//! Schema evolution: the numbered migrations, compiled into the binary.
//!
//! `init` provisions a database and never migrates it, which left upgrading
//! as an instruction rather than a capability - "apply every numbered file
//! that postdates your database", from a directory the release artifact does
//! not contain. An embedder consuming releases had to fetch the SQL out of a
//! git tag. Embedding them here is the same move `SCHEMA_SQL` already makes:
//! a binary-only deployment carries exactly the migrations this build
//! expects, with nothing to vendor and nothing to drift.
//!
//! **What "pending" means.** `morpholog.schema_migrations` records applied
//! versions. A database provisioned from `schema.sql` is at the head by
//! construction, so [`crate::initialise_schema`] records every migration
//! without running any. A database predating that table has no record, so
//! everything is pending - which is sound because the migrations are
//! idempotent, and migration 011 backfills the record once it lands.

use crate::error::{PgError, classify, classify_checked_query};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// One numbered migration, its version, and the SQL that applies it.
pub(crate) struct Migration {
    pub(crate) version: i32,
    pub(crate) name: &'static str,
    pub(crate) sql: &'static str,
}

macro_rules! migrations {
    ($(($version:expr, $name:literal, $file:literal)),* $(,)?) => {
        /// Every migration this build knows, in order.
        pub(crate) const MIGRATIONS: &[Migration] = &[
            $(Migration {
                version: $version,
                name: $name,
                sql: include_str!(concat!("../../morpholog-core/sql/migrations/", $file)),
            }),*
        ];
    };
}

migrations![
    (1, "outbox_delivery_state", "001_outbox_delivery_state.sql"),
    (
        2,
        "compensation_in_progress",
        "002_compensation_in_progress.sql"
    ),
    (
        3,
        "outbox_intent_type_next_attempt_index",
        "003_outbox_intent_type_next_attempt_index.sql"
    ),
    (4, "audit_actor", "004_audit_actor.sql"),
    (5, "rejections", "005_rejections.sql"),
    (6, "audit_keyset_index", "006_audit_keyset_index.sql"),
    (7, "derived_read_cache", "007_derived_read_cache.sql"),
    (8, "checkpoint_signatures", "008_checkpoint_signatures.sql"),
    (9, "audit_attestation", "009_audit_attestation.sql"),
    (10, "rejections_witness", "010_rejections_witness.sql"),
    (11, "schema_migrations", "011_schema_migrations.sql"),
];

/// The newest migration this binary carries.
pub fn head_version() -> i32 {
    MIGRATIONS.last().map_or(0, |m| m.version)
}

/// One migration, as reported.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationRef {
    pub version: i32,
    pub name: String,
}

/// What `migrate` found and did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationReport {
    /// The version the database was at before this run.
    pub database_version: i32,
    /// The newest migration this binary carries.
    pub binary_version: i32,
    /// Applied by this run, in order. Empty when checking, and empty when
    /// the database was already current.
    pub applied: Vec<MigrationRef>,
    /// Still outstanding. Empty after a successful run; populated when
    /// checking a database that is behind.
    pub pending: Vec<MigrationRef>,
}

/// Which versions the database records as applied.
///
/// `None` when the record itself does not exist - a database older than the
/// migration that introduces it. That is different from "recorded nothing",
/// and the caller treats it as "everything is pending".
async fn recorded_versions(pool: &PgPool) -> Result<Option<Vec<i32>>, PgError> {
    let present = sqlx::query!(
        "SELECT 1 AS one FROM pg_tables
         WHERE schemaname = 'morpholog' AND tablename = 'schema_migrations'"
    )
    .fetch_optional(pool)
    .await
    .map_err(classify_checked_query)?;
    if present.is_none() {
        return Ok(None);
    }
    let rows = sqlx::query!("SELECT version FROM morpholog.schema_migrations ORDER BY version")
        .fetch_all(pool)
        .await
        .map_err(classify_checked_query)?;
    Ok(Some(rows.into_iter().map(|r| r.version).collect()))
}

/// Record every migration this build carries as applied, without running
/// them. For a database provisioned from `schema.sql`, which is at the head
/// by construction.
pub(crate) async fn record_all_applied(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<(), PgError> {
    for m in MIGRATIONS {
        sqlx::query!(
            "INSERT INTO morpholog.schema_migrations (version, name)
             VALUES ($1, $2) ON CONFLICT (version) DO NOTHING",
            m.version,
            m.name,
        )
        .execute(&mut **tx)
        .await
        .map_err(classify_checked_query)?;
    }
    Ok(())
}

/// Refuse a database that has never been provisioned, rather than running
/// migrations against nothing and reporting success.
async fn ensure_schema_present(pool: &PgPool) -> Result<(), PgError> {
    let exists = sqlx::query!("SELECT 1 AS one FROM pg_namespace WHERE nspname = 'morpholog'")
        .fetch_optional(pool)
        .await
        .map_err(classify_checked_query)?;
    if exists.is_none() {
        return Err(PgError::InvalidState(
            "this database has no `morpholog` schema, so there is nothing to migrate. \
             Run `morpholog init` to provision it - a fresh database is created at the \
             head and needs no migrations."
                .to_string(),
        ));
    }
    Ok(())
}

/// Report the database's migration state, changing nothing.
pub async fn migration_status(pool: &PgPool) -> Result<MigrationReport, PgError> {
    ensure_schema_present(pool).await?;
    let recorded = recorded_versions(pool).await?;
    let applied_versions = recorded.clone().unwrap_or_default();
    let pending: Vec<MigrationRef> = MIGRATIONS
        .iter()
        .filter(|m| !applied_versions.contains(&m.version))
        .map(|m| MigrationRef {
            version: m.version,
            name: m.name.to_string(),
        })
        .collect();
    Ok(MigrationReport {
        database_version: applied_versions.iter().copied().max().unwrap_or(0),
        binary_version: head_version(),
        applied: Vec::new(),
        pending,
    })
}

/// Apply every migration the database has not recorded, in order.
///
/// Each runs in its own transaction with its record written alongside, so a
/// failure part-way leaves the versions before it applied and recorded
/// rather than a half-migrated database claiming to be current.
pub async fn apply_migrations(pool: &PgPool) -> Result<MigrationReport, PgError> {
    ensure_schema_present(pool).await?;
    let before = migration_status(pool).await?;
    // The record has to exist before the first migration can record itself:
    // the migration that introduces it is number 11, and 001 would otherwise
    // insert into a table ten steps in its future. Idempotent, and it
    // matches what 011 creates - which stays, for anyone applying the files
    // by hand with psql.
    sqlx::raw_sql(
        "CREATE TABLE IF NOT EXISTS morpholog.schema_migrations (
             version     integer      PRIMARY KEY,
             name        text         NOT NULL,
             applied_at  timestamptz  NOT NULL DEFAULT now()
         )",
    )
    .execute(pool)
    .await
    .map_err(classify)?;
    let mut applied = Vec::new();
    for m in MIGRATIONS {
        if !before.pending.iter().any(|p| p.version == m.version) {
            continue;
        }
        let mut tx = pool.begin().await.map_err(classify)?;
        sqlx::raw_sql(m.sql)
            .execute(&mut *tx)
            .await
            .map_err(classify)?;
        sqlx::query!(
            "INSERT INTO morpholog.schema_migrations (version, name)
             VALUES ($1, $2) ON CONFLICT (version) DO NOTHING",
            m.version,
            m.name,
        )
        .execute(&mut *tx)
        .await
        .map_err(classify_checked_query)?;
        tx.commit().await.map_err(classify)?;
        applied.push(MigrationRef {
            version: m.version,
            name: m.name.to_string(),
        });
    }
    Ok(MigrationReport {
        database_version: before.database_version,
        binary_version: head_version(),
        applied,
        pending: Vec::new(),
    })
}
