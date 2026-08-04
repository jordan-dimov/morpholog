//! Which database role may propose as which actor.
//!
//! Gateway attestation records the PostgreSQL login role that vouched
//! for an actor, so audit reads honestly as "role R asserted that
//! Jordan did this". Nothing in that arrangement stops role R
//! asserting ANY actor label - which quietly empties every rule
//! written about distinct people, because one connection can play
//! both of them.
//!
//! Two claims close that, and they are deliberately separate:
//!
//! - `ActorAssertionRestricted(actor)` is the POLICY. While no such
//!   claim is admitted for an actor, anyone may assert it, exactly as
//!   before - the policy ARMS WHEN THE CLAIM IS ADMITTED, never
//!   merely by the predicate being declared, so a programme that
//!   adopts these predicates does not thereby restrict every actor in
//!   it, and a deployment that admits nothing keeps working.
//! - `ActorAssertionAuthority(actor, login_role)` is the GRANT: this
//!   login role may assert this actor.
//!
//! Arming by the grants alone would be simpler and wrong. Retracting
//! the last grant would return the actor to unrestricted at exactly
//! the moment someone is revoking access - a silent downgrade in the
//! middle of an incident. Here, retracting the last grant LOCKS THE
//! ACTOR OUT; getting back to unrestricted means retracting the
//! policy claim, which is its own governed, visible act.
//!
//! Both are ordinary claims the operator declares and governs through
//! their own transformations under their own authority gates. The
//! runtime only recognises the shape, exactly as it does for
//! `AuditSigningKey`.
//!
//! **What this does and does not protect.** The check runs in the
//! adapter, so it binds callers whose actor input passes through
//! Morpholog. It is not a defence against a compromised gateway
//! process: the runtime's writer role holds `INSERT`/`DELETE` on
//! `morpholog.claims` and `INSERT` on `morpholog.audit`, so code
//! holding those credentials can write claims and attestation-shaped
//! audit rows directly, without passing here at all. Two verifier
//! identities are genuinely distinct only when the two gateways and
//! their credentials are genuinely separate. This is adapter-enforced
//! actor-assertion policy, not proof of authorship.

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
/// This matters more than the equivalent mistake for `AuditSigningKey`,
/// which fails loudly the moment someone tries to sign. A misshapen
/// policy declaration fails the other way: the runtime would simply
/// never match it, the intended restriction would never arm, and
/// everything would look fine. Refusing the programme is the only
/// point at which that is visible.
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

/// Decide whether `login_role` may propose as `actor`, reading the
/// policy inside the caller's own transaction so the answer is the one
/// in force in the snapshot the kernel is about to evaluate against.
///
/// Unrestricted actors cost one indexed lookup that finds nothing.
pub(crate) async fn authorise(
    tx: &mut Transaction<'_, Postgres>,
    actor: &Subject,
    login_role: &str,
) -> Result<(), PgError> {
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
