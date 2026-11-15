//! Which database role may propose as which actor.
//!
//! Audit records the login role that vouched for an actor. Without a
//! policy, one role can assert any actor, so one connection can play two
//! "distinct" people and every rule about distinct people is empty.
//!
//! Two separate claims close that:
//!
//! - `ActorAssertionRestricted(actor)` is the POLICY. It takes effect
//!   only when the claim is admitted, not when the predicate is
//!   declared. An actor with no such claim may be asserted by anyone.
//! - `ActorAssertionAuthority(actor, login_role)` is the GRANT: this
//!   login role may assert this actor.
//!
//! Arming on the grants alone would be wrong: retracting the last grant
//! would silently unrestrict the actor just as access is being revoked.
//! Here, retracting the last grant locks the actor out. Unrestricting
//! means retracting the policy claim, a visible act of its own.
//!
//! Both are ordinary claims the operator governs through its own
//! transformations. The runtime only recognises their shape, as it does
//! for `AuditSigningKey`.
//!
//! **Limits.** The check runs in the adapter, so it binds only callers
//! that go through Morpholog. It does not defend against a compromised
//! gateway: the writer role can insert claims and attestation-shaped
//! audit rows directly. Two verifiers are truly distinct only when their
//! gateways and credentials are. This is actor-assertion policy, not
//! proof of authorship.

use morpholog_core::{PredicateArgKind, Program, Subject};
use sqlx::{PgPool, Postgres, Transaction};

use crate::error::PgError;
use crate::error::classify_checked_query;

/// The policy claim: this actor may only be asserted by an authorised
/// login role.
pub const RESTRICTED_PREDICATE: &str = "ActorAssertionRestricted";

/// The grant claim: this login role may assert this actor.
pub const AUTHORITY_PREDICATE: &str = "ActorAssertionAuthority";

/// A declaration of a reserved name that the runtime cannot recognise.
///
/// A misshapen policy declaration fails silently: the runtime never
/// matches it and the restriction never takes effect. Refusing the
/// programme is the only point where that is visible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDeclarationError {
    pub predicate: String,
    pub detail: String,
}

impl std::fmt::Display for PolicyDeclarationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "`{}` is reserved for actor-assertion policy and {} - \
             the runtime would not recognise this declaration, so the \
             restriction it looks like would never take effect",
            self.predicate, self.detail
        )
    }
}

/// Check any declaration of the reserved names against the shape the
/// runtime matches. A programme that declares neither is unaffected.
pub fn validate_declarations(program: &Program) -> Vec<PolicyDeclarationError> {
    let expected: [(&str, usize); 2] = [(RESTRICTED_PREDICATE, 1), (AUTHORITY_PREDICATE, 2)];
    let mut findings = Vec::new();
    for declaration in &program.predicates {
        let name = declaration.name.as_str();
        let Some((_, arity)) = expected.iter().find(|(n, _)| *n == name) else {
            continue;
        };
        if declaration.args.len() != *arity {
            findings.push(PolicyDeclarationError {
                predicate: name.to_string(),
                detail: format!("takes {arity} argument(s), not {}", declaration.args.len()),
            });
            continue;
        }
        if let Some(bad) = declaration
            .args
            .iter()
            .find(|a| a.kind != PredicateArgKind::Subject)
        {
            findings.push(PolicyDeclarationError {
                predicate: name.to_string(),
                detail: format!(
                    "takes Subject arguments only; `{}` is declared {}",
                    bad.name, bad.kind
                ),
            });
        }
    }
    findings
}

/// Decide whether `login_role` may propose as `actor`.
///
/// Reads the policy inside the caller's transaction, so the answer is the
/// one in force in the snapshot the kernel evaluates against.
///
/// Unrestricted actors cost one indexed lookup that finds nothing.
pub(crate) async fn authorise(
    tx: &mut Transaction<'_, Postgres>,
    actor: &Subject,
    login_role: &str,
) -> Result<(), PgError> {
    // A misshapen policy claim would never match, leaving an actor the
    // operator thinks is restricted wide open. Refuse it here, the one
    // point every durable path passes: compensation arrives with no
    // programme, so the facades' declaration check does not cover it.
    let malformed = sqlx::query_scalar!(
        r#"SELECT count(*) AS "malformed!" FROM morpholog.claims
           WHERE (predicate_name = $1
                    AND jsonb_array_length(arguments) IS DISTINCT FROM 1)
              OR (predicate_name = $2
                    AND jsonb_array_length(arguments) IS DISTINCT FROM 2)"#,
        RESTRICTED_PREDICATE,
        AUTHORITY_PREDICATE,
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(classify_checked_query)?;
    if malformed > 0 {
        return Err(PgError::ActorPolicyDeclaration {
            findings: vec![format!(
                "{malformed} admitted claim(s) under `{RESTRICTED_PREDICATE}` or                  `{AUTHORITY_PREDICATE}` do not have the shape the runtime matches,                  so the restriction they look like would protect nothing"
            )],
        });
    }

    let identity = sqlx::query!(
        r#"SELECT closed AS "closed!", root_actor, root_login,
                  root_login_oid IS NOT DISTINCT FROM
                      (SELECT oid FROM pg_roles WHERE rolname = $1) AS "same_login!"
           FROM morpholog.deployment_identity"#,
        login_role,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(classify_checked_query)?
    .ok_or(PgError::DeploymentIdentityMissing)?;
    if identity.closed && identity.root_actor.as_deref() == Some(actor.as_str()) {
        let root_login = identity.root_login.as_deref() == Some(login_role);
        return if root_login && identity.same_login {
            Ok(())
        } else {
            Err(PgError::RootActorUnauthorised {
                actor: actor.clone(),
                login_role: login_role.to_string(),
            })
        };
    }

    let actor_arg = serde_json::json!([{"type": "subject", "value": actor.as_str()}]);
    let restricted = sqlx::query_scalar!(
        r#"SELECT EXISTS (
               SELECT 1 FROM morpholog.claims
               WHERE predicate_name = $1 AND arguments = $2
           ) AS "restricted!""#,
        RESTRICTED_PREDICATE,
        actor_arg,
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(classify_checked_query)?;
    if !restricted && !identity.closed {
        return Ok(());
    }

    let grant = serde_json::json!([
        {"type": "subject", "value": actor.as_str()},
        {"type": "subject", "value": login_role},
    ]);
    let authorised = sqlx::query_scalar!(
        r#"SELECT EXISTS (
               SELECT 1 FROM morpholog.claims
               WHERE predicate_name = $1 AND arguments = $2
           ) AS "authorised!""#,
        AUTHORITY_PREDICATE,
        grant,
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(classify_checked_query)?;
    if authorised {
        return Ok(());
    }
    Err(PgError::ActorAssertionUnauthorised {
        actor: actor.clone(),
        login_role: login_role.to_string(),
    })
}

/// Who may assert an actor on a deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeploymentIdentity {
    /// An actor nobody restricted may be asserted by any login.
    Open,
    /// Every actor needs a grant; the root actor is asserted by its root
    /// login alone.
    Closed {
        root_actor: Subject,
        root_login: String,
    },
}

/// What closing found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseOutcome {
    Closed,
    AlreadyClosed,
}

