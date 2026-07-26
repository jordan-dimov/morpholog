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

/// A connection string with a username filled in when it specifies
/// none, restoring the behaviour `libpq`, `psql`, and sqlx 0.8 all
/// share: an unspecified user means the operating-system user. sqlx
/// 0.9 stopped doing this and connects as `anonymous` instead, which
/// turns the `postgres:///mydb` form every Postgres user reaches for -
/// and that this project's own install guide teaches - into a
/// peer-authentication failure. Precedence follows libpq: `PGUSER`,
/// then the OS user.
///
/// Applied at the connection door rather than pushed onto users as a
/// documentation change, because the short form is not a Morpholog
/// convention to revise - it is what Postgres tooling means.
pub fn with_default_user(url: &str) -> String {
    // Precedence follows libpq: PGUSER, then the OS user.
    let user = std::env::var("PGUSER")
        .ok()
        .or_else(|| std::env::var("USER").ok())
        .or_else(|| std::env::var("LOGNAME").ok())
        .filter(|u| !u.is_empty());
    apply_default_user(url, user.as_deref())
}

/// The rule itself, with the environment lookup lifted out: the
/// workspace forbids `unsafe`, so a test cannot mutate the environment -
/// and the substitution is the part worth pinning regardless.
fn apply_default_user(url: &str, user: Option<&str>) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    // Userinfo present, or a `user=` parameter: the caller has spoken.
    if rest[..authority_end].contains('@') || url.contains("user=") {
        return url.to_string();
    }
    // Nothing to fill in with: let the driver report its own error
    // rather than invent a username.
    let Some(user) = user else {
        return url.to_string();
    };
    if authority_end == 0 {
        // The hostless socket form. Injecting `user@` here would read as
        // an empty host and be rejected, so the username goes in as the
        // query parameter instead - the same thing, spelled the way this
        // form accepts.
        let separator = if url.contains('?') { '&' } else { '?' };
        return format!("{url}{separator}user={user}");
    }
    format!("{scheme}://{user}@{rest}")
}

/// A connection string with any userinfo stripped, for a message an
/// operator has to read. The host and database still identify the
/// target - which is the whole point of echoing it before a
/// destructive act - while a password copied into CI scrollback
/// outlives the run that leaked it.
pub fn redact_database_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    // Userinfo, if present, precedes the first `@`, and that `@` must
    // come before the path - otherwise it belongs to the database name.
    let authority_end = rest.find('/').unwrap_or(rest.len());
    match rest[..authority_end].rfind('@') {
        Some(at) => format!("{scheme}://{}", &rest[at + 1..]),
        None => url.to_string(),
    }
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
            // Audited for AssertSqlSafe: `role` is one of this crate's two
            // role constants, quoted - no caller input reaches this string.
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "CREATE ROLE {} NOLOGIN",
                quote_ident(role)
            )))
            .execute(&mut *tx)
            .await
            .map_err(provision_error)?;
        }
    }
    // Audited: built from the same two constants, quoted.
    sqlx::raw_sql(sqlx::AssertSqlSafe(least_privilege_sql(
        WRITER_ROLE,
        READER_ROLE,
    )))
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

#[cfg(test)]
mod redaction_tests {
    use super::redact_database_url;

    #[test]
    fn strips_userinfo_but_keeps_the_target_identifiable() {
        assert_eq!(
            redact_database_url("postgres://user:secret@db.internal:5432/morpholog"),
            "postgres://db.internal:5432/morpholog"
        );
        assert_eq!(
            redact_database_url("postgres://user@host/db"),
            "postgres://host/db"
        );
    }

    #[test]
    fn leaves_a_url_without_credentials_alone() {
        // The local socket form the whole test suite uses.
        assert_eq!(
            redact_database_url("postgres:///morpholog_dev"),
            "postgres:///morpholog_dev"
        );
        assert_eq!(
            redact_database_url("postgres://localhost:5432/morpholog"),
            "postgres://localhost:5432/morpholog"
        );
    }

    #[test]
    fn an_at_sign_in_the_path_is_not_userinfo() {
        // Naive splitting on the first `@` would truncate the database
        // name here and hide which target was refused.
        assert_eq!(
            redact_database_url("postgres://host/weird@name"),
            "postgres://host/weird@name"
        );
    }

    #[test]
    fn a_password_containing_an_at_sign_is_fully_stripped() {
        // Splitting on the FIRST `@` would leave the tail of the
        // password in the message.
        assert_eq!(
            redact_database_url("postgres://user:p@ss@host/db"),
            "postgres://host/db"
        );
    }

    #[test]
    fn a_malformed_string_is_returned_unchanged_rather_than_mangled() {
        assert_eq!(redact_database_url("not-a-url"), "not-a-url");
    }
}

#[cfg(test)]
mod default_user_tests {
    use super::apply_default_user;

    // sqlx 0.9 connects an unspecified user as `anonymous`, where libpq,
    // psql, and sqlx 0.8 all use the OS user. These pin the compatibility
    // shim: the short URL form is what every Postgres tool means by "this
    // database, as me", and what this project's install guide,
    // CONTRIBUTING, and whole test suite use.

    #[test]
    fn a_socket_url_gains_the_supplied_user_as_a_query_parameter() {
        // Not `postgres://alice@/morpholog_dev`: with no host, injected
        // userinfo reads as an empty host and the driver refuses it.
        // Caught by an end-to-end connection, not by this test - which is
        // why the shape is pinned here now.
        assert_eq!(
            apply_default_user("postgres:///morpholog_dev", Some("alice")),
            "postgres:///morpholog_dev?user=alice"
        );
    }

    #[test]
    fn an_existing_query_string_is_extended_not_replaced() {
        assert_eq!(
            apply_default_user("postgres:///db?sslmode=disable", Some("alice")),
            "postgres:///db?sslmode=disable&user=alice"
        );
    }

    #[test]
    fn a_host_without_a_user_still_gains_one() {
        assert_eq!(
            apply_default_user("postgres://localhost:5432/db", Some("alice")),
            "postgres://alice@localhost:5432/db"
        );
    }

    #[test]
    fn an_explicit_user_is_never_overridden() {
        // Userinfo in the authority, password and all.
        assert_eq!(
            apply_default_user("postgres://carol:pw@host:5432/db", Some("alice")),
            "postgres://carol:pw@host:5432/db"
        );
        // The query-parameter spelling, which is how this shim is
        // side-stepped deliberately.
        assert_eq!(
            apply_default_user("postgres:///db?user=carol", Some("alice")),
            "postgres:///db?user=carol"
        );
    }

    #[test]
    fn an_at_sign_in_the_database_name_is_not_userinfo() {
        // The authority ends at the first `/` or `?`, so a later `@`
        // must not be mistaken for credentials and suppress the fill-in.
        assert_eq!(
            apply_default_user("postgres:///weird@name", Some("alice")),
            "postgres:///weird@name?user=alice"
        );
    }

    #[test]
    fn with_no_user_available_the_url_is_untouched() {
        assert_eq!(apply_default_user("postgres:///db", None), "postgres:///db");
    }

    #[test]
    fn a_malformed_string_is_returned_unchanged() {
        assert_eq!(apply_default_user("not-a-url", Some("alice")), "not-a-url");
    }
}
