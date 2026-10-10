//! What one proposal left behind, observed the same way on every route,
//! so two routes can be held to one decision: outcome, reason, rule,
//! witness variables, the rejection log, and the persisted rows with
//! generated identities normalised.

#![allow(dead_code)]

use morpholog_core::{ClaimInstance, EvalError, Transition};
use morpholog_postgres::{
    PgError, PgPool, PgProgram, PgProposalOutcome, list_rejection_rows, propose_against_pg,
};
use morpholog_test_support::differential::normalize_uuids;

use super::{attested, reset_db, seed_claims};

pub async fn count(pool: &PgPool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

/// Rows as sorted text with generated identities normalised, so two runs
/// that minted different subjects compare equal. Audit rows drop their
/// transition id and time, claims the transition that asserted them,
/// outbox rows their ids and the key derived from one.
pub async fn persisted(pool: &PgPool, sql: &'static str) -> Vec<String> {
    let rows: Vec<String> = sqlx::query_scalar(sql).fetch_all(pool).await.unwrap();
    let mut rows: Vec<String> = rows.iter().map(|r| normalize_uuids(r)).collect();
    rows.sort();
    rows
}

pub const AUDIT_ROWS: &str = "SELECT jsonb_build_object(
        'transformation', transformation_name, 'arguments', arguments, 'actor', actor,
        'epoch', invariant_epoch, 'checked', invariants_checked,
        'asserted', asserted_claims, 'retracted', retracted_claims,
        'emitted', emitted_intents, 'attestation', attestation, 'parameters', parameters
    )::text FROM morpholog.audit";

pub const CLAIM_ROWS: &str =
    "SELECT jsonb_build_object('predicate', predicate_name, 'arguments', arguments)::text
     FROM morpholog.claims";

pub const OUTBOX_ROWS: &str =
    "SELECT jsonb_build_object('intent', intent_type, 'arguments', arguments)::text
     FROM morpholog.outbox";

/// What one proposal left behind that both routes must agree on.
#[derive(Debug, PartialEq, Eq)]
pub struct Observed {
    pub outcome: String,
    pub rejection: Option<String>,
    pub audit: Vec<String>,
    pub claims: Vec<String>,
    pub outbox: Vec<String>,
}

/// A route's answer, typed: a decision with what it left behind, the
/// kernel's own evaluation error, or an operational failure.
#[derive(Debug, PartialEq, Eq)]
pub enum RouteObservation {
    Decided(Observed),
    Kernel(EvalError),
    Operational(String),
}

pub async fn observe(
    pool: &PgPool,
    program: &PgProgram,
    seeded: &[ClaimInstance],
    transition: &Transition,
) -> RouteObservation {
    reset_db(pool).await;
    // A sampled state may hold a claim twice; the table holds it once.
    let mut seeded: Vec<ClaimInstance> = seeded.to_vec();
    seeded.sort_by_key(|c| format!("{c:?}"));
    seeded.dedup();
    seed_claims(pool, &seeded).await;
    let outcome = match propose_against_pg(pool, program, &attested(transition)).await {
        Ok(outcome) => outcome,
        Err(PgError::Kernel(e)) => return RouteObservation::Kernel(e),
        Err(e) => return RouteObservation::Operational(format!("{e:?}")),
    };
    let outcome = match outcome {
        PgProposalOutcome::Committed {
            asserted_claims,
            retracted_claims,
            emitted_intents,
            ..
        } => normalize_uuids(&format!(
            "committed +{asserted_claims:?} -{retracted_claims:?} !{emitted_intents:?}"
        )),
        PgProposalOutcome::Rejected {
            reason,
            rule,
            witness,
            ..
        } => {
            let vars: Vec<_> = witness.iter().map(|w| w.var.to_string()).collect();
            format!("rejected {reason} | rule {rule:?} | witness vars {vars:?}")
        }
    };
    let rejection = list_rejection_rows(pool, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|r| {
            let vars: Vec<_> = r
                .witness
                .iter()
                .flatten()
                .map(|w| w.var.to_string())
                .collect();
            normalize_uuids(&format!(
                "{} {:?} {:?} {} {} {:?} {} {vars:?}",
                r.transformation_name,
                r.arguments,
                r.actor,
                r.kind,
                r.rule,
                r.invariant_version,
                r.reason
            ))
        })
        .reduce(|a, b| format!("{a}\n{b}"));
    RouteObservation::Decided(Observed {
        outcome,
        rejection,
        audit: persisted(pool, AUDIT_ROWS).await,
        claims: persisted(pool, CLAIM_ROWS).await,
        outbox: persisted(pool, OUTBOX_ROWS).await,
    })
}
