//! Moving a deployment from one recorded pair of roles to a new pair of
//! its own: the step a deployment provisioned with the shared default
//! pair takes when that pair also reaches another database.
//!
//! Role membership belongs to the whole cluster, so the move never
//! changes who is a member of the old pair, and never drops it: another
//! deployment may still use both. It moves this database's privileges,
//! and the memberships of the logins the operator names.

use crate::error::{PgError, classify, classify_checked_query};
use crate::provision::{
    DeploymentRoles, existing_role, grant_floor, other_databases, provision_error, recorded_roles,
    require_recorded_roles, role_exists,
};
use crate::sql_quote::quote_ident;
use serde::Serialize;
use sqlx::PgPool;

/// What a move did, printed only once it has committed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RolesMoveReport {
    /// The pair this database recorded before the move.
    pub from: RolePair,
    /// The named logins, each now a member of the new role of every kind
    /// it held, with the same inherit and `SET ROLE` settings.
    pub logins: Vec<MovedLogin>,
    /// Other databases the old pair still holds privileges in or on. Its
    /// memberships were kept, so remove them only once no deployment
    /// needs them.
    pub old_pair_reaches: Vec<String>,
    /// The pair this database records now.
    pub to: RolePair,
    /// Direct members of the old pair that were not named. They no longer
    /// hold, through the old pair, anything this deployment granted it;
    /// what they hold directly or through other roles is not read here.
    pub unmoved_members: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RolePair {
    pub reader: String,
    pub writer: String,
}

impl From<&DeploymentRoles> for RolePair {
    fn from(roles: &DeploymentRoles) -> Self {
        Self {
            reader: roles.reader().to_string(),
            writer: roles.writer().to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MovedLogin {
    pub login: String,
    pub reader: bool,
    pub writer: bool,
}

/// One direct membership of an old role, its grants folded together.
struct Membership {
    member: String,
    can_login: bool,
    role: String,
    admin: bool,
    inherit: bool,
    set: bool,
}

/// Move this database from its recorded pair to `to`, in one transaction.
///
/// Refuses, changing nothing, unless the schema is current, a pair is
/// recorded and still exists, `to` differs from it and neither of its
/// roles exists, the caller may create roles, and every named login can
/// log in and is a direct member of the old pair without the admin
/// option. It also refuses when the old pair holds anything in this
/// database beyond the floor `to` has just been granted - a privilege
/// granted by hand, an object it owns, a policy or default privilege
/// naming it, a privilege on the database itself - so nothing is revoked
/// that the move did not grant. The record table is locked first, so a
/// `migrate` re-applying the floor waits for the move or sees its result.
pub async fn move_deployment_roles(
    pool: &PgPool,
    to: &DeploymentRoles,
    logins: &[String],
) -> Result<RolesMoveReport, PgError> {
    crate::migrations::require_current_schema(pool).await?;
    let mut tx = pool.begin().await.map_err(classify)?;
    sqlx::raw_sql("LOCK TABLE morpholog.deployment_roles IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *tx)
        .await
        .map_err(classify)?;

    let Some(from) = recorded_roles(&mut tx).await? else {
        return Err(PgError::InvalidState(
            "this database records no least-privilege roles, so there is nothing to move; \
             provision them with `morpholog init --skip-if-exists --least-privilege`"
                .to_string(),
        ));
    };
    require_recorded_roles(&mut tx, &from).await?;
    if from == *to {
        return Err(PgError::InvalidState(format!(
            "this database already records `{}` and `{}`",
            to.writer(),
            to.reader()
        )));
    }
    for role in to.both() {
        if role_exists(&mut tx, role).await? {
            return Err(existing_role(&mut tx, role).await?);
        }
    }
    let may_create = sqlx::query_scalar!(
        "SELECT rolsuper OR rolcreaterole AS \"may!\" FROM pg_roles WHERE rolname = current_user"
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(classify_checked_query)?;
    if !may_create {
        return Err(PgError::InvalidState(
            "moving roles creates the new pair, and this role may not create roles; connect \
             as a superuser, or as a role that has CREATEROLE and owns the morpholog tables"
                .to_string(),
        ));
    }

    let old: Vec<String> = from.both().map(str::to_string).to_vec();
    let memberships: Vec<Membership> = sqlx::query!(
        r#"SELECT m.rolname::text AS "member!", m.rolcanlogin AS "can_login!",
                  g.rolname::text AS "role!",
                  bool_or(am.admin_option) AS "admin!",
                  bool_or(am.inherit_option) AS "inherit!",
                  bool_or(am.set_option) AS "set!"
           FROM pg_auth_members am
           JOIN pg_roles g ON g.oid = am.roleid
           JOIN pg_roles m ON m.oid = am.member
           WHERE g.rolname = ANY($1)
           GROUP BY 1, 2, 3
           ORDER BY 1, 3"#,
        &old,
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(classify_checked_query)?
    .into_iter()
    .map(|r| Membership {
        member: r.member,
        can_login: r.can_login,
        role: r.role,
        admin: r.admin,
        inherit: r.inherit,
        set: r.set,
    })
    .collect();
    for login in logins {
        let held: Vec<&Membership> = memberships.iter().filter(|m| &m.member == login).collect();
        let Some(first) = held.first() else {
            return Err(PgError::InvalidState(format!(
                "`{login}` is not a direct member of `{}` or `{}`, so moving it would grant \
                 access it does not have here; name only the logins this deployment's old \
                 roles were granted to",
                from.writer(),
                from.reader()
            )));
        };
        if !first.can_login {
            return Err(PgError::InvalidState(format!(
                "`{login}` cannot log in; name the logins, and move a group role's members \
                 by hand"
            )));
        }
        if let Some(m) = held.iter().find(|m| m.admin) {
            return Err(PgError::InvalidState(format!(
                "`{login}` holds `{}` with the admin option, which the move does not grant \
                 again; revoke the admin option first, or move this login by hand",
                m.role
            )));
        }
    }

    for role in to.both() {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE ROLE {} NOLOGIN",
            quote_ident(role)
        )))
        .execute(&mut *tx)
        .await
        .map_err(provision_error)?;
    }
    grant_floor(&mut tx, to).await?;

    let beyond = beyond_the_floor(&mut tx, &from, to).await?;
    if !beyond.is_empty() {
        return Err(PgError::InvalidState(format!(
            "the old roles hold more in this database than the floor Morpholog grants, and \
             the move revokes only that floor; remove these by hand, or keep the old roles: {}",
            beyond.join("; ")
        )));
    }

    let mut moved: Vec<MovedLogin> = Vec::new();
    for login in logins {
        let mut entry = MovedLogin {
            login: login.clone(),
            reader: false,
            writer: false,
        };
        for m in memberships.iter().filter(|m| &m.member == login) {
            let (new_role, is_writer) = if m.role == from.writer() {
                (to.writer(), true)
            } else {
                (to.reader(), false)
            };
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "GRANT {} TO {} WITH INHERIT {}, SET {}",
                quote_ident(new_role),
                quote_ident(login),
                m.inherit,
                m.set
            )))
            .execute(&mut *tx)
            .await
            .map_err(provision_error)?;
            if is_writer {
                entry.writer = true;
            } else {
                entry.reader = true;
            }
        }
        moved.push(entry);
    }

