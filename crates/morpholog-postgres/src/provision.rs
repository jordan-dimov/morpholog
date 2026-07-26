//! Day-zero provisioning: the embedded schema, and the opt-in
//! least-privilege floor that makes "the governed path is the only
//! door" true on a fresh database by default rather than by operator
//! convention.

use crate::error::{PgError, classify};
use crate::sql_quote::quote_ident;
use sqlx::PgPool;
use std::fmt::Write as _;

/// The canonical Morpholog schema, compiled into this crate. The same
/// file a repo checkout applies with `psql -f`; embedding it means a
/// binary-only deployment provisions exactly the schema this build
/// expects - nothing to vendor, nothing to drift.
pub const SCHEMA_SQL: &str = include_str!("../../morpholog-core/sql/schema.sql");

/// Outcome of [`initialise_schema`]: provisioned now, or found already
/// provisioned (the caller decides whether that is fine or an error).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitOutcome {
    Initialised,
    AlreadyInitialised,
}

/// Provision the `morpholog` schema in an existing database from the
/// embedded `SCHEMA_SQL`. Day-zero only: if the schema already
/// exists this returns [`InitOutcome::AlreadyInitialised`] without
/// touching anything - it never drops and never migrates. Schema
/// *evolution* is the deferred migrations story, not this function.
///
/// Provisioning is atomic: the existence check and the whole schema
/// script run in one transaction (the script is plain DDL, which
/// PostgreSQL rolls back like any other statement), so a mid-script
/// failure leaves nothing behind - in particular, no partial schema
/// the existence guard would later misread as already-initialised.
pub async fn initialise_schema(pool: &PgPool) -> Result<InitOutcome, PgError> {
    let mut tx = pool.begin().await.map_err(classify)?;
    let exists = sqlx::query!("SELECT 1 AS one FROM pg_namespace WHERE nspname = 'morpholog'")
        .fetch_optional(&mut *tx)
        .await
        .map_err(classify)?;
    if exists.is_some() {
        return Ok(InitOutcome::AlreadyInitialised);
    }
    sqlx::raw_sql(SCHEMA_SQL)
        .execute(&mut *tx)
        .await
        .map_err(classify)?;
    tx.commit().await.map_err(classify)?;
    Ok(InitOutcome::Initialised)
}

/// Drop the `morpholog` schema and everything in it, for a
/// development database that wants re-provisioning from scratch.
///
/// Destructive by definition and deliberately dumb: the caller owns
/// the acknowledgement (see the CLI's `init --reset`), because a
/// library function cannot tell a scratch database from production.
/// Returns whether a schema was there to drop, so a caller can report
/// honestly rather than implying it removed something.
///
/// One transaction with the re-provisioning it precedes is NOT
/// possible here - the caller runs [`initialise_schema`] next, and a
/// failure between the two leaves an un-provisioned database, which is
/// the same state `init` starts from and recovers by re-running.
pub async fn drop_schema(pool: &PgPool) -> Result<bool, PgError> {
    let existed = sqlx::query!("SELECT 1 AS one FROM pg_namespace WHERE nspname = 'morpholog'")
        .fetch_optional(pool)
        .await
        .map_err(classify)?
        .is_some();
    sqlx::raw_sql("DROP SCHEMA IF EXISTS morpholog CASCADE")
        .execute(pool)
        .await
        .map_err(classify)?;
    Ok(existed)
}

/// The group role holding exactly the runtime's write set. NOLOGIN and
/// passwordless: the operator grants membership to the runtime's real
/// login role.
pub const WRITER_ROLE: &str = "morpholog_writer";

/// The group role holding read-only access to the governed tables and
/// the derived read cache, for dashboards, projections, and auditors.
pub const READER_ROLE: &str = "morpholog_reader";

/// Provision the least-privilege floor: mint the [`WRITER_ROLE`] and
/// [`READER_ROLE`] group roles (kept if they already exist - roles are
/// cluster-global), revoke PUBLIC from the governed schemas and tables,
/// and grant each role exactly what it needs. Idempotent; one
/// transaction, so a failure leaves nothing half-provisioned.
///
/// The writer's grants are the runtime's write set and nothing more.
/// In particular `morpholog.audit` gets INSERT and SELECT only: the
/// audit log is append-only at the database layer even for the
/// runtime's own role.
///
/// Membership grants (and `pg_read_all_stats` for an audit-tailing
/// reader) are deliberately left to the operator - printed by the CLI,
/// never applied here - so no secret or cluster-wide policy decision
/// hides inside provisioning.
pub async fn provision_least_privilege(pool: &PgPool) -> Result<(), PgError> {
    let mut tx = pool.begin().await.map_err(classify)?;
    for role in [WRITER_ROLE, READER_ROLE] {
        let exists = sqlx::query!("SELECT 1 AS one FROM pg_roles WHERE rolname = $1", role)
            .fetch_optional(&mut *tx)
            .await
            .map_err(classify)?;
        if exists.is_none() {
            sqlx::raw_sql(&format!("CREATE ROLE {} NOLOGIN", quote_ident(role)))
                .execute(&mut *tx)
                .await
                .map_err(provision_error)?;
        }
    }
    sqlx::raw_sql(&least_privilege_sql(WRITER_ROLE, READER_ROLE))
        .execute(&mut *tx)
        .await
        .map_err(provision_error)?;
    tx.commit().await.map_err(classify)?;
    Ok(())
}

