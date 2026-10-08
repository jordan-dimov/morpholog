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
    // The file records the migrations it embodies, so the fresh schema
    // says it is at the head.
    sqlx::raw_sql(SCHEMA_SQL)
        .execute(&mut *tx)
        .await
        .map_err(classify)?;
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
/// Returns whether there was a schema to drop, and the least-privilege
/// roles it recorded, which [`rebind_least_privilege`] can bind again.
///
/// Not atomic with the [`initialise_schema`] that follows; a failure in
/// between leaves an unprovisioned database, which re-running `init`
/// recovers.
pub async fn drop_schema(pool: &PgPool) -> Result<DroppedSchema, PgError> {
    let mut tx = pool.begin().await.map_err(classify)?;
    let existed = sqlx::query!("SELECT 1 AS one FROM pg_namespace WHERE nspname = 'morpholog'")
        .fetch_optional(&mut *tx)
        .await
        .map_err(classify)?
        .is_some();
    let database = this_database(&mut tx).await?;
    let roles = recorded_roles(&mut tx)
        .await?
        .map(|roles| RecordedRoles { roles, database });
    sqlx::raw_sql("DROP SCHEMA IF EXISTS morpholog CASCADE")
        .execute(&mut *tx)
        .await
        .map_err(classify)?;
    tx.commit().await.map_err(classify)?;
    Ok(DroppedSchema { existed, roles })
}

/// What [`drop_schema`] dropped.
#[derive(Debug)]
pub struct DroppedSchema {
    pub existed: bool,
    pub roles: Option<RecordedRoles>,
}

/// The least-privilege roles a database recorded when its schema was
/// dropped, and which database that was. Only [`drop_schema`] makes one,
/// so binding it again restores what that same database had: never a
/// role chosen afterwards, and never another database's roles.
#[derive(Debug)]
pub struct RecordedRoles {
    roles: DeploymentRoles,
    database: i64,
}

impl RecordedRoles {
    pub fn roles(&self) -> &DeploymentRoles {
        &self.roles
    }
}

/// The current database's OID: unlike its name, never shared with a
/// database dropped and created again.
async fn this_database(conn: &mut sqlx::PgConnection) -> Result<i64, PgError> {
    sqlx::query_scalar!(
        r#"SELECT oid::int8 AS "oid!" FROM pg_database WHERE datname = current_database()"#
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(classify_checked_query)
}

/// The prefix of the default role names, `morpholog_writer` and
/// `morpholog_reader`.
pub const DEFAULT_ROLE_PREFIX: &str = "morpholog_";

/// One deployment's two group roles. The writer holds exactly the
/// runtime's write set; the reader holds read-only access to the governed
/// tables and the derived read cache, for dashboards, projections and
/// auditors. Both are NOLOGIN and passwordless: the operator grants
/// membership to real login roles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeploymentRoles {
    writer: String,
    reader: String,
}

impl DeploymentRoles {
    /// `<prefix>writer` and `<prefix>reader`. A prefix is lowercase ASCII
    /// letters, digits and `_`, starts with a letter, does not start with
    /// `pg_` (reserved by PostgreSQL), and keeps both names within
    /// PostgreSQL's 63-byte limit.
    pub fn with_prefix(prefix: &str) -> Result<Self, PgError> {
        let lawful = prefix.starts_with(|c: char| c.is_ascii_lowercase())
            && prefix
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            && !prefix.starts_with("pg_")
            && prefix.len() + "writer".len() <= 63;
        if !lawful {
            return Err(PgError::InvalidState(format!(
                "{prefix:?} is not a role prefix: use lowercase letters, digits and `_`, \
                 starting with a letter and not with `pg_`, at most 57 characters"
            )));
        }
        Ok(Self {
            writer: format!("{prefix}writer"),
            reader: format!("{prefix}reader"),
        })
    }

    pub fn writer(&self) -> &str {
        &self.writer
    }

    pub fn reader(&self) -> &str {
        &self.reader
    }

    fn both(&self) -> [&str; 2] {
        [&self.writer, &self.reader]
    }
}

impl Default for DeploymentRoles {
    fn default() -> Self {
        Self {
            writer: format!("{DEFAULT_ROLE_PREFIX}writer"),
            reader: format!("{DEFAULT_ROLE_PREFIX}reader"),
        }
    }
}