    let (w, r) = (quote_ident(from.writer()), quote_ident(from.reader()));
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "REVOKE ALL ON ALL TABLES IN SCHEMA morpholog, morpholog_read FROM {w}, {r}; \
         REVOKE ALL ON SCHEMA morpholog, morpholog_read FROM {w}, {r};"
    )))
    .execute(&mut *tx)
    .await
    .map_err(provision_error)?;
    let left = held_here(&mut tx, &from).await?;
    if left != 0 {
        return Err(PgError::InvalidState(format!(
            "after the revokes the old roles still hold {left} grant(s) in this database; \
             nothing was changed"
        )));
    }

    sqlx::query!("DELETE FROM morpholog.deployment_roles")
        .execute(&mut *tx)
        .await
        .map_err(classify_checked_query)?;
    sqlx::query!(
        "INSERT INTO morpholog.deployment_roles (writer_role, reader_role) VALUES ($1, $2)",
        to.writer(),
        to.reader(),
    )
    .execute(&mut *tx)
    .await
    .map_err(classify_checked_query)?;

    let mut unmoved: Vec<String> = memberships
        .iter()
        .filter(|m| !logins.contains(&m.member))
        .map(|m| m.member.clone())
        .collect();
    unmoved.dedup();
    let old_pair_reaches = other_databases(&mut tx, &from).await?;
    tx.commit().await.map_err(classify)?;
    Ok(RolesMoveReport {
        from: RolePair::from(&from),
        logins: moved,
        old_pair_reaches,
        to: RolePair::from(to),
        unmoved_members: unmoved,
    })
}

