//! The activation: the first ordered audit rows. The baseline, every
//! claim the table held at cutover with the transition that admitted
//! it, is written as chunk rows, and the activation record comes last,
//! fixing the legacy prefix by its size and Merkle root and the chunks
//! by their number and a digest over their leaf hashes. Every later row
//! replays from a state the log itself commits to.
//!
//! The rows are control records, not transitions of any programme. The
//! migration that introduces the order writes them, in that transaction,
//! so they are all of order 1: chunks at ordinals 0 to k-1, the record at
//! k. A database `init` creates is ordered from its first row, has no
//! legacy prefix, and so has none of them.

use morpholog_core::{EvalValue, Subject, TransformationName};
use sha2::{Digest as _, Sha256};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::attestation::AuditAttestation;
use crate::audit::{
    AuditPosition, AuditRow, Operation, OperationKind, REPLAY_CHUNK, list_audit_rows_page,
};
use crate::error::{PgError, classify_checked_query};
use crate::merkle::{Digest, Frontier, Hash, audit_leaf_hash};

/// The reserved names: a `.morph` identifier cannot contain a dot.
pub(crate) const ACTIVATE: &str = "morpholog.activate";
pub(crate) const BASELINE: &str = "morpholog.baseline";

/// The canonical hash of the programme the records name as their model,
/// `program morpholog_activation`, which declares nothing: no rule
/// decided anything. As `morpholog hash` prints it.
pub(crate) const ACTIVATION_MODEL_HASH: &str =
    "sha256:c3f04c0b2a8e587d1c5e9980d988619c74d64ba85ab7e9007fe19eda6064cbd4";

pub(crate) const PARAMETERS: [&str; 4] = [
    "legacy_tree_size",
    "legacy_root",
    "baseline_chunks",
    "baseline_digest",
];

/// A chunk holds at most this many claims, and at most this many bytes
/// of serialised operations, whichever bound comes first.
pub const CHUNK_CLAIMS: usize = 10_000;
pub const CHUNK_BYTES: usize = 4 << 20;

/// Claims fetched per statement while a chunk is built, at most; a
/// statement also stops at the chunk's byte bound, so a fetch never holds
/// more than one chunk's worth of claims whatever their width.
const FETCH: i64 = 1024;

/// The digest the record names for its baseline: over the chunk rows'
/// leaf hashes in ordinal order.
pub(crate) fn baseline_digest(chunks: &[Hash]) -> Digest {
    let mut hasher = Sha256::new();
    for leaf in chunks {
        hasher.update(leaf);
    }
    Digest::from_bytes(hasher.finalize().into())
}

/// The record's arguments over this legacy prefix and these chunks.
pub(crate) fn activation_arguments(
    legacy: &Frontier,
    chunks: u64,
    digest: &Digest,
) -> Vec<EvalValue> {
    arguments_over(legacy.len() as u64, legacy.root(), chunks, digest)
}

/// The record's arguments over a legacy prefix of `len` rows with this
/// root, and `chunks` baseline chunks with this digest.
pub(crate) fn arguments_over(len: u64, root: Hash, chunks: u64, digest: &Digest) -> Vec<EvalValue> {
    vec![
        EvalValue::Decimal(len.into()),
        EvalValue::Subject(Subject::from(Digest::from_bytes(root).to_string())),
        EvalValue::Decimal(chunks.into()),
        EvalValue::Subject(Subject::from(digest.to_string())),
    ]
}

/// Write the baseline chunks and the activation record over every row
/// already in the log and every claim in the table. The caller holds both
/// tables locked.
pub(crate) async fn write_activation_record(
    tx: &mut Transaction<'_, Postgres>,
) -> Result<(), PgError> {
    write_activation_record_with(tx, CHUNK_CLAIMS, CHUNK_BYTES).await
}

