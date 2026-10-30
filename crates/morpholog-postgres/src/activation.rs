//! The activation record: the first ordered row of the audit log. It
//! fixes the legacy prefix, by its size and Merkle root, and the claims
//! that prefix left, with the transition that admitted each, so every
//! later row replays from a state the log itself commits to.
//!
//! It is a control record, not a transition of any programme. The
//! migration that introduces the order writes it, in that transaction.
//! A database `init` creates is ordered from its first row, has no
//! legacy prefix, and so has no record.

use morpholog_core::{EvalValue, Subject};
use sha2::{Digest as _, Sha256};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::attestation::AuditAttestation;
use crate::audit::{AuditPosition, Operation, OperationKind, REPLAY_CHUNK, list_audit_rows_page};
use crate::error::{PgError, classify_checked_query};
use crate::merkle::{Digest, Frontier, audit_leaf_hash};

/// The reserved name: a `.morph` identifier cannot contain a dot.
pub(crate) const ACTIVATE: &str = "morpholog.activate";

/// The canonical hash of the programme the record names as its model,
/// `program morpholog_activation`, which declares nothing: no rule
/// decided anything. As `morpholog hash` prints it.
pub(crate) const ACTIVATION_MODEL_HASH: &str =
    "sha256:c3f04c0b2a8e587d1c5e9980d988619c74d64ba85ab7e9007fe19eda6064cbd4";

pub(crate) const PARAMETERS: [&str; 3] = ["legacy_tree_size", "legacy_root", "baseline_digest"];

/// The digest the record's arguments name for its baseline.
pub(crate) fn baseline_digest(baseline: &[Operation]) -> Result<Digest, serde_json::Error> {
    Ok(Digest::from_bytes(
        Sha256::digest(serde_json::to_vec(baseline)?).into(),
    ))
}

/// The record's arguments over this legacy prefix and baseline.
pub(crate) fn activation_arguments(
    legacy: &Frontier,
    baseline: &[Operation],
) -> Result<Vec<EvalValue>, serde_json::Error> {
    Ok(vec![
        EvalValue::Decimal(legacy.len().into()),
        EvalValue::Subject(Subject::from(Digest::from_bytes(legacy.root()).to_string())),
        EvalValue::Subject(Subject::from(baseline_digest(baseline)?.to_string())),
    ])
}

/// Write the activation record over every row already in the log and
/// every claim in the table. The caller holds both tables locked.
pub(crate) async fn write_activation_record(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<(), PgError> {
    let mut legacy = Frontier::default();
    let mut cursor: Option<AuditPosition> = None;
    loop {
        let page = list_audit_rows_page(tx, cursor, None, REPLAY_CHUNK).await?;
        for row in &page {
            legacy.push(audit_leaf_hash(row)?);
        }
        match page.last() {
            Some(last) if page.len() as i64 == REPLAY_CHUNK => cursor = Some(last.position()),
            _ => break,
        }
    }

    let baseline: Vec<Operation> = sqlx::query!(
        "SELECT predicate_name, arguments, asserted_in FROM morpholog.claims
         ORDER BY predicate_name, arguments_hash"
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(classify_checked_query)?
    .into_iter()
    .map(|claim| {
        Ok(Operation {
            op: OperationKind::Ins,
            predicate: claim.predicate_name.into(),
            args: serde_json::from_value(claim.arguments)?,
            asserted_in: Some(claim.asserted_in),
        })
    })
    .collect::<Result<_, serde_json::Error>>()?;

    let login = sqlx::query!(
        r#"SELECT session_user::text AS "name!", r.oid::int8 AS "oid!"
           FROM pg_catalog.pg_roles r WHERE r.rolname = session_user"#
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(classify_checked_query)?;
    let oid = u32::try_from(login.oid)
        .map_err(|_| PgError::InvalidState(format!("role oid {} is not a u32", login.oid)))?;

    let arguments = activation_arguments(&legacy, &baseline)?;
    let attestation = AuditAttestation::Migration {
        authenticated_by: login.name.clone(),
        authenticated_by_oid: oid,
    };
    sqlx::query!(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents, attestation,
            parameters, model_hash, semantics_version, drawn_subjects, operations
         ) VALUES ($1, $2, $3, $4, 1, '[]', '[]', '[]', '[]', $5, $6, $7, $8, '[]', $9)",
        Uuid::now_v7(),
        ACTIVATE,
        serde_json::to_value(&arguments)?,
        serde_json::to_value(EvalValue::Subject(Subject::from(login.name)))?,
        serde_json::to_value(&attestation)?,
        serde_json::to_value(PARAMETERS)?,
        ACTIVATION_MODEL_HASH,
        i64::from(morpholog_core::SEMANTICS_VERSION),
        serde_json::to_value(&baseline)?,
    )
    .execute(&mut **tx)
    .await
    .map_err(classify_checked_query)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_hash_is_the_empty_programmes() {
        let empty = morpholog_core::ir_builder::program("morpholog_activation").build();
        assert_eq!(
            morpholog_core::format::canonical_hash(&empty),
            ACTIVATION_MODEL_HASH
        );
    }
}