/// What the old pair holds in this database, or on it, that the new pair
/// was not just granted: each as one line naming the role and the object.
async fn beyond_the_floor(
    conn: &mut sqlx::PgConnection,
    from: &DeploymentRoles,
    to: &DeploymentRoles,
) -> Result<Vec<String>, PgError> {
    sqlx::query_scalar!(
        r#"WITH pairs AS (
               SELECT o.oid AS old_oid, o.rolname::text AS old_name, n.oid AS new_oid
               FROM pg_roles o
               JOIN pg_roles n
                 ON (o.rolname = $1 AND n.rolname = $3) OR (o.rolname = $2 AND n.rolname = $4)
           ),
           here AS (SELECT oid FROM pg_database WHERE datname = current_database()),
           deps AS (
               SELECT s.classid, s.objid, s.objsubid, s.deptype, p.old_oid, p.old_name, p.new_oid
               FROM pg_shdepend s
               JOIN pairs p ON p.old_oid = s.refobjid
               WHERE s.refclassid = 'pg_authid'::regclass
                 AND (s.dbid = (SELECT oid FROM here)
                      OR (s.dbid = 0 AND s.classid = 'pg_database'::regclass
                          AND s.objid = (SELECT oid FROM here)))
           )
           SELECT format('`%s` %s %s', d.old_name,
                         CASE d.deptype WHEN 'o' THEN 'owns'
                                        WHEN 'r' THEN 'is named in a policy on'
                                        ELSE 'holds a privilege on' END,
                         pg_describe_object(d.classid, d.objid, d.objsubid)) AS "finding!"
           FROM deps d
           WHERE NOT (d.deptype = 'a' AND d.objsubid = 0
                      AND d.classid IN ('pg_class'::regclass, 'pg_namespace'::regclass))
           UNION ALL
           SELECT format('`%s` holds %s%s on %s', d.old_name, a.privilege_type,
                         CASE WHEN a.is_grantable THEN ' with grant option' ELSE '' END,
                         pg_describe_object(d.classid, d.objid, 0))
           FROM deps d
           JOIN pg_class c ON d.classid = 'pg_class'::regclass AND c.oid = d.objid
           CROSS JOIN LATERAL aclexplode(c.relacl) a
           WHERE d.deptype = 'a' AND d.objsubid = 0 AND a.grantee = d.old_oid
             AND (a.is_grantable OR NOT EXISTS (
                   SELECT 1 FROM aclexplode(c.relacl) b
                   WHERE b.grantee = d.new_oid AND b.privilege_type = a.privilege_type))
           UNION ALL
           SELECT format('`%s` holds %s%s on %s', d.old_name, a.privilege_type,
                         CASE WHEN a.is_grantable THEN ' with grant option' ELSE '' END,
                         pg_describe_object(d.classid, d.objid, 0))
           FROM deps d
           JOIN pg_namespace ns ON d.classid = 'pg_namespace'::regclass AND ns.oid = d.objid
           CROSS JOIN LATERAL aclexplode(ns.nspacl) a
           WHERE d.deptype = 'a' AND d.objsubid = 0 AND a.grantee = d.old_oid
             AND (a.is_grantable OR NOT EXISTS (
                   SELECT 1 FROM aclexplode(ns.nspacl) b
                   WHERE b.grantee = d.new_oid AND b.privilege_type = a.privilege_type))
           ORDER BY 1"#,
        from.writer(),
        from.reader(),
        to.writer(),
        to.reader(),
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(classify_checked_query)
}

/// How many grants, ownerships or policy references of `roles` remain in
/// this database or on it.
async fn held_here(conn: &mut sqlx::PgConnection, roles: &DeploymentRoles) -> Result<i64, PgError> {
    let names: Vec<String> = roles.both().map(str::to_string).to_vec();
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "count!"
           FROM pg_shdepend s
           JOIN pg_roles r ON r.oid = s.refobjid
           WHERE s.refclassid = 'pg_authid'::regclass
             AND r.rolname = ANY($1)
             AND (s.dbid = (SELECT oid FROM pg_database WHERE datname = current_database())
                  OR (s.dbid = 0 AND s.classid = 'pg_database'::regclass
                      AND s.objid = (SELECT oid FROM pg_database
                                     WHERE datname = current_database())))"#,
        &names,
    )
    .fetch_one(&mut *conn)
    .await
    .map_err(classify_checked_query)
}