/// The roles this database records as its least-privilege floor, if it has
/// one. A database from before the record existed answers `None` until it
/// is migrated.
pub async fn deployment_roles(pool: &PgPool) -> Result<Option<DeploymentRoles>, PgError> {
    let mut conn = pool.acquire().await.map_err(classify)?;
    recorded_roles(&mut conn).await
}

async fn recorded_roles(conn: &mut sqlx::PgConnection) -> Result<Option<DeploymentRoles>, PgError> {
    let present = sqlx::query_scalar!(
        "SELECT to_regclass('morpholog.deployment_roles') IS NOT NULL AS \"present!\""
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(classify_checked_query)?;
    if !present {
        return Ok(None);
    }
    let row = sqlx::query!("SELECT writer_role, reader_role FROM morpholog.deployment_roles")
        .fetch_optional(&mut *conn)
        .await
        .map_err(classify_checked_query)?;
    Ok(row.map(|r| DeploymentRoles {
        writer: r.writer_role,
        reader: r.reader_role,
    }))
}

/// What migration 023 would leave in `morpholog.deployment_roles`, read
/// from the database as it is now: the row the table already holds, none
/// when the table exists without one, and otherwise the fixed pair when
/// both roles exist and hold the floor by name. The migration's own
/// census decides; this asks the same question without writing, and an
/// agreement test holds the two together.
pub async fn preview_role_backfill(pool: &PgPool) -> Result<Option<DeploymentRoles>, PgError> {
    let mut conn = pool.acquire().await.map_err(classify)?;
    let present = sqlx::query_scalar!(
        "SELECT to_regclass('morpholog.deployment_roles') IS NOT NULL AS \"present!\""
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(classify_checked_query)?;
    if present {
        return recorded_roles(&mut conn).await;
    }
    let fixed = DeploymentRoles::default();
    // Grants made to the role by name only: aclexplode lists PUBLIC as
    // grantee 0, and inheritance never appears in an ACL.
    let would_record = sqlx::query_scalar!(
        r#"SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)
              AND EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $2)
              AND (SELECT coalesce(array_agg(g), ARRAY[]::text[]) @> ARRAY[
                       'schema:' || $1 || ':USAGE',
                       'claims:' || $1 || ':INSERT',
                       'claims:' || $1 || ':DELETE',
                       'audit:' || $1 || ':INSERT',
                       'outbox:' || $1 || ':UPDATE',
                       'schema:' || $2 || ':USAGE',
                       'audit:' || $2 || ':SELECT']
                   FROM (SELECT c.relname || ':' || r.rolname || ':' || a.privilege_type AS g
                         FROM pg_class c, aclexplode(c.relacl) a, pg_roles r
                         WHERE c.relnamespace = 'morpholog'::regnamespace AND r.oid = a.grantee
                         UNION ALL
                         SELECT 'schema:' || r.rolname || ':' || a.privilege_type
                         FROM pg_namespace n, aclexplode(n.nspacl) a, pg_roles r
                         WHERE n.nspname = 'morpholog' AND r.oid = a.grantee) x)
           AS "would_record!""#,
        fixed.writer(),
        fixed.reader(),
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(classify_checked_query)?;
    Ok(would_record.then_some(fixed))
}

/// A role granted membership of a deployment role directly. Membership
/// through a third role, and what the member can do with it under its
/// inheritance and `SET ROLE` settings, are not read here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleMember {
    pub member: String,
    pub can_login: bool,
}

/// The direct members of a deployment's two roles, each list by name.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoleMembers {
    pub writer: Vec<RoleMember>,
    pub reader: Vec<RoleMember>,
}

pub async fn direct_members(
    pool: &PgPool,
    roles: &DeploymentRoles,
) -> Result<RoleMembers, PgError> {
    let mut conn = pool.acquire().await.map_err(classify)?;
    let names: Vec<String> = roles.both().map(str::to_string).to_vec();
    let rows = sqlx::query!(
        r#"SELECT g.rolname::text AS "group!", m.rolname::text AS "member!", m.rolcanlogin AS "can_login!"
           FROM pg_auth_members am
           JOIN pg_roles g ON g.oid = am.roleid
           JOIN pg_roles m ON m.oid = am.member
           WHERE g.rolname = ANY($1)
           ORDER BY 1, 2"#,
        &names,
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(classify_checked_query)?;
    let mut members = RoleMembers::default();
    for row in rows {
        let member = RoleMember {
            member: row.member,
            can_login: row.can_login,
        };
        if row.group == roles.writer() {
            members.writer.push(member);
        } else {
            members.reader.push(member);
        }
    }
    Ok(members)
}

/// One login's sessions on this database, other than the caller's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCount {
    pub role: String,
    pub sessions: i64,
}

