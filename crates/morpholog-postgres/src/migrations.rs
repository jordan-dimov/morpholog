//! Schema evolution: the numbered migrations, compiled into the binary.
//!
//! The baseline is the schema v0.0.14 provisioned, migration 23. A
//! database that does not record it was made by an older release and is
//! brought to the baseline with a v0.0.14 binary first; this binary
//! carries only the migrations beyond it. Embedded like `SCHEMA_SQL`, so
//! a binary-only deployment carries exactly the migrations it expects.
//!
//! **What "pending" means.** `morpholog.schema_migrations` records applied
//! versions. A database provisioned from `schema.sql` is at the head, and
//! the file records the ledger up to the baseline, so none is pending.

use crate::error::{PgError, classify, classify_checked_query};
use crate::preparation::{
    ActivationPreflight, LockPolicy, activation_preflight, clear_progress, prepare_canonical_order,
    take_migration_lock,
};
use serde::{Deserialize, Serialize};
use sqlx::{Connection as _, PgPool};

/// The migration every database must record: v0.0.14's head.
pub const BASELINE_VERSION: i32 = 23;

/// The release whose `migrate` brings an older database to the baseline.
pub(crate) const BASELINE_RELEASE: &str = "0.0.14";

/// Every migration this build carries beyond the baseline, in order, as
/// `(version, name, sql)`; the SQL is `include_str!` of a file under
/// `crates/morpholog-postgres/sql/migrations/`.
pub(crate) const MIGRATIONS: &[(i32, &str, &str)] = &[
    (
        24,
        "gate_witness",
        include_str!("../sql/migrations/024_gate_witness.sql"),
    ),
    (
        25,
        "rejection_compared",
        include_str!("../sql/migrations/025_rejection_compared.sql"),
    ),
    (
        26,
        "decimal_arithmetic",
        include_str!("../sql/migrations/026_decimal_arithmetic.sql"),
    ),
    (
        27,
        "canonical_order",
        include_str!("../sql/migrations/027_canonical_order.sql"),
    ),
];

/// Whether `migrate` stops before the one transaction that pauses writers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrateMode {
    /// Apply everything, the cutover included.
    Complete,
    /// Apply the plain migrations and every preparation step of the
    /// staged one, then stop with it still pending: `migrate --prepare`.
    Prepare,
}

/// The staged migration: prepared in steps outside any long lock
/// (`preparation`), then one transaction that runs its script, writes the
/// activation record (the legacy root comes from the Rust leaf encoder)
/// and records it.
const ACTIVATION_VERSION: i32 = crate::preparation::CANONICAL_ORDER_VERSION;

/// The newest migration this binary carries.
pub fn head_version() -> i32 {
    MIGRATIONS.last().map_or(BASELINE_VERSION, |m| m.0)
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
    /// The newest version the database recorded before this run.
    pub recorded_version_before: i32,
    /// The newest version recorded after this run. Unchanged by `--check`.
    pub recorded_version_after: i32,
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
    /// Present only when the database is older than the baseline. Nothing
    /// is pending then, since this binary can apply nothing to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub below_baseline: Option<BelowBaseline>,
    /// Present while the staged migration is pending: where its
    /// preparation stands, what its cutover would cost, and what refuses
    /// it. Absent once the database is current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation_preflight: Option<ActivationPreflight>,
}

/// A database an older release made, which a binary of release
/// `upgrade_with` must migrate before this one can.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BelowBaseline {
    pub baseline: i32,
    /// The newest migration the database records; absent when it records
    /// none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded: Option<i32>,
    pub upgrade_with: String,
}

impl MigrationReport {
    /// What `migrate --check` reports for [`PgError::SchemaBelowBaseline`].
    pub fn below_baseline(recorded: Option<i32>) -> Self {
        Self {
            recorded_version_before: recorded.unwrap_or(0),
            recorded_version_after: recorded.unwrap_or(0),
            binary_version: head_version(),
            applied: Vec::new(),
            pending: Vec::new(),
            unknown: Vec::new(),
            activation_preflight: None,
            below_baseline: Some(BelowBaseline {
                baseline: BASELINE_VERSION,
                recorded,
                upgrade_with: BASELINE_RELEASE.to_string(),
            }),
        }
    }

