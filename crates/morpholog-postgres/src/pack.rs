//! Evidence packs: a portable, offline-verifiable export of a prefix of
//! the audit log.
//!
//! A pack carries the covered audit rows, the checkpoint chain up to a
//! covering checkpoint, and a thin manifest. A third party recomputes the
//! Merkle root from the rows and checks it against the checkpoints - with
//! no database access - and, against an externally-held anchor, catches a
//! coordinated rewrite.
//!
//! **Honest v1 boundary.** A pack is a *complete prefix* of the log. It
//! proves that exported prefix is genuine and matches an anchor that left
//! the database. It is not yet logarithmic inclusion/consistency proofs,
//! selective disclosure, or subject-scoped - those need the Certificate
//! Transparency proof APIs / a subject-indexed commitment (deferred).

use serde::{Deserialize, Serialize};
use sqlx::PgPool;

use crate::audit::{AuditRow, REPLAY_CHUNK, list_audit_rows_page};
use crate::checkpoints::{
    Checkpoint, TreeVerification, checkpoint_hash, load_checkpoint_chain, same_tree_head,
    signature_crypto_violation, verify_tree,
};
use crate::error::{PgError, classify};
use crate::merkle::{
    Hash, ProofError, audit_leaf_hash, consistency_proof, inclusion_proof, parse_hash, render_hash,
    verify_consistency_proof, verify_inclusion_proof,
};
use crate::txn::{TxIsolation, begin_isolated_tx};

const PACK_FORMAT_V1: u32 = 1;
const PACK_FORMAT_V2: u32 = 2;
const PACK_KIND_WINDOW: &str = "window";

/// A thin convenience header on the pack. The authoritative data is
/// `checkpoints` + `rows`; the manifest just summarises the covering
/// checkpoint for a human reading the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackManifest {
    pub pack_format_version: u32,
    pub tree_size: i64,
    pub root_hash: String,
    pub checkpoint_hash: String,
}

/// A portable evidence pack: everything an offline verifier needs to
/// recompute and check the covered prefix of the audit log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidencePack {
    pub manifest: PackManifest,
    /// The checkpoint chain up to and including the covering checkpoint.
    pub checkpoints: Vec<Checkpoint>,
    /// The covered audit rows; carried whole, so the verifier recomputes
    /// each leaf with the same encoding the live runtime used.
    pub rows: Vec<AuditRow>,
}

/// Export a complete-prefix evidence pack covering a checkpoint (the
/// latest, or the one at `tree_size` if given). Reads under `SERIALIZABLE
/// READ ONLY DEFERRABLE`. Errors if there is no such checkpoint.
pub async fn export_pack(pool: &PgPool, tree_size: Option<i64>) -> Result<EvidencePack, PgError> {
    let mut tx = begin_isolated_tx(pool, TxIsolation::SerializableReadOnlyDeferrable).await?;

    let mut checkpoints = load_checkpoint_chain(&mut tx).await?;

    let covering = match tree_size {
        Some(n) => checkpoints.iter().find(|c| c.tree_size == n).cloned(),
        None => checkpoints.last().cloned(),
    };
    let Some(covering) = covering else {
        return Err(PgError::NoCheckpoint);
    };
    checkpoints.retain(|c| c.tree_size <= covering.tree_size);

    // The first `covering.tree_size` rows in canonical order.
    let mut rows: Vec<AuditRow> = Vec::new();
    let mut cursor = None;
    while (rows.len() as i64) < covering.tree_size {
        let page = list_audit_rows_page(&mut tx, cursor, None, REPLAY_CHUNK).await?;
        if page.is_empty() {
            break;
        }
        for row in page {
            cursor = Some((row.committed_at, row.transition_id));
            rows.push(row);
            if (rows.len() as i64) >= covering.tree_size {
                break;
            }
        }
    }
    // The checkpoint is watermark-bounded, so its rows should all be
    // present and visible. Fewer means the audit log was edited under the
    // checkpoint - fail loudly rather than emit a pack the verifier would
    // (rightly) reject as malformed.
    if rows.len() as i64 != covering.tree_size {
        return Err(PgError::InvalidState(format!(
            "checkpoint commits to {} audit rows but only {} were present",
            covering.tree_size,
            rows.len()
        )));
    }
    tx.commit().await.map_err(classify)?;

    Ok(EvidencePack {
        manifest: PackManifest {
            pack_format_version: PACK_FORMAT_V1,
            tree_size: covering.tree_size,
            root_hash: covering.root_hash,
            checkpoint_hash: covering.checkpoint_hash,
        },
        checkpoints,
        rows,
    })
}