/// The other client sessions on this database, by login, in name order.
/// Maintenance workers and the caller's own backend are left out. A
/// reading, not a lock: a session can open the moment after.
pub async fn other_sessions(pool: &PgPool) -> Result<Vec<SessionCount>, PgError> {
    let mut conn = pool.acquire().await.map_err(classify)?;
    let rows = sqlx::query!(
        r#"SELECT coalesce(usename::text, '?') AS "role!", count(*) AS "sessions!"
           FROM pg_stat_activity
           WHERE datname = current_database()
             AND pid <> pg_backend_pid()
             AND backend_type = 'client backend'
           GROUP BY 1
           ORDER BY 1"#
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(classify_checked_query)?;
    Ok(rows
        .into_iter()
        .map(|r| SessionCount {
            role: r.role,
            sessions: r.sessions,
        })
        .collect())
}

/// The other databases on this cluster in which `roles` hold a privilege
/// or own an object, or on which they hold a privilege or ownership. Read
/// from the cluster-wide dependency catalogue, so it sees databases this
/// connection cannot enter.
pub async fn databases_also_reached(
    pool: &PgPool,
    roles: &DeploymentRoles,
) -> Result<Vec<String>, PgError> {
    let mut conn = pool.acquire().await.map_err(classify)?;
    other_databases(&mut conn, roles).await
}

async fn other_databases(
    conn: &mut sqlx::PgConnection,
    roles: &DeploymentRoles,
) -> Result<Vec<String>, PgError> {
    let names: Vec<String> = roles.both().map(str::to_string).to_vec();
    sqlx::query_scalar!(
        r#"SELECT d.datname::text AS "datname!"
           FROM pg_shdepend s
           JOIN pg_database d ON d.oid = s.dbid
           JOIN pg_roles r ON r.oid = s.refobjid
           WHERE s.refclassid = 'pg_authid'::regclass
             AND r.rolname = ANY($1)
             AND d.datname <> current_database()
           UNION
           SELECT d.datname::text
           FROM pg_shdepend s
           JOIN pg_database d ON d.oid = s.objid
           JOIN pg_roles r ON r.oid = s.refobjid
           WHERE s.dbid = 0
             AND s.classid = 'pg_database'::regclass
             AND s.refclassid = 'pg_authid'::regclass
             AND r.rolname = ANY($1)
             AND d.datname <> current_database()
           ORDER BY 1"#,
        &names,
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(classify_checked_query)
}

/// The refusal for a role that exists but that this database does not
/// record. A deployment's roles are created by its own provisioning, so two
/// databases provisioning the same names at once collide on the name and
/// one fails; adopting an existing role would let both pass.
async fn existing_role(conn: &mut sqlx::PgConnection, role: &str) -> Result<PgError, PgError> {
    let single = DeploymentRoles {
        writer: role.to_string(),
        reader: role.to_string(),
    };
    let elsewhere = other_databases(conn, &single).await?;
    let used = if elsewhere.is_empty() {
        String::new()
    } else {
        format!(" (it reaches {})", elsewhere.join(", "))
    };
    Ok(PgError::InvalidState(format!(
        "the role `{role}` already exists{used} and this database does not record it; \
         Morpholog creates a deployment's roles itself, so choose another --role-prefix"
    )))
}

async fn role_exists(conn: &mut sqlx::PgConnection, role: &str) -> Result<bool, PgError> {
    Ok(
        sqlx::query!("SELECT 1 AS one FROM pg_roles WHERE rolname = $1", role)
            .fetch_optional(&mut *conn)
            .await
            .map_err(classify_checked_query)?
            .is_some(),
    )
}

/// Refuse when a role this database records no longer exists: recreating
/// it would hand the deployment's authority to whoever makes that name
/// next.
async fn require_recorded_roles(
    conn: &mut sqlx::PgConnection,
    roles: &DeploymentRoles,
) -> Result<(), PgError> {
    for role in roles.both() {
        if !role_exists(conn, role).await? {
            return Err(PgError::InvalidState(format!(
                "this database records `{}` and `{}` as its least-privilege roles, but \
                 `{role}` does not exist; Morpholog never recreates a role that holds a \
                 deployment's authority. Re-provision the floor deliberately (docs/install.md, \
                 \"Several deployments on one cluster\")",
                roles.writer, roles.reader
            )));
        }
    }
    Ok(())
}

/// Provision the least-privilege floor for `roles`: create both roles,
/// record the pair as this database's own, revoke PUBLIC from the governed
/// schemas and tables, and grant each role exactly what it needs.
/// Idempotent for the recorded pair, and in one transaction.
///
/// Roles belong to the whole cluster, so a deployment's are its own: a
/// role that already exists is refused unless this database records it,
/// as is a pair other than the one this database records.
///
/// The writer gets the runtime's write set and nothing more. In
/// particular `morpholog.audit` gets INSERT and SELECT only, so the log is
/// append-only in the database even for the runtime's own role.
///
/// Membership grants (and `pg_read_all_stats` for an audit-tailing
/// reader) are left to the operator, so no secret or cluster-wide policy
/// hides inside provisioning.
pub async fn provision_least_privilege(
    pool: &PgPool,
    roles: &DeploymentRoles,
) -> Result<(), PgError> {
    provision(pool, roles, false).await
}

/// Provision the floor again for the roles this database recorded before
/// its schema was dropped, as a reset does: they are bound again rather
/// than refused as unrecorded. A recorded role that no longer exists is
/// refused, never recreated, and roles another database recorded are
/// refused here.
pub async fn rebind_least_privilege(pool: &PgPool, recorded: RecordedRoles) -> Result<(), PgError> {
    let mut conn = pool.acquire().await.map_err(classify)?;
    if this_database(&mut conn).await? != recorded.database {
        return Err(PgError::InvalidState(format!(
            "`{}` and `{}` were recorded by another database; a reset binds again only \
             the roles of the database it reset",
            recorded.roles.writer, recorded.roles.reader
        )));
    }
    drop(conn);
    provision(pool, &recorded.roles, true).await
}

async fn provision(pool: &PgPool, roles: &DeploymentRoles, rebinding: bool) -> Result<(), PgError> {
    let mut tx = pool.begin().await.map_err(classify)?;
    match recorded_roles(&mut tx).await? {
        Some(recorded) if recorded != *roles => {
            return Err(PgError::InvalidState(format!(
                "this database records `{}` and `{}` as its least-privilege roles, not `{}` \
                 and `{}`; changing a deployment's roles is a re-provision (docs/install.md, \
                 \"Several deployments on one cluster\")",
                recorded.writer, recorded.reader, roles.writer, roles.reader
            )));
        }
        Some(recorded) => require_recorded_roles(&mut tx, &recorded).await?,
        None => {
            // A rebind restores roles that still exist; a missing one is a
            // broken binding, never recreated.
            if rebinding {
                require_recorded_roles(&mut tx, roles).await?;
            }
            for role in roles.both() {
                if role_exists(&mut tx, role).await? {
                    if rebinding {
                        continue;
                    }
                    return Err(existing_role(&mut tx, role).await?);
                }
                // Audited for AssertSqlSafe: `role` is a validated prefix
                // plus a fixed suffix, quoted.
                sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                    "CREATE ROLE {} NOLOGIN",
                    quote_ident(role)
                )))
                .execute(&mut *tx)
                .await
                .map_err(provision_error)?;
            }
            sqlx::query!(
                "INSERT INTO morpholog.deployment_roles (writer_role, reader_role) VALUES ($1, $2)",
                roles.writer,
                roles.reader,
            )
            .execute(&mut *tx)
            .await
            .map_err(classify_checked_query)?;
        }
    }
    grant_floor(&mut tx, roles).await?;
    tx.commit().await.map_err(classify)?;
    Ok(())
}