    /// Nothing outstanding, nothing unrecognised, and not below the
    /// baseline. What a deploy gate asks.
    pub fn is_current(&self) -> bool {
        self.pending.is_empty() && self.unknown.is_empty() && self.below_baseline.is_none()
    }
}

/// Which versions the database records as applied, or `None` when the
/// record table itself does not exist.
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

fn refuse_if_ahead(status: &MigrationReport) -> Result<(), PgError> {
    match status.unknown.iter().map(|m| m.version).max() {
        Some(recorded) => Err(PgError::SchemaAhead {
            recorded,
            binary: head_version(),
        }),
        None => Ok(()),
    }
}

/// Refuse a database this binary cannot serve, before its first query:
/// one with no `morpholog` schema, one below the baseline, one ahead of
/// this binary, or one behind it, asked in that order, so an older binary
/// never advises a migration against a database it does not understand.
/// What every database-backed command asks once after connecting, except
/// the two that make a database current, `init` and `migrate`.
pub async fn require_current_schema(pool: &PgPool) -> Result<(), PgError> {
    let status = migration_status(pool).await?;
    refuse_if_ahead(&status)?;
    if let Some(first) = status.pending.first() {
        return Err(PgError::SchemaBehind {
            detail: format!(
                "{} migration(s) pending, from {} ({})",
                status.pending.len(),
                first.version,
                first.name
            ),
        });
    }
    Ok(())
}

/// Report the database's migration state, changing nothing. A database
/// that does not record the baseline is refused: nothing this binary
/// carries can bring it forward.
pub async fn migration_status(pool: &PgPool) -> Result<MigrationReport, PgError> {
    ensure_schema_present(pool).await?;
    let rows = recorded_versions(pool).await?.unwrap_or_default();
    if !rows.iter().any(|r| r.version == BASELINE_VERSION) {
        let recorded = rows.iter().map(|r| r.version).max();
        // No release records a later migration without the baseline, so
        // no release's `migrate` is the remedy.
        if let Some(newest) = recorded.filter(|v| *v > BASELINE_VERSION) {
            return Err(PgError::InvalidState(format!(
                "this database records migration {newest} but not migration \
                 {BASELINE_VERSION}, which every release since v{BASELINE_RELEASE} records; \
                 its migration record was changed outside `morpholog migrate`, and no \
                 release can be trusted to migrate it"
            )));
        }
        return Err(PgError::SchemaBelowBaseline {
            recorded,
            baseline: BASELINE_VERSION,
        });
    }
    let pending: Vec<MigrationRef> = MIGRATIONS
        .iter()
        .filter(|m| !rows.iter().any(|r| r.version == m.0))
        .map(|m| MigrationRef {
            version: m.0,
            name: m.1.to_string(),
        })
        .collect();
    // Recorded here, beyond the baseline and unknown to this build: the
    // database is ahead. Versions below the baseline are its history.
    let unknown: Vec<MigrationRef> = rows
        .iter()
        .filter(|r| r.version > BASELINE_VERSION && !MIGRATIONS.iter().any(|m| m.0 == r.version))
        .cloned()
        .collect();
    let newest = rows
        .iter()
        .map(|r| r.version)
        .max()
        .unwrap_or(BASELINE_VERSION);
    Ok(MigrationReport {
        recorded_version_before: newest,
        recorded_version_after: newest,
        binary_version: head_version(),
        applied: Vec::new(),
        pending,
        unknown,
        below_baseline: None,
        activation_preflight: None,
    })
}

/// Apply every migration the database has not recorded, in order, then
/// re-apply the privilege floor to the roles the database records.
///
/// Each plain migration runs in its own transaction with its record, so a
/// failure part-way leaves the earlier versions applied and recorded,
/// never a half-migrated database claiming to be current. The staged
/// migration prepares first, in idempotent steps outside any long lock,
/// then runs its own transaction the same way.
pub async fn apply_migrations(pool: &PgPool) -> Result<MigrationReport, PgError> {
    apply_migrations_with(pool, MigrateMode::Complete, &LockPolicy::default()).await
}