/// [`write_activation_record`] with the chunk bounds chosen, so a test
/// can make several chunks of a few claims.
#[doc(hidden)]
pub async fn write_activation_record_with(
    tx: &mut Transaction<'_, Postgres>,
    chunk_claims: usize,
    chunk_bytes: usize,
) -> Result<(), PgError> {
    let (mut legacy, mut cursor): (Frontier, Option<AuditPosition>) =
        crate::preparation::prepared_frontier(tx).await?;
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

    let login = sqlx::query!(
        r#"SELECT session_user::text AS "name!", r.oid::int8 AS "oid!"
           FROM pg_catalog.pg_roles r WHERE r.rolname = session_user"#
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(classify_checked_query)?;
    let writer = Writer {
        login: login.name,
        oid: u32::try_from(login.oid)
            .map_err(|_| PgError::InvalidState(format!("role oid {} is not a u32", login.oid)))?,
    };

    // One chunk in memory at a time: claims stream in by primary key, a
    // statement bringing at most a chunk's worth of bytes (its first row
    // always, so a claim too wide for any chunk is met and refused), and
    // go straight into the chunk being built.
    let mut chunks: Vec<Hash> = Vec::new();
    let mut chunk = Chunk::default();
    let mut after: Option<(String, Vec<u8>)> = None;
    loop {
        let (name, hash) = after.clone().unzip();
        let page = sqlx::query!(
            r#"SELECT predicate_name, arguments, arguments_hash, asserted_in
               FROM (SELECT predicate_name, arguments, arguments_hash, asserted_in,
                            row_number() OVER w AS n,
                            sum(octet_length(arguments::text)) OVER w AS bytes
                     FROM (SELECT predicate_name, arguments, arguments_hash, asserted_in
                           FROM morpholog.claims
                           WHERE (predicate_name, arguments_hash)
                                 > (coalesce($1, ''), coalesce($2, ''::bytea))
                           ORDER BY predicate_name, arguments_hash
                           LIMIT $3) page
                     WINDOW w AS (ORDER BY predicate_name, arguments_hash
                                  ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)) bounded
               WHERE n = 1 OR bytes <= $4
               ORDER BY n"#,
            name,
            hash,
            FETCH,
            chunk_bytes as i64,
        )
        .fetch_all(&mut **tx)
        .await
        .map_err(classify_checked_query)?;
        if page.is_empty() {
            break;
        }
        for claim in page {
            after = Some((claim.predicate_name.clone(), claim.arguments_hash));
            let op = Operation {
                op: OperationKind::Ins,
                predicate: claim.predicate_name.into(),
                args: serde_json::from_value(claim.arguments)?,
                asserted_in: Some(claim.asserted_in),
            };
            // The array's bytes: each operation and a separator, plus one,
            // since two brackets stand where the last separator would.
            let size = serde_json::to_vec(&op)?.len() + 1;
            if !chunk.operations.is_empty() && chunk.bytes + size + 1 > chunk_bytes {
                chunks.push(chunk.write(tx, &writer, chunks.len()).await?);
                chunk = Chunk::default();
            }
            if chunk.operations.is_empty() && size + 1 > chunk_bytes {
                return Err(PgError::InvalidState(format!(
                    "a claim of {} bytes does not fit a baseline chunk of {chunk_bytes} bytes",
                    size + 1
                )));
            }
            chunk.bytes += size;
            chunk.operations.push(op);
            if chunk.operations.len() == chunk_claims {
                chunks.push(chunk.write(tx, &writer, chunks.len()).await?);
                chunk = Chunk::default();
            }
        }
    }
    if !chunk.operations.is_empty() {
        chunks.push(chunk.write(tx, &writer, chunks.len()).await?);
    }

    let digest = baseline_digest(&chunks);
    let arguments = activation_arguments(&legacy, chunks.len() as u64, &digest);
    let placed = insert_control_row(
        tx,
        &writer,
        ACTIVATE,
        &serde_json::to_value(&arguments)?,
        &serde_json::to_value(PARAMETERS)?,
        &serde_json::Value::Array(Vec::new()),
    )
    .await?;
    expect_place(&placed, chunks.len())?;
    Ok(())
}

struct Writer {
    login: String,
    oid: u32,
}

impl Writer {
    fn attestation(&self) -> AuditAttestation {
        AuditAttestation::Migration {
            authenticated_by: self.login.clone(),
            authenticated_by_oid: self.oid,
        }
    }
}

#[derive(Default)]
struct Chunk {
    operations: Vec<Operation>,
    /// The serialised size of `operations` as an array, less one byte.
    bytes: usize,
}

impl Chunk {
    /// Insert the chunk as the row at order 1, ordinal `ordinal`, and
    /// return its leaf hash, from the row as the trigger placed it.
    async fn write(
        self,
        tx: &mut Transaction<'_, Postgres>,
        writer: &Writer,
        ordinal: usize,
    ) -> Result<Hash, PgError> {
        let placed = insert_control_row(
            tx,
            writer,
            BASELINE,
            &serde_json::Value::Array(Vec::new()),
            &serde_json::Value::Array(Vec::new()),
            &serde_json::to_value(&self.operations)?,
        )
        .await?;
        expect_place(&placed, ordinal)?;
        let row = AuditRow {
            transition_id: placed.transition_id,
            transformation_name: TransformationName::from(BASELINE),
            arguments: Vec::new(),
            actor: Subject::from(writer.login.clone()),
            invariant_epoch: 1,
            invariants_checked: Vec::new(),
            asserted_claims: Vec::new(),
            retracted_claims: Vec::new(),
            emitted_intents: Vec::new(),
            committed_at: placed.committed_at,
            attestation: Some(writer.attestation()),
            parameters: Some(Vec::new()),
            model_hash: Some(ACTIVATION_MODEL_HASH.to_string()),
            semantics_version: Some(morpholog_core::SEMANTICS_VERSION),
            drawn_subjects: Some(Vec::new()),
            audit_order: Some(placed.audit_order),
            act_ordinal: Some(placed.act_ordinal),
            ordered_at: Some(placed.ordered_at),
            operations: Some(self.operations),
        };
        Ok(audit_leaf_hash(&row)?)
    }
}

/// Where the trigger put a control row.
struct Placed {
    transition_id: Uuid,
    audit_order: u64,
    act_ordinal: u32,
    ordered_at: jiff::Timestamp,
    committed_at: jiff::Timestamp,
}

fn expect_place(placed: &Placed, ordinal: usize) -> Result<(), PgError> {
    if (placed.audit_order, placed.act_ordinal as usize) != (1, ordinal) {
        return Err(PgError::InvalidState(format!(
            "the activation expected its row at order 1, ordinal {ordinal}, but the trigger \
             placed it at order {}, ordinal {}",
            placed.audit_order, placed.act_ordinal
        )));
    }
    Ok(())
}

async fn insert_control_row(
    tx: &mut Transaction<'_, Postgres>,
    writer: &Writer,
    name: &str,
    arguments: &serde_json::Value,
    parameters: &serde_json::Value,
    operations: &serde_json::Value,
) -> Result<Placed, PgError> {
    let transition_id = Uuid::now_v7();
    // `ordered_at!`: the ordering trigger sets it on every row it places.
    let row = sqlx::query!(
        r#"INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents, attestation,
            parameters, model_hash, semantics_version, drawn_subjects, operations
         ) VALUES ($1, $2, $3, $4, 1, '[]', '[]', '[]', '[]', $5, $6, $7, $8, '[]', $9)
         RETURNING audit_order, act_ordinal, ordered_at AS "ordered_at!", committed_at"#,
        transition_id,
        name,
        arguments,
        serde_json::to_value(EvalValue::Subject(Subject::from(writer.login.clone())))?,
        serde_json::to_value(writer.attestation())?,
        parameters,
        ACTIVATION_MODEL_HASH,
        i64::from(morpholog_core::SEMANTICS_VERSION),
        operations,
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(classify_checked_query)?;
    Ok(Placed {
        transition_id,
        audit_order: row.audit_order.cast_unsigned(),
        act_ordinal: row.act_ordinal.cast_unsigned(),
        ordered_at: row.ordered_at.to_jiff(),
        committed_at: row.committed_at.to_jiff(),
    })
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

    #[test]
    fn the_digest_over_no_chunks_is_of_nothing() {
        assert_eq!(
            baseline_digest(&[]).to_string(),
            format!(
                "sha256:{}",
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
            )
        );
    }
}
