//! Day-zero provisioning: the embedded schema, and the opt-in
//! least-privilege floor that makes the governed path the only way in.

use crate::error::{PgError, classify, classify_checked_query};
use crate::sql_quote::quote_ident;
use sqlx::PgPool;
use std::fmt::Write as _;

/// The canonical Morpholog schema, compiled in so a binary-only
/// deployment provisions exactly the schema this build expects.
pub const SCHEMA_SQL: &str = include_str!("../../morpholog-core/sql/schema.sql");

/// Outcome of [`initialise_schema`]: provisioned now, or found already
/// provisioned (the caller decides whether that is fine or an error).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitOutcome {
    Initialised,
    AlreadyInitialised,
}

/// Provision the `morpholog` schema from the embedded `SCHEMA_SQL`.
///
/// Day-zero only: if the schema exists, returns
/// [`InitOutcome::AlreadyInitialised`] and touches nothing. It never drops
/// or migrates (see [`crate::apply_migrations`]), so it is safe against a
/// live database.
///
/// Atomic: the check and the whole script run in one transaction, so a
/// failure leaves no partial schema to be mistaken for an initialised one.
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
    // A fresh schema is at the head, so record every migration as applied.
    crate::migrations::record_all_applied(&mut tx).await?;
    tx.commit().await.map_err(classify)?;
    Ok(InitOutcome::Initialised)
}

/// A connection string with a username filled in when it names none, so
/// `postgres:///mydb` keeps working. sqlx 0.9 connects an unnamed user as
/// `anonymous`, which turns this standard short form into a
/// peer-authentication failure.
///
/// **Not libpq parity.** libpq uses the effective OS account; this reads
/// `PGUSER`, then `USER`, then `LOGNAME`, which can differ under `sudo`, a
/// service manager, or a container. Those contexts should name the user in
/// the URL. With nothing available the URL is returned untouched, so the
/// driver reports the problem rather than this inventing an identity.
pub fn with_default_user(url: &str) -> String {
    // An EMPTY variable counts as unset and falls through - `PGUSER=` is
    // how a CI environment often clears it, and treating it as a value
    // suppressed the fallback entirely (found by the shell-twin
    // agreement test, which the pure-function unit tests could not see).
    let user = ["PGUSER", "USER", "LOGNAME"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok())
        .find(|value| !value.is_empty());
    apply_default_user(url, user.as_deref())
}

/// A pool capped at one connection, for a resident session: its lockstep
/// protocol cannot use a second, and the cap bounds load when many workers
/// each hold one. The URL is used as given; apply [`with_default_user`]
/// first for the OS-user default.
pub async fn single_connection_pool(url: &str) -> Result<PgPool, PgError> {
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(url)
        .await
        .map_err(crate::error::classify)
}

/// [`with_default_user`] with the username supplied rather than read from
/// the environment.
pub fn with_user(url: &str, user: &str) -> String {
    apply_default_user(url, Some(user))
}

