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

use morpholog_core::{ClaimInstance, EvalValue, PredicateArgKind, Program, Subject, Transition};
use sqlx::postgres::types::Oid;
use sqlx::{PgPool, Postgres, Transaction};

use crate::error::PgError;
use crate::error::classify_checked_query;

/// The policy claim: this actor may only be asserted by an authorised
/// login role.
pub const RESTRICTED_PREDICATE: &str = "ActorAssertionRestricted";

/// The grant claim: this login role may assert this actor.
pub const AUTHORITY_PREDICATE: &str = "ActorAssertionAuthority";

/// The kind claim: this actor is of this kind, at most one per actor.
pub(crate) const KIND_PREDICATE: &str = "ActorKind";

/// The kind grant: this login role may assert any actor of this kind.
pub(crate) const KIND_AUTHORITY_PREDICATE: &str = "ActorKindAuthority";

/// The registrar: this actor may enrol actors of this kind, each one only
/// in the act that mints it.
pub(crate) const KIND_ENROLMENT_PREDICATE: &str = "ActorKindEnrolment";

/// Every reserved name: on a closed deployment only the root, or a
/// registrar enrolling a new actor, changes a claim under one.
const POLICY_PREDICATES: [&str; 5] = [
    RESTRICTED_PREDICATE,
    AUTHORITY_PREDICATE,
    KIND_PREDICATE,
    KIND_AUTHORITY_PREDICATE,
    KIND_ENROLMENT_PREDICATE,
];

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
    let expected: [(&str, usize); 5] = [
        (RESTRICTED_PREDICATE, 1),
        (AUTHORITY_PREDICATE, 2),
        (KIND_PREDICATE, 2),
        (KIND_AUTHORITY_PREDICATE, 2),
        (KIND_ENROLMENT_PREDICATE, 2),
    ];
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
    // programme, so the facades' declaration check does not cover it. A
    // misshapen grant, kind or registrar never matches either, which
    // leaves an actor refused, so those are not read here.
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
                "{malformed} admitted claim(s) under `{RESTRICTED_PREDICATE}` or \
                 `{AUTHORITY_PREDICATE}` do not have the shape the runtime matches, \
                 so the restriction they look like would protect nothing"
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

    let actor_value = subject_value(actor.as_str());
    let login_value = subject_value(login_role);
    if !identity.closed
        && !claim_held(tx, RESTRICTED_PREDICATE, serde_json::json!([actor_value])).await?
    {
        return Ok(());
    }
    let grant = serde_json::json!([actor_value, login_value]);
    if claim_held(tx, AUTHORITY_PREDICATE, grant).await? {
        return Ok(());
    }
    // A kind counts on a closed deployment only, where its claims are
    // the root's or a registrar's to write. The login's kind grants are
    // few; each is looked up by the claims table's key.
    if identity.closed {
        let by_kind = sqlx::query_scalar!(
            r#"SELECT EXISTS (
                   SELECT 1 FROM morpholog.claims g
                   WHERE g.predicate_name = $1 AND g.arguments -> 1 = $2
                     AND EXISTS (
                         SELECT 1 FROM morpholog.claims k
                         WHERE k.predicate_name = $3
                           AND k.arguments_hash = morpholog.claim_digest(
                                   jsonb_build_array($4::jsonb, g.arguments -> 0))
                           AND k.arguments = jsonb_build_array($4::jsonb, g.arguments -> 0))
               ) AS "by_kind!""#,
            KIND_AUTHORITY_PREDICATE,
            login_value,
            KIND_PREDICATE,
            actor_value,
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(classify_checked_query)?;
        if by_kind {
            return Ok(());
        }
    }
    Err(PgError::ActorAssertionUnauthorised {
        actor: actor.clone(),
        login_role: login_role.to_string(),
    })
}

fn subject_value(value: &str) -> serde_json::Value {
    serde_json::json!({"type": "subject", "value": value})
}

