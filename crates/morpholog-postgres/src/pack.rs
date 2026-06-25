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
use crate::checkpoints::{Checkpoint, TreeVerification, verify_tree};
use crate::error::{PgError, classify};
use crate::merkle::{Hash, audit_leaf_hash};
use crate::txn::{TxIsolation, begin_isolated_tx};

const PACK_FORMAT_V1: u32 = 1;

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

    let stored = sqlx::query!(
        "SELECT tree_size, root_hash, prev_checkpoint_hash, checkpoint_hash
         FROM morpholog.audit_checkpoints
         ORDER BY tree_size ASC",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(classify)?;
    let mut checkpoints: Vec<Checkpoint> = stored
        .into_iter()
        .map(|r| Checkpoint {
            tree_size: r.tree_size,
            root_hash: r.root_hash,
            prev_checkpoint_hash: r.prev_checkpoint_hash,
            checkpoint_hash: r.checkpoint_hash,
        })
        .collect();

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

    let mut rows: Vec<&AuditRow> = pack.rows.iter().collect();
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

    let leaves: Vec<Hash> = rows
        .iter()
        .map(|r| audit_leaf_hash(r))
        .collect::<Result<_, _>>()?;
    Ok(verify_tree(&leaves, &pack.checkpoints, anchor))
}

/// The v1 envelope rules a well-formed pack must satisfy before its
/// cryptographic core is even worth checking. Stricter than the live
/// verifier on purpose: a pack is untrusted JSON, not a database read.
fn validate_envelope(pack: &EvidencePack) -> Result<(), PackError> {
    let malformed = |detail: String| PackError::Malformed { detail };

    let Some(covering) = pack.checkpoints.last() else {
        return Err(malformed("the checkpoint chain is empty".into()));
    };
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
}