/// Re-apply the floor to the roles this database records, after a
/// migration added tables a grant could not reach. Creates no role and
/// grants to no other: a database without a record has no floor.
pub(crate) async fn reapply_least_privilege(pool: &PgPool) -> Result<(), PgError> {
    let mut tx = pool.begin().await.map_err(classify)?;
    if let Some(roles) = recorded_roles(&mut tx).await? {
        require_recorded_roles(&mut tx, &roles).await?;
        grant_floor(&mut tx, &roles).await?;
    }
    tx.commit().await.map_err(classify)?;
    Ok(())
}

/// Refuse when a role this database records is gone: before migrating, so
/// a migration never lands tables the floor then cannot reach, and before
/// a reset drops the record that names it.
pub async fn require_deployment_roles(pool: &PgPool) -> Result<(), PgError> {
    let mut conn = pool.acquire().await.map_err(classify)?;
    match recorded_roles(&mut conn).await? {
        Some(roles) => require_recorded_roles(&mut conn, &roles).await,
        None => Ok(()),
    }
}

/// Refuse, before `init --least-privilege` changes anything, whatever
/// provisioning `roles` afterwards would refuse. An init that created or
/// dropped the schema and then refused would leave one with no
/// least-privilege floor.
pub async fn require_can_provision_least_privilege(
    pool: &PgPool,
    roles: &DeploymentRoles,
) -> Result<(), PgError> {
    let mut conn = pool.acquire().await.map_err(classify)?;
    let schema = sqlx::query!("SELECT 1 AS one FROM pg_namespace WHERE nspname = 'morpholog'")
        .fetch_optional(&mut *conn)
        .await
        .map_err(classify_checked_query)?
        .is_some();
    let record = sqlx::query_scalar!(
        "SELECT to_regclass('morpholog.deployment_roles') IS NOT NULL AS \"present!\""
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(classify_checked_query)?;
    if schema {
        // Behind names `migrate`; ahead never advises it.
        drop(conn);
        crate::migrations::require_current_schema(pool).await?;
        conn = pool.acquire().await.map_err(classify)?;
        if !record {
            return Err(PgError::InvalidState(
                "this database records the migration that adds `morpholog.deployment_roles`, \
                 but the table is gone, so its schema is not one Morpholog made and `migrate` \
                 cannot repair it; restore it from a backup"
                    .to_string(),
            ));
        }
    }
    match recorded_roles(&mut conn).await? {
        Some(recorded) if recorded != *roles => Err(PgError::InvalidState(format!(
            "this database records `{}` and `{}` as its least-privilege roles, not `{}` and \
             `{}`; pass their prefix, or move the deployment to new roles first \
             (docs/install.md, \"Several deployments on one cluster\")",
            recorded.writer, recorded.reader, roles.writer, roles.reader
        ))),
        Some(recorded) => require_recorded_roles(&mut conn, &recorded).await,
        None => {
            for role in roles.both() {
                if role_exists(&mut conn, role).await? {
                    return Err(existing_role(&mut conn, role).await?);
                }
            }
            let may_create = sqlx::query_scalar!(
                "SELECT rolsuper OR rolcreaterole AS \"may!\" FROM pg_roles \
                 WHERE rolname = current_user"
            )
            .fetch_one(&mut *conn)
            .await
            .map_err(classify_checked_query)?;
            if !may_create {
                return Err(PgError::InvalidState(
                    "least-privilege provisioning creates the deployment's roles, and this \
                     role may not create roles; connect as a superuser, or as a role that \
                     has CREATEROLE and owns the morpholog tables (normally the role that \
                     ran `morpholog init`)"
                        .to_string(),
                ));
            }
            Ok(())
        }
    }
}

async fn grant_floor(
    conn: &mut sqlx::PgConnection,
    roles: &DeploymentRoles,
) -> Result<(), PgError> {
    // Audited: built from validated or recorded names, quoted.
    sqlx::raw_sql(sqlx::AssertSqlSafe(least_privilege_sql(
        &roles.writer,
        &roles.reader,
    )))
    .execute(&mut *conn)
    .await
    .map_err(provision_error)?;
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
    let _ = writeln!(
        out,
        "GRANT SELECT ON morpholog.deployment_roles TO {w}, {r};"
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
        let roles = DeploymentRoles::default();
        let sql = least_privilege_sql(roles.writer(), roles.reader());
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