/// The deployment's identity row, read as a proposal reads it.
pub async fn deployment_identity(pool: &PgPool) -> Result<DeploymentIdentity, PgError> {
    let row = sqlx::query!(
        r#"SELECT closed AS "closed!", root_actor, root_login FROM morpholog.deployment_identity"#
    )
    .fetch_optional(pool)
    .await
    .map_err(classify_checked_query)?
    .ok_or(PgError::DeploymentIdentityMissing)?;
    Ok(match (row.closed, row.root_actor, row.root_login) {
        (true, Some(actor), Some(login)) => DeploymentIdentity::Closed {
            root_actor: Subject::from(actor),
            root_login: login,
        },
        _ => DeploymentIdentity::Open,
    })
}

/// The actors the audit log records that no `ActorAssertionAuthority`
/// claim names, `root` aside: the known actors a closed deployment would
/// refuse. It says nothing of actors not yet seen.
pub async fn actors_without_grant(pool: &PgPool, root: &Subject) -> Result<Vec<Subject>, PgError> {
    let actors = sqlx::query_scalar!(
        r#"SELECT DISTINCT a.actor ->> 'value' AS "actor!"
           FROM morpholog.audit a
           WHERE a.actor ->> 'type' = 'subject'
             AND a.actor ->> 'value' <> $1
             AND NOT EXISTS (
                 SELECT 1 FROM morpholog.claims c
                 WHERE c.predicate_name = $2 AND c.arguments -> 0 = a.actor
             )
           ORDER BY 1"#,
        root.as_str(),
        AUTHORITY_PREDICATE,
    )
    .fetch_all(pool)
    .await
    .map_err(classify_checked_query)?;
    Ok(actors.into_iter().map(Subject::from).collect())
}

/// Close the deployment under `root_actor`, asserted by `root_login`
/// alone, held by its OID. The owner's: the deployment's roles may not
/// write the row. Closing again under the same root changes nothing; a
/// different root is refused. A missing row is written closed.
pub async fn close_deployment(
    pool: &PgPool,
    root_actor: &Subject,
    root_login: &str,
) -> Result<CloseOutcome, PgError> {
    let mut tx = pool.begin().await.map_err(crate::error::classify)?;
    let oid = sqlx::query_scalar!(
        r#"SELECT oid AS "oid!: sqlx::postgres::types::Oid" FROM pg_roles WHERE rolname = $1"#,
        root_login,
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(classify_checked_query)?
    .ok_or_else(|| PgError::RootLoginMissing(root_login.to_string()))?;
    let closed = sqlx::query_scalar!(
        r#"INSERT INTO morpholog.deployment_identity
               (closed, root_actor, root_login, root_login_oid, closed_at, closed_by)
           VALUES (true, $1, $2, $3, now(), session_user)
           ON CONFLICT (singleton) DO UPDATE
               SET closed = true, root_actor = $1, root_login = $2, root_login_oid = $3,
                   closed_at = now(), closed_by = session_user
               WHERE NOT morpholog.deployment_identity.closed
           RETURNING true AS "closed!""#,
        root_actor.as_str(),
        root_login,
        oid as sqlx::postgres::types::Oid,
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(classify_checked_query)?;
    if closed.is_some() {
        tx.commit().await.map_err(crate::error::classify)?;
        return Ok(CloseOutcome::Closed);
    }
    let held = sqlx::query!(
        r#"SELECT root_actor AS "root_actor!", root_login AS "root_login!",
                  root_login_oid = $1 AS "same_oid!"
           FROM morpholog.deployment_identity"#,
        oid as sqlx::postgres::types::Oid,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(classify_checked_query)?;
    tx.rollback().await.map_err(crate::error::classify)?;
    if held.root_actor == root_actor.as_str() && held.root_login == root_login && held.same_oid {
        Ok(CloseOutcome::AlreadyClosed)
    } else {
        Err(PgError::DeploymentRootDiffers {
            root_actor: held.root_actor,
            root_login: held.root_login,
        })
    }
}