pub async fn apply_migrations_with(
    pool: &PgPool,
    mode: MigrateMode,
    policy: &LockPolicy,
) -> Result<MigrationReport, PgError> {
    let before = migration_status(pool).await?;
    // Migrating a database that is ahead would apply nothing and report
    // success, although an unseen migration may have broken this binary.
    refuse_if_ahead(&before)?;
    crate::require_deployment_roles(pool).await?;
    // One runner at a time: a second would race the first over the
    // prepared frontier and the progress rows.
    let lock = match before.pending.is_empty() {
        true => None,
        false => Some(take_migration_lock(pool).await?),
    };
    let mut applied = Vec::new();
    let mut preflight = None;
    for &(version, name, sql) in MIGRATIONS {
        if !before.pending.iter().any(|p| p.version == version) {
            continue;
        }
        if version == ACTIVATION_VERSION {
            prepare_canonical_order(pool, policy).await?;
            let checked = activation_preflight(pool).await?;
            if mode == MigrateMode::Prepare {
                preflight = Some(checked);
                break;
            }
            if !checked.refusals.is_empty() {
                return Err(PgError::CutoverRefused {
                    reasons: checked.refusals,
                });
            }
        }
        let mut tx = pool.begin().await.map_err(classify)?;
        sqlx::raw_sql(sql)
            .execute(&mut *tx)
            .await
            .map_err(classify)?;
        if version == ACTIVATION_VERSION {
            crate::activation::write_activation_record(&mut tx).await?;
            clear_progress(&mut tx).await?;
        }
        sqlx::query!(
            "INSERT INTO morpholog.schema_migrations (version, name)
             VALUES ($1, $2) ON CONFLICT (version) DO NOTHING",
            version,
            name,
        )
        .execute(&mut *tx)
        .await
        .map_err(classify_checked_query)?;
        tx.commit().await.map_err(classify)?;
        applied.push(MigrationRef {
            version,
            name: name.to_string(),
        });
    }
    if let Some(conn) = lock {
        conn.close().await.map_err(classify)?;
    }
    // Grants are per table and do not cover tables created later, so the
    // floor is re-applied on every run (it is idempotent), not only after
    // a migration: a restore or a hand change can have narrowed it too.
    crate::reapply_least_privilege(pool).await?;
    let after = migration_status(pool).await?;
    Ok(MigrationReport {
        recorded_version_before: before.recorded_version_before,
        recorded_version_after: after.recorded_version_after,
        binary_version: head_version(),
        applied,
        pending: after.pending,
        unknown: after.unknown,
        below_baseline: None,
        activation_preflight: preflight,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lines of `sql` that control a transaction. A function body's
    /// `END;` closes a block, not a transaction, so bodies are skipped.
    fn transaction_statements(sql: &str) -> Vec<usize> {
        let mut in_body = false;
        sql.lines()
            .enumerate()
            .filter_map(|(n, line)| {
                let code = line.split("--").next().unwrap_or("");
                let was_in_body = in_body;
                in_body ^= code.matches("$$").count() % 2 == 1;
                let bare = code.trim().to_ascii_uppercase();
                (!was_in_body
                    && matches!(bare.as_str(), "BEGIN;" | "COMMIT;" | "ROLLBACK;" | "END;"))
                .then_some(n + 1)
            })
            .collect()
    }

    /// The runner owns each migration's transaction, and a COMMIT inside
    /// one would separate the schema change from its version record.
    #[test]
    fn no_migration_controls_its_own_transaction() {
        let offenders: Vec<String> = MIGRATIONS
            .iter()
            .flat_map(|&(_, name, sql)| {
                transaction_statements(sql)
                    .into_iter()
                    .map(move |line| format!("{name}:{line}"))
            })
            .collect();
        assert!(offenders.is_empty(), "{}", offenders.join(", "));
    }

    #[test]
    fn a_function_bodys_end_is_not_transaction_control() {
        let sql = "CREATE FUNCTION f() RETURNS void AS $$\nBEGIN\nEND;\n$$ LANGUAGE plpgsql;\nCOMMIT;\nEND;\n";
        assert_eq!(transaction_statements(sql), vec![5, 6]);
    }

    #[test]
    fn migrations_are_numbered_from_the_baseline_in_order() {
        let mut expected = BASELINE_VERSION;
        for &(version, _, _) in MIGRATIONS {
            expected += 1;
            assert_eq!(version, expected);
        }
        assert_eq!(head_version(), expected);
    }
}