/// A pack that is not a well-formed v1 artefact, kept distinct from a
/// cryptographic [`TreeVerification`] verdict: a malformed pack never had
/// a chance to prove anything, where `Tampered` / `ChainBroken` /
/// `AnchorMismatch` are genuine divergences of a well-formed one.
#[derive(Debug, thiserror::Error)]
pub enum PackError {
    /// The pack violates a v1 envelope rule (empty or non-canonical
    /// checkpoint chain, wrong row count, duplicate row coordinates, or a
    /// manifest that disagrees with the covering checkpoint).
    #[error("malformed evidence pack: {detail}")]
    Malformed { detail: String },
    /// A row could not be re-encoded to recompute its leaf hash.
    #[error("could not recompute a leaf hash from the pack: {0}")]
    Encoding(#[from] serde_json::Error),
}

/// Verify an evidence pack offline - no database. First validates the v1
/// envelope (the offline verifier cannot lean on the database constraints
/// the live one does, so the pack is treated as hostile input), then
/// recomputes the leaf hashes from the rows (re-sorted into canonical
/// `(committed_at, transition_id)` order, so serialization order cannot
/// matter) and runs the shared `verify_tree` crypto core over the pack's
/// checkpoints, optionally against an externally-held anchor.
pub fn verify_pack(
    pack: &EvidencePack,
    anchor: Option<&Checkpoint>,
) -> Result<TreeVerification, PackError> {
    validate_envelope(pack)?;

    // Owned + canonically sorted: the leaves are computed from this order
    // and the authority check folds the same rows, so live and offline
    // resolve signing keys from one ordering.
    let mut rows = pack.rows.clone();
    rows.sort_by_key(|a| (a.committed_at, a.transition_id));
    for pair in rows.windows(2) {
        if (pair[0].committed_at, pair[0].transition_id)
            == (pair[1].committed_at, pair[1].transition_id)
        {
            return Err(PackError::Malformed {
                detail: format!(
                    "two rows share coordinates ({}, {})",
                    pair[0].committed_at, pair[0].transition_id
                ),
            });
        }
    }

    let leaves: Vec<Hash> = rows.iter().map(audit_leaf_hash).collect::<Result<_, _>>()?;
    let verdict = verify_tree(&leaves, &pack.checkpoints, anchor);
    // A genuinely-signed intact pack still has to answer the authority
    // question, offline, from its own rows: was each signing key admitted
    // as of its checkpoint's prefix? The supplied anchor is judged the same.
    let signed = |c: &Checkpoint| !c.signatures.is_empty();
    if matches!(verdict, TreeVerification::Intact { .. })
        && (pack.checkpoints.iter().any(signed) || anchor.is_some_and(signed))
        && let Some(violation) =
            crate::checkpoints::authority_violation(&pack.checkpoints, anchor, &rows)
    {
        return Ok(violation);
    }
    Ok(verdict)
}

/// The v1 envelope rules a well-formed pack must satisfy before its
/// cryptographic core is even worth checking. Stricter than the live
/// verifier on purpose: a pack is untrusted JSON, not a database read.
fn validate_envelope(pack: &EvidencePack) -> Result<(), PackError> {
    let malformed = |detail: String| PackError::Malformed { detail };

    let Some(covering) = pack.checkpoints.last() else {
        return Err(malformed("the checkpoint chain is empty".into()));
    };
    // No negative checkpoint size: the database enforces `tree_size >= 0`,
    // but a pack is hostile JSON, so the offline verifier rejects what the
    // runtime could never produce rather than indexing with it.
    if let Some(bad) = pack.checkpoints.iter().find(|c| c.tree_size < 0) {
        return Err(malformed(format!(
            "checkpoint tree_size is negative: {}",
            bad.tree_size
        )));
    }
    // Canonical, strictly increasing checkpoint sizes: a forged pack must
    // not carry duplicate or out-of-order checkpoints the runtime could
    // never produce. The covering checkpoint is therefore the last one.
    for pair in pack.checkpoints.windows(2) {
        if pair[1].tree_size <= pair[0].tree_size {
            return Err(malformed(format!(
                "checkpoint sizes are not strictly increasing: {} then {}",
                pair[0].tree_size, pair[1].tree_size
            )));
        }
    }
    // A v1 pack is a COMPLETE checkpointed prefix - exactly the rows the
    // covering checkpoint commits to, no more (extra rows would otherwise
    // ride along unproven past the last checkpoint) and no fewer.
    if pack.rows.len() as i64 != covering.tree_size {
        return Err(malformed(format!(
            "pack carries {} rows but the covering checkpoint commits to {}",
            pack.rows.len(),
            covering.tree_size
        )));
    }
    // The manifest is non-authoritative but must not lie: a human or script
    // reads it even though the crypto core ignores it.
    let m = &pack.manifest;
    if m.pack_format_version != PACK_FORMAT_V1 {
        return Err(malformed(format!(
            "unsupported pack_format_version {}",
            m.pack_format_version
        )));
    }
    if m.tree_size != covering.tree_size
        || m.root_hash != covering.root_hash
        || m.checkpoint_hash != covering.checkpoint_hash
    {
        return Err(malformed(
            "manifest disagrees with the covering checkpoint".into(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Window evidence packs (the Certificate Transparency proof tier).
//
// A window pack proves two separate things about an interval `[from, to)` of
// the audit log, and needs both because neither implies the other:
//   1. the later checkpoint is an append-only extension of the earlier one
//      (a consistency proof - the prior period was not rewritten), and
//   2. each exported row is included at its declared position in the later
//      checkpoint (per-row inclusion proofs - the rows are the real suffix).
// A consistency proof alone verifies between two roots regardless of any
// rows; an inclusion proof alone says nothing about append-only continuity.
//
// A window pack carries only the `[from, to)` rows, so - unlike a full
// prefix pack - it CANNOT establish governed signing-key *authority* (that
// needs the `[0, from)` rows to fold `AuditSigningKey` claims as of the
// prefix). It checks checkpoint signatures cryptographically only; authority
// remains a full-prefix property.
// ---------------------------------------------------------------------------

/// One exported row's inclusion proof: the row sits at `leaf_index` in the
/// to-checkpoint's tree, proven by `proof` (rendered sibling hashes,
/// leaf-to-root).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RowInclusionProof {
    pub leaf_index: i64,
    pub proof: Vec<String>,
}

/// The window pack's convenience header. The authoritative data is the two
/// checkpoints, the consistency proof, the rows, and the inclusion proofs;
/// the manifest just restates the two endpoints for a human reading the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowPackManifest {
    pub pack_format_version: u32,
    pub pack_kind: String,
    pub from_tree_size: i64,
    pub to_tree_size: i64,
    pub from_checkpoint_hash: String,
    pub to_checkpoint_hash: String,
    pub from_root_hash: String,
    pub to_root_hash: String,
}

/// A windowed evidence pack: everything an offline verifier needs to confirm
/// the interval `[from_tree_size, to_tree_size)` is a faithful, contiguous,
/// append-only continuation of the earlier (anchor) checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowEvidencePack {
    pub manifest: WindowPackManifest,
    pub from_checkpoint: Checkpoint,
    pub to_checkpoint: Checkpoint,
    /// RFC 6962 consistency proof from `from` to `to` (rendered hashes).
    pub consistency_proof: Vec<String>,
    /// The window rows, in canonical order: exactly `to - from` of them.
    pub rows: Vec<AuditRow>,
    /// One inclusion proof per window row, declaring its leaf index.
    pub inclusion_proofs: Vec<RowInclusionProof>,
}

/// The verdict of verifying a window pack. Kept separate from the
/// prefix-shaped [`TreeVerification`]: a window proves consistency +
/// row-inclusion, not a recomputed-from-every-row prefix, so its honest
/// failure modes differ.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WindowVerification {
    /// The later checkpoint extends the earlier (consistency holds), every
    /// window row is included at its declared position, and the
    /// to-checkpoint signatures (if any) verify cryptographically.
    Intact {
        from_tree_size: i64,
        to_tree_size: i64,
        rows: usize,
    },
    /// The later checkpoint is not an append-only extension of the earlier
    /// one - the prior period was altered.
    InconsistentExtension {
        from_tree_size: i64,
        to_tree_size: i64,
    },
    /// A window row is not included at its declared position in the later
    /// checkpoint - the exported rows are not the genuine suffix.
    RowNotIncluded { leaf_index: i64 },
    /// An externally held anchor disagrees with the pack's from-checkpoint.
    AnchorMismatch {
        tree_size: i64,
        anchor_checkpoint_hash: String,
        pack_checkpoint_hash: String,
    },
    /// The to-checkpoint carries a signature that does not verify over its
    /// tree head (cryptographic check only; authority is not judged here).
    SignatureInvalid {
        tree_size: i64,
        key_id: String,
        purpose: String,
        public_key: String,
    },
    /// `--require-signatures` was asked for and the to-checkpoint carries no
    /// signature. A policy verdict the verifier opts into (REMIT attribution
    /// wants a signed window end), not an intrinsic tamper.
    SignatureRequired { tree_size: i64 },
    /// The window pack is not a well-formed v2 artefact (a bad envelope or
    /// unparseable JSON) - it never had a chance to prove anything, kept
    /// distinct from a genuine divergence of a well-formed pack.
    Malformed { detail: String },
}

/// Export a windowed evidence pack between two existing checkpoints. Reads
/// under `SERIALIZABLE READ ONLY DEFERRABLE`; errors if either endpoint is
/// not an existing checkpoint or `from` is not strictly before `to`.
/// `to_tree_size` defaults to the latest checkpoint.
pub async fn export_window(
    pool: &PgPool,
    from_tree_size: i64,
    to_tree_size: Option<i64>,
) -> Result<WindowEvidencePack, PgError> {
    let mut tx = begin_isolated_tx(pool, TxIsolation::SerializableReadOnlyDeferrable).await?;
    let checkpoints = load_checkpoint_chain(&mut tx).await?;

    let to_checkpoint = match to_tree_size {
        Some(n) => checkpoints.iter().find(|c| c.tree_size == n).cloned(),
        None => checkpoints.last().cloned(),
    };
    let Some(to_checkpoint) = to_checkpoint else {
        return Err(PgError::NoCheckpoint);
    };
    let Some(from_checkpoint) = checkpoints
        .iter()
        .find(|c| c.tree_size == from_tree_size)
        .cloned()
    else {
        return Err(PgError::NoCheckpoint);
    };
    if from_checkpoint.tree_size >= to_checkpoint.tree_size {
        return Err(PgError::InvalidState(format!(
            "window from tree_size {} must be strictly before to tree_size {}",
            from_checkpoint.tree_size, to_checkpoint.tree_size
        )));
    }

    // The prover needs the whole `[0, to)` prefix to build the consistency
    // proof and the per-row inclusion paths.
    let to_size = to_checkpoint.tree_size;
    let mut rows: Vec<AuditRow> = Vec::new();
    let mut cursor = None;
    while (rows.len() as i64) < to_size {
        let page = list_audit_rows_page(&mut tx, cursor, None, REPLAY_CHUNK).await?;
        if page.is_empty() {
            break;
        }
        for row in page {
            cursor = Some((row.committed_at, row.transition_id));
            rows.push(row);
            if (rows.len() as i64) >= to_size {
                break;
            }
        }
    }
    if rows.len() as i64 != to_size {
        return Err(PgError::InvalidState(format!(
            "to-checkpoint commits to {} audit rows but only {} were present",
            to_size,
            rows.len()
        )));
    }
    tx.commit().await.map_err(classify)?;

    assemble_window_pack(&rows, from_checkpoint, to_checkpoint)
        .map_err(|e| PgError::InvalidState(format!("could not encode an audit row: {e}")))
}

/// Build a window pack from the canonical `[0, to)` rows and the two
/// checkpoints - the pure core of [`export_window`], so it is testable
/// without a database.
fn assemble_window_pack(
    rows: &[AuditRow],
    from_checkpoint: Checkpoint,
    to_checkpoint: Checkpoint,
) -> Result<WindowEvidencePack, serde_json::Error> {
    let leaves: Vec<Hash> = rows.iter().map(audit_leaf_hash).collect::<Result<_, _>>()?;
    let from = from_checkpoint.tree_size as usize;

    let consistency_proof = consistency_proof(&leaves, from)
        .iter()
        .map(render_hash)
        .collect();
    let inclusion_proofs = (from..leaves.len())
        .map(|index| RowInclusionProof {
            leaf_index: index as i64,
            proof: inclusion_proof(&leaves, index)
                .iter()
                .map(render_hash)
                .collect(),
        })
        .collect();

    let manifest = WindowPackManifest {
        pack_format_version: PACK_FORMAT_V2,
        pack_kind: PACK_KIND_WINDOW.to_string(),
        from_tree_size: from_checkpoint.tree_size,
        to_tree_size: to_checkpoint.tree_size,
        from_checkpoint_hash: from_checkpoint.checkpoint_hash.clone(),
        to_checkpoint_hash: to_checkpoint.checkpoint_hash.clone(),
        from_root_hash: from_checkpoint.root_hash.clone(),
        to_root_hash: to_checkpoint.root_hash.clone(),
    };
    Ok(WindowEvidencePack {
        manifest,
        from_checkpoint,
        to_checkpoint,
        consistency_proof,
        rows: rows[from..].to_vec(),
        inclusion_proofs,
    })
}

/// Verify a window pack offline - no database. Validates the v2 envelope,
/// matches the supplied anchor against the from-checkpoint, then checks the
/// consistency proof (append-only extension) and every row's inclusion proof
/// (the rows are the genuine suffix), and finally the to-checkpoint
/// signatures cryptographically. Governed signer authority is NOT judged - a
/// window lacks the `[0, from)` rows that would establish it.
pub fn verify_window(
    pack: &WindowEvidencePack,
    anchor: Option<&Checkpoint>,
) -> Result<WindowVerification, PackError> {
    validate_window_envelope(pack)?;
    let from = &pack.from_checkpoint;
    let to = &pack.to_checkpoint;

    // The external anchor is the whole point: a window proves it extends the
    // checkpoint the regulator already holds. Compare tree heads (signatures
    // excluded), and reject even if the internal proof would verify.
    if let Some(anchor) = anchor
        && !same_tree_head(anchor, from)
    {
        return Ok(WindowVerification::AnchorMismatch {
            tree_size: from.tree_size,
            anchor_checkpoint_hash: anchor.checkpoint_hash.clone(),
            pack_checkpoint_hash: from.checkpoint_hash.clone(),
        });
    }

    let malformed = |detail: String| PackError::Malformed { detail };
    let from_root = parse_hash(&from.root_hash)
        .ok_or_else(|| malformed("from root_hash is not sha256".into()))?;
    let to_root =
        parse_hash(&to.root_hash).ok_or_else(|| malformed("to root_hash is not sha256".into()))?;

    let consistency = parse_hashes(&pack.consistency_proof)?;
    match verify_consistency_proof(
        from.tree_size as usize,
        &from_root,
        to.tree_size as usize,
        &to_root,
        &consistency,
    ) {
        Ok(()) => {}
        Err(ProofError::Malformed | ProofError::BadParameters) => {
            return Err(malformed("the consistency proof is malformed".into()));
        }
        Err(ProofError::RootMismatch) => {
            return Ok(WindowVerification::InconsistentExtension {
                from_tree_size: from.tree_size,
                to_tree_size: to.tree_size,
            });
        }
    }

    for (row, rp) in pack.rows.iter().zip(&pack.inclusion_proofs) {
        let leaf = audit_leaf_hash(row)?;
        let proof = parse_hashes(&rp.proof)?;
        match verify_inclusion_proof(
            rp.leaf_index as usize,
            to.tree_size as usize,
            &leaf,
            &to_root,
            &proof,
        ) {
            Ok(()) => {}
            Err(ProofError::Malformed | ProofError::BadParameters) => {
                return Err(malformed(format!(
                    "inclusion proof for leaf {} is malformed",
                    rp.leaf_index
                )));
            }
            Err(ProofError::RootMismatch) => {
                return Ok(WindowVerification::RowNotIncluded {
                    leaf_index: rp.leaf_index,
                });
            }
        }
    }

    // The to-checkpoint is the new attestation this pack carries; its
    // signatures must be genuine. (The from-checkpoint's trust comes from the
    // external anchor.) Authority - whether the key was admitted - is a
    // full-prefix question a window cannot answer.
    if let Some(TreeVerification::SignatureInvalid {
        tree_size,
        key_id,
        purpose,
        public_key,
    }) = signature_crypto_violation(to)
    {
        return Ok(WindowVerification::SignatureInvalid {
            tree_size,
            key_id,
            purpose,
            public_key,
        });
    }

    Ok(WindowVerification::Intact {
        from_tree_size: from.tree_size,
        to_tree_size: to.tree_size,
        rows: pack.rows.len(),
    })
}

fn parse_hashes(strings: &[String]) -> Result<Vec<Hash>, PackError> {
    strings
        .iter()
        .map(|s| {
            parse_hash(s).ok_or_else(|| PackError::Malformed {
                detail: format!("proof hash is not a sha256 digest: {s}"),
            })
        })
        .collect()
}

/// The v2 envelope rules a well-formed window pack must satisfy before its
/// proofs are worth checking. Like the prefix validator, stricter than the
/// live path: a pack is untrusted JSON.
fn validate_window_envelope(pack: &WindowEvidencePack) -> Result<(), PackError> {
    let malformed = |detail: String| PackError::Malformed { detail };
    let m = &pack.manifest;
    if m.pack_format_version != PACK_FORMAT_V2 {
        return Err(malformed(format!(
            "unsupported pack_format_version {}",
            m.pack_format_version
        )));
    }
    if m.pack_kind != PACK_KIND_WINDOW {
        return Err(malformed(format!("unexpected pack_kind {:?}", m.pack_kind)));
    }

    let from = &pack.from_checkpoint;
    let to = &pack.to_checkpoint;
    if from.tree_size < 0 || to.tree_size < 0 {
        return Err(malformed("a checkpoint tree_size is negative".into()));
    }
    if from.tree_size >= to.tree_size {
        return Err(malformed(format!(
            "from tree_size {} is not strictly before to tree_size {}",
            from.tree_size, to.tree_size
        )));
    }

    // Each checkpoint's identity hash must match its own contents - a forged
    // checkpoint_hash is rejected before the proofs trust it.
    for (label, cp) in [("from", from), ("to", to)] {
        let expected = checkpoint_hash(
            cp.tree_size,
            &cp.root_hash,
            cp.prev_checkpoint_hash.as_deref(),
        );
        if expected != cp.checkpoint_hash {
            return Err(malformed(format!(
                "{label}-checkpoint hash {} does not match its contents",
                cp.checkpoint_hash
            )));
        }
    }

    // Exactly the `to - from` window rows, one inclusion proof each, with
    // declared leaf indices exactly `from .. to-1` in order - so every
    // position in the window is covered and none is duplicated or omitted.
    let expected = (to.tree_size - from.tree_size) as usize;
    if pack.rows.len() != expected {
        return Err(malformed(format!(
            "the window covers {} rows but the pack carries {}",
            expected,
            pack.rows.len()
        )));
    }
    if pack.inclusion_proofs.len() != pack.rows.len() {
        return Err(malformed(format!(
            "{} rows but {} inclusion proofs",
            pack.rows.len(),
            pack.inclusion_proofs.len()
        )));
    }
    for (offset, rp) in pack.inclusion_proofs.iter().enumerate() {
        let expected_index = from.tree_size + offset as i64;
        if rp.leaf_index != expected_index {
            return Err(malformed(format!(
                "inclusion proof {offset} declares leaf_index {} but the window expects {expected_index}",
                rp.leaf_index
            )));
        }
    }

    let mut rows = pack.rows.clone();
    rows.sort_by_key(|a| (a.committed_at, a.transition_id));
    for pair in rows.windows(2) {
        if (pair[0].committed_at, pair[0].transition_id)
            == (pair[1].committed_at, pair[1].transition_id)
        {
            return Err(malformed(format!(
                "two rows share coordinates ({}, {})",
                pair[0].committed_at, pair[0].transition_id
            )));
        }
    }

    if m.from_tree_size != from.tree_size
        || m.to_tree_size != to.tree_size
        || m.from_checkpoint_hash != from.checkpoint_hash
        || m.to_checkpoint_hash != to.checkpoint_hash
        || m.from_root_hash != from.root_hash
        || m.to_root_hash != to.root_hash
    {
        return Err(malformed("manifest disagrees with the checkpoints".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dummy audit row built through the same `Deserialize` the pack
    /// uses. The content is irrelevant to envelope validation (which runs
    /// before any hashing); only the `(committed_at, transition_id)`
    /// coordinates matter here.
    fn row(committed_at: &str, id: &str) -> AuditRow {
        serde_json::from_value(serde_json::json!({
            "transition_id": id,
            "transformation_name": "X",
            "arguments": [],
            "actor": { "type": "subject", "value": "00000000-0000-0000-0000-000000000001" },
            "invariant_epoch": 1,
            "invariants_checked": [],
            "asserted_claims": [],
            "retracted_claims": [],
            "emitted_intents": [],
            "committed_at": committed_at,
        }))
        .unwrap()
    }

    /// A dummy checkpoint. Its hashes are placeholders - envelope checks
    /// compare strings, they do not recompute the Merkle root (that is
    /// `verify_tree`, reached only after a well-formed envelope).
    fn checkpoint(tree_size: i64) -> Checkpoint {
        Checkpoint {
            tree_size,
            root_hash: format!("sha256:{tree_size:0>64}"),
            prev_checkpoint_hash: None,
            checkpoint_hash: format!("cp-{tree_size}"),
            signatures: Vec::new(),
        }
    }

    fn manifest_for(c: &Checkpoint) -> PackManifest {
        PackManifest {
            pack_format_version: PACK_FORMAT_V1,
            tree_size: c.tree_size,
            root_hash: c.root_hash.clone(),
            checkpoint_hash: c.checkpoint_hash.clone(),
        }
    }

    fn malformed(detail_contains: &str, pack: &EvidencePack) {
        match verify_pack(pack, None) {
            Err(PackError::Malformed { detail }) => assert!(
                detail.contains(detail_contains),
                "expected detail to mention {detail_contains:?}, got {detail:?}"
            ),
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn empty_checkpoint_chain_is_malformed() {
        let pack = EvidencePack {
            manifest: manifest_for(&checkpoint(0)),
            checkpoints: vec![],
            rows: vec![],
        };
        malformed("chain is empty", &pack);
    }

    #[test]
    fn non_increasing_checkpoints_are_malformed() {
        let pack = EvidencePack {
            manifest: manifest_for(&checkpoint(2)),
            checkpoints: vec![checkpoint(2), checkpoint(2)],
            rows: vec![],
        };
        malformed("strictly increasing", &pack);
    }

    #[test]
    fn too_few_rows_is_malformed() {
        let cp = checkpoint(2);
        let pack = EvidencePack {
            manifest: manifest_for(&cp),
            checkpoints: vec![cp],
            rows: vec![row(
                "2026-06-24T00:00:00Z",
                "00000000-0000-0000-0000-0000000000a1",
            )],
        };
        malformed("commits to 2", &pack);
    }

    #[test]
    fn extra_rows_beyond_the_checkpoint_are_malformed() {
        // The load-bearing case from review: rows past the covering
        // checkpoint must NOT ride along unproven.
        let cp = checkpoint(1);
        let pack = EvidencePack {
            manifest: manifest_for(&cp),
            checkpoints: vec![cp],
            rows: vec![
                row(
                    "2026-06-24T00:00:00Z",
                    "00000000-0000-0000-0000-0000000000a1",
                ),
                row(
                    "2026-06-24T00:00:01Z",
                    "00000000-0000-0000-0000-0000000000a2",
                ),
            ],
        };
        malformed("commits to 1", &pack);
    }

    #[test]
    fn manifest_disagreement_is_malformed() {
        let cp = checkpoint(1);
        let mut manifest = manifest_for(&cp);
        manifest.tree_size = 999;
        let pack = EvidencePack {
            manifest,
            checkpoints: vec![cp],
            rows: vec![row(
                "2026-06-24T00:00:00Z",
                "00000000-0000-0000-0000-0000000000a1",
            )],
        };
        malformed("manifest disagrees", &pack);
    }

    #[test]
    fn an_unknown_top_level_field_is_rejected() {
        let cp = checkpoint(1);
        let pack = EvidencePack {
            manifest: manifest_for(&cp),
            checkpoints: vec![cp],
            rows: vec![row(
                "2026-06-24T00:00:00Z",
                "00000000-0000-0000-0000-0000000000a1",
            )],
        };
        let mut v = serde_json::to_value(&pack).unwrap();
        v["surprise"] = serde_json::json!("not part of the proof");
        assert!(
            serde_json::from_value::<EvidencePack>(v).is_err(),
            "an unknown top-level field must not be silently tolerated"
        );
    }

    #[test]
    fn duplicate_row_coordinates_are_malformed() {
        let cp = checkpoint(2);
        let dup = row(
            "2026-06-24T00:00:00Z",
            "00000000-0000-0000-0000-0000000000a1",
        );
        let pack = EvidencePack {
            manifest: manifest_for(&cp),
            checkpoints: vec![cp],
            rows: vec![dup.clone(), dup],
        };
        malformed("share coordinates", &pack);
    }

    // --- window packs ---

    use crate::merkle::merkle_root;

    /// `n` rows in canonical order, each tagged so a different `tag` yields
    /// a different leaf at the same coordinates (a rewritten-prefix forgery).
    fn rows_tagged(n: usize, tag: char) -> Vec<AuditRow> {
        (0..n)
            .map(|i| {
                serde_json::from_value(serde_json::json!({
                    "transition_id": format!("00000000-0000-0000-0000-{:012x}", i + 1),
                    "transformation_name": format!("X{tag}"),
                    "arguments": [],
                    "actor": { "type": "subject", "value": "00000000-0000-0000-0000-000000000001" },
                    "invariant_epoch": 1,
                    "invariants_checked": [],
                    "asserted_claims": [],
                    "retracted_claims": [],
                    "emitted_intents": [],
                    "committed_at": format!("2026-06-24T00:00:{:02}Z", i),
                }))
                .unwrap()
            })
            .collect()
    }

    fn real_checkpoint(leaves: &[Hash], size: usize, prev: Option<&Checkpoint>) -> Checkpoint {
        let root = render_hash(&merkle_root(&leaves[..size]));
        let prev_hash = prev.map(|c| c.checkpoint_hash.clone());
        let checkpoint_hash = checkpoint_hash(size as i64, &root, prev_hash.as_deref());
        Checkpoint {
            tree_size: size as i64,
            root_hash: root,
            prev_checkpoint_hash: prev_hash,
            checkpoint_hash,
            signatures: Vec::new(),
        }
    }

    /// A valid window pack over a single `tag`-history of `to` rows, with the
    /// from-checkpoint at `from`. Returns the pack and the from-checkpoint
    /// (the prior anchor a verifier would hold).
    fn valid_window(from: usize, to: usize) -> (WindowEvidencePack, Checkpoint) {
        let rows = rows_tagged(to, 'a');
        let leaves: Vec<Hash> = rows.iter().map(|r| audit_leaf_hash(r).unwrap()).collect();
        let from_cp = real_checkpoint(&leaves, from, None);
        let to_cp = real_checkpoint(&leaves, to, Some(&from_cp));
        let pack = assemble_window_pack(&rows, from_cp.clone(), to_cp).unwrap();
        (pack, from_cp)
    }

    #[test]
    fn a_window_round_trips_and_matches_its_anchor() {
        let (pack, anchor) = valid_window(3, 7);
        let intact = WindowVerification::Intact {
            from_tree_size: 3,
            to_tree_size: 7,
            rows: 4,
        };
        assert_eq!(verify_window(&pack, None).unwrap(), intact);
        assert_eq!(verify_window(&pack, Some(&anchor)).unwrap(), intact);
    }

    #[test]
    fn a_wrong_anchor_is_caught_even_when_the_proof_verifies() {
        let (pack, _) = valid_window(3, 7);
        // An anchor from an unrelated history at the same size.
        let other: Vec<Hash> = rows_tagged(3, 'z')
            .iter()
            .map(|r| audit_leaf_hash(r).unwrap())
            .collect();
        let wrong_anchor = real_checkpoint(&other, 3, None);
        assert!(matches!(
            verify_window(&pack, Some(&wrong_anchor)),
            Ok(WindowVerification::AnchorMismatch { .. })
        ));
    }

    #[test]
    fn a_tampered_window_row_is_not_included() {
        // The overclaim-1 guard at the pack level: a genuine consistency
        // proof does not protect the rows; only inclusion does. Mutating a
        // row's body (coordinates unchanged, so the envelope passes) is
        // caught by its inclusion proof, not the consistency proof.
        let (mut pack, _) = valid_window(3, 7);
        pack.rows[0].invariant_epoch = 999;
        assert_eq!(
            verify_window(&pack, None).unwrap(),
            WindowVerification::RowNotIncluded { leaf_index: 3 }
        );
    }

    #[test]
    fn a_rewritten_prior_prefix_is_an_inconsistent_extension() {
        // The to-checkpoint is over the real history; the from-checkpoint
        // claims a different prefix root (the prior period was rewritten).
        // Consistency must reject - and this is a genuine fork, not a
        // corrupted proof.
        let rows = rows_tagged(7, 'a');
        let leaves: Vec<Hash> = rows.iter().map(|r| audit_leaf_hash(r).unwrap()).collect();
        let to_cp = real_checkpoint(&leaves, 7, None);
        let forged: Vec<Hash> = rows_tagged(3, 'b')
            .iter()
            .map(|r| audit_leaf_hash(r).unwrap())
            .collect();
        let from_cp = real_checkpoint(&forged, 3, None);
        let pack = assemble_window_pack(&rows, from_cp, to_cp).unwrap();
        assert_eq!(
            verify_window(&pack, None).unwrap(),
            WindowVerification::InconsistentExtension {
                from_tree_size: 3,
                to_tree_size: 7,
            }
        );
    }

    #[test]
    fn a_wrong_window_row_count_is_malformed() {
        let (mut pack, _) = valid_window(3, 7);
        pack.rows.pop();
        pack.inclusion_proofs.pop();
        match verify_window(&pack, None) {
            Err(PackError::Malformed { detail }) => assert!(detail.contains("covers 4 rows")),
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn a_wrong_declared_leaf_index_is_malformed() {
        let (mut pack, _) = valid_window(3, 7);
        pack.inclusion_proofs[1].leaf_index = 99;
        match verify_window(&pack, None) {
            Err(PackError::Malformed { detail }) => assert!(detail.contains("leaf_index")),
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn a_forged_checkpoint_hash_is_malformed() {
        let (mut pack, _) = valid_window(3, 7);
        pack.to_checkpoint.checkpoint_hash = "cp-forged".into();
        pack.manifest.to_checkpoint_hash = "cp-forged".into();
        match verify_window(&pack, None) {
            Err(PackError::Malformed { detail }) => {
                assert!(detail.contains("does not match its contents"))
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_window_field_is_rejected() {
        let (pack, _) = valid_window(3, 7);
        let mut v = serde_json::to_value(&pack).unwrap();
        v["surprise"] = serde_json::json!("not part of the proof");
        assert!(serde_json::from_value::<WindowEvidencePack>(v).is_err());
    }
}
