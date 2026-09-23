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
use sqlx::{Postgres, Transaction};

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
    if !restricted {
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
