//! Schema evolution: the numbered migrations, compiled into the binary.
//!
//! Embedded like `SCHEMA_SQL`, so a binary-only deployment carries exactly
//! the migrations it expects, with nothing to vendor or drift.
//!
//! **What "pending" means.** `morpholog.schema_migrations` records applied
//! versions. A database provisioned from `schema.sql` is at the head, so
//! [`crate::initialise_schema`] records every migration without running
//! any. A database predating that table has no record, so everything is
//! pending. That is sound because the migrations are idempotent.

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
    (12, "claims_hash_key", "012_claims_hash_key.sql"),
    (13, "checkpoint_witnesses", "013_checkpoint_witnesses.sql"),
    (14, "audit_parameters", "014_audit_parameters.sql"),
    (15, "managed_indexes", "015_managed_indexes.sql"),
    (16, "timestamp_nanos", "016_timestamp_nanos.sql"),
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
    /// The newest version the database recorded before this run, or `None`
    /// when it predates the record.
    ///
    /// `None`, not `0`: such a database may well have migrations applied;
    /// only the record is missing.
    pub recorded_version_before: Option<i32>,
    /// The newest version recorded after this run. Unchanged by `--check`.
    pub recorded_version_after: Option<i32>,
    /// The newest migration this binary carries.
    pub binary_version: i32,
    /// Applied by this run, in order. Empty when checking, and empty when
    /// the database was already current.
    pub applied: Vec<MigrationRef>,
    /// Still outstanding. Empty after a successful run; populated when
    /// checking a database that is behind.
    pub pending: Vec<MigrationRef>,
    /// Recorded by the database and unknown to this binary: the database
    /// is AHEAD, as after a rollback to an older binary.
    ///
    /// Refused, not ignored: the binary cannot know whether an unseen
    /// migration changed something it depends on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unknown: Vec<MigrationRef>,
}

impl MigrationReport {
    /// Nothing outstanding and nothing unrecognised. What a deploy gate asks.
    pub fn is_current(&self) -> bool {
        self.pending.is_empty() && self.unknown.is_empty()
    }
}

/// Which versions the database records as applied.
///
/// `None` when the record table itself does not exist, which the caller
/// treats as "everything is pending".
async fn recorded_versions(pool: &PgPool) -> Result<Option<Vec<MigrationRef>>, PgError> {
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
    let rows =
        sqlx::query!("SELECT version, name FROM morpholog.schema_migrations ORDER BY version")
            .fetch_all(pool)
            .await
            .map_err(classify_checked_query)?;
    Ok(Some(
        rows.into_iter()
            .map(|r| MigrationRef {
                version: r.version,
                name: r.name,
            })
            .collect(),
    ))
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
    let rows = recorded.clone().unwrap_or_default();
    let pending: Vec<MigrationRef> = MIGRATIONS
        .iter()
        .filter(|m| !rows.iter().any(|r| r.version == m.version))
        .map(|m| MigrationRef {
            version: m.version,
            name: m.name.to_string(),
        })
        .collect();
    // Recorded here, unknown to this build: the database is ahead.
    let unknown: Vec<MigrationRef> = rows
        .iter()
        .filter(|r| !MIGRATIONS.iter().any(|m| m.version == r.version))
        .cloned()
        .collect();
    let newest = recorded
        .as_ref()
        .and_then(|rows| rows.iter().map(|r| r.version).max());
    Ok(MigrationReport {
        recorded_version_before: newest,
        recorded_version_after: newest,
        binary_version: head_version(),
        applied: Vec::new(),
        pending,
        unknown,
    })
}

/// Apply every migration the database has not recorded, in order.
///
/// Each runs in its own transaction with its record, so a failure part-way
/// leaves the earlier versions applied and recorded, never a half-migrated
/// database claiming to be current.
pub async fn apply_migrations(pool: &PgPool) -> Result<MigrationReport, PgError> {
    ensure_schema_present(pool).await?;
    let before = migration_status(pool).await?;
    if !before.unknown.is_empty() {
        // Migrating a database that is ahead would apply nothing and report
        // success, although an unseen migration may have broken this binary.
        let names: Vec<String> = before
            .unknown
            .iter()
            .map(|m| format!("{} ({})", m.version, m.name))
            .collect();
        return Err(PgError::InvalidState(format!(
            "this database records migrations this binary does not know: {}. \
             It was migrated by a newer Morpholog, so this build cannot tell \
             whether its schema is still compatible - upgrade the binary rather \
             than migrating the database.",
            names.join(", ")
        )));
    }
    // The record table must exist before the first migration records
    // itself, though a later migration introduces it. This matches what that
    // migration creates, which stays for anyone applying files by hand.
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
    // Grants are per table and do not cover tables created later, so
    // re-apply the least-privilege floor (idempotent) after migrating.
    if !applied.is_empty() && crate::least_privilege_roles_exist(pool).await? {
        crate::provision_least_privilege(pool).await?;
    }
    let after = migration_status(pool).await?;
    Ok(MigrationReport {
        recorded_version_before: before.recorded_version_before,
        recorded_version_after: after.recorded_version_after,
        binary_version: head_version(),
        applied,
        pending: after.pending,
        unknown: after.unknown,
    })
}