/// Name the remedy when the connection role cannot provision, because a
/// bare permission error would leave the operator guessing. Two distinct
/// privileges are in play: CREATE ROLE needs CREATEROLE, and the
/// REVOKE/GRANT statements need ownership of the governed tables - in
/// practice the role that ran `morpholog init`. A superuser has both.
fn provision_error(e: sqlx::Error) -> PgError {
    if let sqlx::Error::Database(db) = &e
        && db.code().as_deref() == Some("42501")
    {
        return PgError::InvalidState(format!(
            "least-privilege provisioning was refused: {}; connect as a \
             superuser, or as a role that has CREATEROLE and owns the \
             morpholog tables (normally the role that ran `morpholog init`), \
             and re-run",
            db.message()
        ));
    }
    classify(e)
}

/// The REVOKE/GRANT script, pure and deterministic so a test pins the
/// exact privilege floor. The grants are the write-set census of the
/// runtime's SQL, table by table.
fn least_privilege_sql(writer: &str, reader: &str) -> String {
    let w = quote_ident(writer);
    let r = quote_ident(reader);
    let mut out = String::new();
    let _ = writeln!(out, "REVOKE ALL ON SCHEMA morpholog FROM PUBLIC;");
    let _ = writeln!(out, "REVOKE ALL ON SCHEMA morpholog_read FROM PUBLIC;");
    let _ = writeln!(
        out,
        "REVOKE ALL ON ALL TABLES IN SCHEMA morpholog FROM PUBLIC;"
    );
    let _ = writeln!(
        out,
        "REVOKE ALL ON ALL TABLES IN SCHEMA morpholog_read FROM PUBLIC;"
    );
    let _ = writeln!(out, "GRANT USAGE ON SCHEMA morpholog TO {w}, {r};");
    let _ = writeln!(out, "GRANT USAGE ON SCHEMA morpholog_read TO {w}, {r};");
    // The write set: assert is INSERT, retract is DELETE (claims);
    // audit is append-only; the outbox lease and checkpoint co-sign
    // paths UPDATE; refresh derived owns the read cache.
    let _ = writeln!(
        out,
        "GRANT SELECT, INSERT, DELETE ON morpholog.claims TO {w};"
    );
    let _ = writeln!(out, "GRANT SELECT, INSERT ON morpholog.audit TO {w};");
    let _ = writeln!(out, "GRANT SELECT, INSERT ON morpholog.rejections TO {w};");
    let _ = writeln!(
        out,
        "GRANT SELECT, INSERT, UPDATE ON morpholog.outbox TO {w};"
    );
    let _ = writeln!(
        out,
        "GRANT SELECT, INSERT, UPDATE ON morpholog.audit_checkpoints TO {w};"
    );
    let _ = writeln!(
        out,
        "GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA morpholog_read TO {w};"
    );
    let _ = writeln!(
        out,
        "GRANT SELECT ON ALL TABLES IN SCHEMA morpholog TO {r};"
    );
    let _ = writeln!(
        out,
        "GRANT SELECT ON ALL TABLES IN SCHEMA morpholog_read TO {r};"
    );
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_privilege_floor_is_pinned() {
        let sql = least_privilege_sql(WRITER_ROLE, READER_ROLE);
        // The floor's teeth: PUBLIC is revoked, and the writer gets no
        // UPDATE or DELETE on the append-only audit log.
        assert!(sql.contains("REVOKE ALL ON SCHEMA morpholog FROM PUBLIC"));
        assert!(sql.contains("GRANT SELECT, INSERT ON morpholog.audit TO \"morpholog_writer\""));
        assert!(!sql.contains("UPDATE ON morpholog.audit "));
        assert!(!sql.contains("DELETE ON morpholog.audit "));
        assert!(!sql.contains("TRUNCATE"));
    }

    #[test]
    fn role_names_are_quoted_identifiers() {
        let sql = least_privilege_sql("odd\"name", "reader");
        assert!(sql.contains("\"odd\"\"name\""));
    }
}