/// Whether `predicate(arguments)` is admitted, found by the claims
/// table's key rather than by reading every claim under `predicate`.
async fn claim_held(
    tx: &mut Transaction<'_, Postgres>,
    predicate: &str,
    arguments: serde_json::Value,
) -> Result<bool, PgError> {
    sqlx::query_scalar!(
        r#"SELECT EXISTS (
               SELECT 1 FROM morpholog.claims
               WHERE predicate_name = $1
                 AND arguments_hash = morpholog.claim_digest($2::jsonb)
                 AND arguments = $2
           ) AS "held!""#,
        predicate,
        arguments,
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(classify_checked_query)
}

/// The `ActorKind` claims `actor` holds. Reads every kind claim, so it is
/// asked only when the root classifies an existing actor, or at closing.
async fn kinds_of(tx: &mut Transaction<'_, Postgres>, actor: &Subject) -> Result<i64, PgError> {
    sqlx::query_scalar!(
        r#"SELECT count(*) AS "kinds!" FROM morpholog.claims
           WHERE predicate_name = $1 AND arguments -> 0 = $2"#,
        KIND_PREDICATE,
        subject_value(actor.as_str()),
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(classify_checked_query)
}

/// On a closed deployment, refuse a decided delta that changes identity
/// policy without the authority to: only the root changes a claim under a
/// reserved name, except that a registrar holding
/// `ActorKindEnrolment(kind, registrar)` may admit `ActorKind(new, kind)`
/// for a subject this same act drew. Nor may any act leave an actor with
/// two kinds. The delta is what the act would write, so no branch of a
/// transformation, nor a later act of a `transact`, writes policy
/// unchecked. An open deployment leaves policy writes to the programme's
/// gates, as before.
pub(crate) async fn authorise_policy_writes(
    tx: &mut Transaction<'_, Postgres>,
    transition: &Transition,
    drawn: &[Subject],
    asserted: &[ClaimInstance],
    retracted: &[ClaimInstance],
) -> Result<(), PgError> {
    let is_policy = |c: &&ClaimInstance| POLICY_PREDICATES.contains(&c.predicate.as_str());
    if !asserted.iter().chain(retracted).any(|c| is_policy(&c)) {
        return Ok(());
    }
    let identity = sqlx::query!(
        r#"SELECT closed AS "closed!", root_actor FROM morpholog.deployment_identity"#
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(classify_checked_query)?
    .ok_or(PgError::DeploymentIdentityMissing)?;
    if !identity.closed {
        return Ok(());
    }
    let root = identity.root_actor.as_deref() == Some(transition.actor.as_str());
    if !root {
        if let Some(claim) = retracted.iter().find(is_policy) {
            return Err(policy_write_refused(transition, claim, "retract"));
        }
        let registrar = subject_value(transition.actor.as_str());
        for claim in asserted.iter().filter(is_policy) {
            let enrolment = match (claim.predicate.as_str(), claim.args.as_slice()) {
                (KIND_PREDICATE, [EvalValue::Subject(new), EvalValue::Subject(kind)])
                    if drawn.contains(new) =>
                {
                    serde_json::json!([subject_value(kind.as_str()), registrar])
                }
                _ => return Err(policy_write_refused(transition, claim, "admit")),
            };
            if !claim_held(tx, KIND_ENROLMENT_PREDICATE, enrolment).await? {
                return Err(policy_write_refused(transition, claim, "admit"));
            }
        }
    }
    one_kind_each(tx, drawn, asserted, retracted).await
}

/// Refuse a delta that would leave an actor with two kinds: either kind's
/// logins could then assert it. A subject this act drew has no kind yet.
async fn one_kind_each(
    tx: &mut Transaction<'_, Postgres>,
    drawn: &[Subject],
    asserted: &[ClaimInstance],
    retracted: &[ClaimInstance],
) -> Result<(), PgError> {
    let kind_of = |c: &ClaimInstance| match (c.predicate.as_str(), c.args.first()) {
        (KIND_PREDICATE, Some(EvalValue::Subject(actor))) => Some(actor.clone()),
        _ => None,
    };
    let mut classified: Vec<Subject> = Vec::new();
    for actor in asserted.iter().filter_map(kind_of) {
        if !classified.contains(&actor) {
            classified.push(actor);
        }
    }
    for actor in classified {
        let count = |claims: &[ClaimInstance]| {
            i64::try_from(
                claims
                    .iter()
                    .filter(|c| kind_of(c).as_ref() == Some(&actor))
                    .count(),
            )
            .unwrap_or(i64::MAX)
        };
        let held = if drawn.contains(&actor) {
            0
        } else {
            kinds_of(tx, &actor).await?
        };
        let kinds = held - count(retracted) + count(asserted);
        if kinds > 1 {
            return Err(PgError::ActorKindAmbiguous { actor, kinds });
        }
    }
    Ok(())
}

fn policy_write_refused(transition: &Transition, claim: &ClaimInstance, verb: &str) -> PgError {
    PgError::PolicyWriteUnauthorised {
        actor: transition.actor.clone(),
        write: format!("{verb} {}", claim.predicate),
    }
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
) -> Result<(), PgError> {
    let mut tx = pool.begin().await.map_err(crate::error::classify)?;
    let oid = root_login_oid(&mut tx, root_login).await?;
    // Closed, a kind grant reaches every actor of its kind; one that holds
    // two would be reachable by both kinds' logins.
    let doubled = sqlx::query!(
        r#"SELECT arguments -> 0 ->> 'value' AS "actor!", count(*) AS "kinds!"
           FROM morpholog.claims WHERE predicate_name = $1
           GROUP BY 1 HAVING count(*) > 1 ORDER BY 1 LIMIT 1"#,
        KIND_PREDICATE,
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(classify_checked_query)?;
    if let Some(doubled) = doubled {
        return Err(PgError::ActorKindAmbiguous {
            actor: Subject::from(doubled.actor),
            kinds: doubled.kinds,
        });
    }
    if write_root(&mut tx, root_actor, root_login, oid).await? {
        return tx.commit().await.map_err(crate::error::classify);
    }
    let held = sqlx::query!(
        r#"SELECT root_actor AS "root_actor!", root_login AS "root_login!",
                  root_login_oid = $1 AS "same_oid!"
           FROM morpholog.deployment_identity"#,
        oid as Oid,
    )
    .fetch_one(&mut *tx)
    .await
    .map_err(classify_checked_query)?;
    tx.rollback().await.map_err(crate::error::classify)?;
    if held.root_actor == root_actor.as_str() && held.root_login == root_login && held.same_oid {
        Ok(())
    } else {
        Err(PgError::DeploymentRootDiffers {
            root_actor: held.root_actor,
            root_login: held.root_login,
        })
    }
}

/// The OID of `login`, which must exist and be able to log in: a root no
/// client can authenticate as would close the deployment to everyone.
pub(crate) async fn root_login_oid(
    tx: &mut Transaction<'_, Postgres>,
    login: &str,
) -> Result<Oid, PgError> {
    let role = sqlx::query!(
        r#"SELECT oid AS "oid!: Oid", rolcanlogin AS "can_login!"
           FROM pg_roles WHERE rolname = $1"#,
        login,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(classify_checked_query)?
    .ok_or_else(|| PgError::RootLoginMissing(login.to_string()))?;
    if !role.can_login {
        return Err(PgError::RootLoginCannotLogIn(login.to_string()));
    }
    Ok(role.oid)
}

/// Write the root into the identity row unless it is closed already;
/// whether it wrote.
pub(crate) async fn write_root(
    tx: &mut Transaction<'_, Postgres>,
    root_actor: &Subject,
    root_login: &str,
    oid: Oid,
) -> Result<bool, PgError> {
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
        oid as Oid,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(classify_checked_query)?;
    Ok(closed.is_some())
}