/// Percent-encode a username for a URL query value, BY BYTE outside the
/// unreserved set. UTF-8 survives, and a username cannot smuggle in
/// options: `PGUSER='ops&sslmode=disable'` would otherwise disable TLS.
fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// The rule itself, without the environment lookup, so tests can pin it
/// without mutating the environment (which needs `unsafe`).
fn apply_default_user(url: &str, user: Option<&str>) -> String {
    let Some((_, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    // Userinfo present, or a `user` query parameter: the caller has
    // spoken. Matched at a parameter boundary, so a database or option
    // named `...user` (`clusteruser=`, `superuser=1`) does not silently
    // suppress the fill-in and leave the caller connecting as anonymous.
    let has_user_param = rest
        .split_once('?')
        .map(|(_, query)| {
            query
                .split('&')
                .any(|param| param == "user" || param.starts_with("user="))
        })
        .unwrap_or(false);
    if rest[..authority_end].contains('@') || has_user_param {
        return url.to_string();
    }
    // Nothing to fill in with: let the driver report its own error
    // rather than invent a username.
    let Some(user) = user else {
        return url.to_string();
    };
    // Always the query parameter, never injected userinfo. Userinfo
    // needs two special cases - the hostless socket form reads `user@`
    // as an empty host and refuses it, and a username carrying a
    // delimiter (`PGUSER=user@server`, the Azure shape) would give two
    // `@` and a wrong host - while the parameter form has none and means
    // the same thing to the driver. One spelling is also what lets the
    // shell twin in the scripts stay verifiably identical.
    let separator = if rest.contains('?') { '&' } else { '?' };
    format!("{url}{separator}user={}", percent_encode(user))
}

/// A connection string with any userinfo stripped, for operator messages.
/// Host and database still identify the target; a password must not land
/// in CI logs.
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
/// Destructive and deliberately dumb: the caller owns the confirmation,
/// because a library cannot tell a scratch database from production.
/// Returns whether there was a schema to drop.
///
/// Not atomic with the [`initialise_schema`] that follows; a failure in
/// between leaves an unprovisioned database, which re-running `init`
/// recovers.
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

/// Whether this database has the least-privilege floor provisioned.
///
/// Asked after migrating: the floor grants per table, so a migration that
/// adds one leaves the roles without access until the floor is re-applied.
pub(crate) async fn least_privilege_roles_exist(pool: &PgPool) -> Result<bool, PgError> {
    let found = sqlx::query!(
        "SELECT 1 AS one FROM pg_roles WHERE rolname = $1",
        WRITER_ROLE
    )
    .fetch_optional(pool)
    .await
    .map_err(classify_checked_query)?;
    Ok(found.is_some())
}

/// Provision the least-privilege floor: create the [`WRITER_ROLE`] and
/// [`READER_ROLE`] group roles (kept if they exist; roles are
/// cluster-global), revoke PUBLIC from the governed schemas and tables,
/// and grant each role exactly what it needs. Idempotent and in one
/// transaction.
///
/// The writer gets the runtime's write set and nothing more. In
/// particular `morpholog.audit` gets INSERT and SELECT only, so the log is
/// append-only in the database even for the runtime's own role.
///
/// Membership grants (and `pg_read_all_stats` for an audit-tailing
/// reader) are left to the operator, so no secret or cluster-wide policy
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

/// Name the remedy when the connection role cannot provision. CREATE ROLE
/// needs CREATEROLE, and REVOKE/GRANT need ownership of the governed
/// tables (normally the role that ran `morpholog init`). A superuser has
/// both.
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

/// The REVOKE/GRANT script, pure so a test pins the exact privilege floor.
/// The grants are the runtime SQL's write set, table by table.
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
    // Readable by both, written by neither: a deployment role can ask
    // `migrate --check` without the power to migrate.
    let _ = writeln!(
        out,
        "GRANT SELECT ON morpholog.schema_migrations TO {w}, {r};"
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

    // sqlx 0.9 connects an unnamed user as `anonymous`, where libpq and
    // psql use the OS user. These pin the shim that restores the short URL
    // form's meaning: "this database, as me".

    #[test]
    fn a_socket_url_gains_the_supplied_user_as_a_query_parameter() {
        // Not `postgres://alice@/morpholog_dev`: with no host, injected
        // userinfo reads as an empty host and the driver refuses it.
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
            "postgres://localhost:5432/db?user=alice"
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
    fn a_parameter_merely_ending_in_user_does_not_suppress_the_fill_in() {
        // A substring match on "user=" would skip the fill-in here and
        // connect as `anonymous`.
        assert_eq!(
            apply_default_user("postgres:///db?clusteruser=x", Some("alice")),
            "postgres:///db?clusteruser=x&user=alice"
        );
        assert_eq!(
            apply_default_user("postgres:///db?superuser=1", Some("alice")),
            "postgres:///db?superuser=1&user=alice"
        );
    }

    #[test]
    fn a_username_carrying_a_delimiter_is_carried_safely() {
        // `PGUSER=user@server` is the Azure Postgres shape. Injected as
        // userinfo it would produce two `@` and a wrong host, so it goes
        // in as the parameter instead.
        // Percent-encoded, and provably still read as `alice@server`:
        // the shell-twin suite parses this back through
        // `PgConnectOptions` and asserts the username round-trips.
        assert_eq!(
            apply_default_user("postgres://host:5432/db", Some("alice@server")),
            "postgres://host:5432/db?user=alice%40server"
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
