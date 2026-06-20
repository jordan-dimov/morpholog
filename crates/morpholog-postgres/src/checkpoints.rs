//! Tamper-evident audit checkpoints.
//!
//! A checkpoint is a signed-tree-head-style commitment to a prefix of the
//! audit log: the RFC 6962 Merkle root of the first `tree_size` rows (in
//! `(committed_at, transition_id)` order). Checkpoints chain off one
//! another (`prev_checkpoint_hash`), so the checkpoint table is itself an
//! append-only structure.
//!
//! **Threat model, stated honestly.** Recomputing the root and comparing
//! it to a stored checkpoint catches an edit to `audit` (or `claims`,
//! via [`crate::verify_replay`]) made by someone who did *not* also
//! rewrite `audit_checkpoints`. An attacker with full write access can
//! edit a row, recompute the root, and rewrite the checkpoint chain into
//! a self-consistent false history. The real trust anchor is therefore a
//! checkpoint that has **left the database** - printed by `morpholog
//! checkpoint` and held externally; [`verify_audit_tree`] takes such an
//! anchor and fails if the stored checkpoint at that size disagrees. The
//! checkpoint chain raises the forgery cost; the external anchor is what
//! makes tampering provable.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::audit::{REPLAY_CHUNK, list_audit_rows_page};
use crate::error::{PgError, classify};
use crate::merkle::{audit_leaf_hash, merkle_root, render_hash};
use crate::txn::{TxIsolation, begin_isolated_tx};

/// Transaction-level advisory-lock key serialising checkpoint creation,
/// so two concurrent `checkpoint` runs cannot fork the chain. Arbitrary
/// fixed constant, namespaced to this feature.
const CHECKPOINT_LOCK_KEY: i64 = 0x4D4F_5250_4F4C_4701; // "MORPOLG\x01"

/// A checkpoint as it is stored, printed, and held externally as an
/// anchor. `tree_size` + `root_hash` are the cryptographic commitment;
/// `checkpoint_hash` is this checkpoint's identity in the chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub tree_size: i64,
    pub root_hash: String,
    pub prev_checkpoint_hash: Option<String>,
    pub checkpoint_hash: String,
}

/// Outcome of [`create_checkpoint`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CheckpointOutcome {
    /// A new checkpoint was recorded over a longer prefix.
    Created(Checkpoint),
    /// The stable prefix has not grown since the latest checkpoint, so
    /// there was nothing new to commit to.
    NoNewRows { tree_size: i64 },
}

/// Result of verifying the audit tree against its checkpoints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TreeVerification {
    /// Every checkpoint's root recomputes from the log, the checkpoint
    /// chain is intact, and the anchor (if supplied) matches.
    Intact { checkpoints: usize, tree_size: i64 },
    /// A checkpoint's recorded root does not match the root recomputed
    /// from the current audit log - the log was edited under a
    /// checkpoint.
    Tampered {
        tree_size: i64,
        recorded_root: String,
        recomputed_root: String,
    },
    /// The checkpoint chain is internally inconsistent (a `checkpoint_hash`
    /// does not match its contents, or a `prev` link is broken) - the
    /// checkpoint table itself was edited.
    ChainBroken { detail: String },
    /// An externally held anchor disagrees with the stored checkpoint at
    /// its tree size - the strongest signal, catching a coordinated
    /// rewrite of audit + checkpoints that is internally self-consistent
    /// but cannot match the copy that left the database.
    AnchorMismatch {
        tree_size: i64,
        anchor_checkpoint_hash: String,
        stored_checkpoint_hash: Option<String>,
    },
}

/// This checkpoint's identity hash: `SHA-256(tree_size_le ||
/// root_hash_bytes || prev_bytes)`, rendered `sha256:<hex>`. A genesis
/// checkpoint hashes the empty string for `prev`.
fn checkpoint_hash(tree_size: i64, root_hash: &str, prev: Option<&str>) -> String {
    let mut h = Sha256::new();
    h.update(tree_size.to_le_bytes());
    h.update(root_hash.as_bytes());
    h.update(prev.unwrap_or("").as_bytes());
    let digest: [u8; 32] = h.finalize().into();
    render_hash(&digest)
}

/// Page the audit log in canonical order, hashing each row to its leaf.
/// `horizon` bounds by `committed_at` (the resume watermark, for
/// checkpoint creation); `max` stops after that many rows (verification,
/// which only needs the checkpointed prefix). Returns the leaf hashes and
/// the last row's coordinates.
async fn collect_leaves(
    conn: &mut sqlx::PgConnection,
    horizon: Option<DateTime<Utc>>,
    max: Option<i64>,
) -> Result<(Vec<[u8; 32]>, Option<(Uuid, DateTime<Utc>)>), PgError> {
    let mut leaves = Vec::new();
    let mut cursor: Option<(DateTime<Utc>, Uuid)> = None;
    let mut last = None;
    loop {
        let page = list_audit_rows_page(conn, cursor, horizon, REPLAY_CHUNK).await?;
        if page.is_empty() {
            break;
        }
        let short = (page.len() as i64) < REPLAY_CHUNK;
        for row in &page {
            leaves.push(audit_leaf_hash(row)?);
            last = Some((row.transition_id, row.committed_at));
            cursor = Some((row.committed_at, row.transition_id));
            if max.is_some_and(|m| leaves.len() as i64 >= m) {
                return Ok((leaves, last));
            }
        }
        if short {
            break;
        }
    }
    Ok((leaves, last))
}

/// The latest checkpoint (highest `tree_size`), or `None` if the chain is
/// empty.
async fn latest_checkpoint(conn: &mut sqlx::PgConnection) -> Result<Option<Checkpoint>, PgError> {
    let row = sqlx::query!(
        "SELECT tree_size, root_hash, prev_checkpoint_hash, checkpoint_hash
         FROM morpholog.audit_checkpoints
         ORDER BY tree_size DESC
         LIMIT 1",
    )
    .fetch_optional(&mut *conn)
    .await
    .map_err(classify)?;
    Ok(row.map(|r| Checkpoint {
        tree_size: r.tree_size,
        root_hash: r.root_hash,
        prev_checkpoint_hash: r.prev_checkpoint_hash,
        checkpoint_hash: r.checkpoint_hash,
    }))
}

/// Record a checkpoint over the current watermark-stable prefix of the
/// audit log. The heavy root computation runs under `SERIALIZABLE READ
/// ONLY DEFERRABLE` (zero SSI footprint); the short append takes a
/// transaction advisory lock and re-reads the chain head, so concurrent
/// runs cannot fork it. Refuses a no-op when the stable prefix has not
/// grown.
pub async fn create_checkpoint(pool: &PgPool) -> Result<CheckpointOutcome, PgError> {
    // Watermark first, then the deferrable read - the lossless-resume
    // ordering: only rows below the horizon are stable enough that no
    // in-flight writer can later insert inside the prefix.
    let horizon = crate::audit::audit_resume_watermark(pool).await?;

    let mut read_tx = begin_isolated_tx(pool, TxIsolation::SerializableReadOnlyDeferrable).await?;
    let (leaves, last) = collect_leaves(&mut read_tx, Some(horizon), None).await?;
    read_tx.commit().await.map_err(classify)?;

    let tree_size = leaves.len() as i64;
    let root_hash = render_hash(&merkle_root(&leaves));

    let mut tx = pool.begin().await.map_err(classify)?;
    sqlx::query!("SELECT pg_advisory_xact_lock($1)", CHECKPOINT_LOCK_KEY)
        .execute(&mut *tx)
        .await
        .map_err(classify)?;

    let prev = latest_checkpoint(&mut tx).await?;
    if let Some(p) = &prev
        && tree_size <= p.tree_size
    {
        tx.rollback().await.map_err(classify)?;
        return Ok(CheckpointOutcome::NoNewRows {
            tree_size: p.tree_size,
        });
    }

    let prev_hash = prev.as_ref().map(|p| p.checkpoint_hash.clone());
    let cp_hash = checkpoint_hash(tree_size, &root_hash, prev_hash.as_deref());
    let (last_tid, last_at) = match last {
        Some((tid, at)) => (Some(tid), Some(at)),
        None => (None, None),
    };

    sqlx::query!(
        "INSERT INTO morpholog.audit_checkpoints (
            checkpoint_id, tree_size, root_hash, prev_checkpoint_hash,
            checkpoint_hash, covered_until, last_transition_id, last_committed_at
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        Uuid::now_v7(),
        tree_size,
        root_hash,
        prev_hash,
        cp_hash,
        horizon,
        last_tid,
        last_at,
    )
    .execute(&mut *tx)
    .await
    .map_err(classify)?;
    tx.commit().await.map_err(classify)?;

    Ok(CheckpointOutcome::Created(Checkpoint {
        tree_size,
        root_hash,
        prev_checkpoint_hash: prev_hash,
        checkpoint_hash: cp_hash,
    }))
}

/// Verify the audit tree against its checkpoints. Reads under
/// `SERIALIZABLE READ ONLY DEFERRABLE`. Three checks, strongest last:
/// every checkpoint's root recomputes from the current log; the
/// checkpoint chain is internally consistent (hash + `prev` links); and,
/// if `anchor` is supplied, the stored checkpoint at the anchor's size
/// matches the externally held copy.
pub async fn verify_audit_tree(
    pool: &PgPool,
    anchor: Option<Checkpoint>,
) -> Result<TreeVerification, PgError> {
    let mut tx = begin_isolated_tx(pool, TxIsolation::SerializableReadOnlyDeferrable).await?;

    let stored = sqlx::query!(
        "SELECT tree_size, root_hash, prev_checkpoint_hash, checkpoint_hash
         FROM morpholog.audit_checkpoints
         ORDER BY tree_size ASC",
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(classify)?;
    let checkpoints: Vec<Checkpoint> = stored
        .into_iter()
        .map(|r| Checkpoint {
            tree_size: r.tree_size,
            root_hash: r.root_hash,
            prev_checkpoint_hash: r.prev_checkpoint_hash,
            checkpoint_hash: r.checkpoint_hash,
        })
        .collect();

    // Anchor check first: a coordinated rewrite is internally consistent,
    // so only the external copy can expose it.
    if let Some(anchor) = &anchor {
        let stored_at_size = checkpoints.iter().find(|c| c.tree_size == anchor.tree_size);
        if stored_at_size != Some(anchor) {
            return Ok(TreeVerification::AnchorMismatch {
                tree_size: anchor.tree_size,
                anchor_checkpoint_hash: anchor.checkpoint_hash.clone(),
                stored_checkpoint_hash: stored_at_size.map(|c| c.checkpoint_hash.clone()),
            });
        }
    }

    let max_size = checkpoints.last().map(|c| c.tree_size).unwrap_or(0);
    let (leaves, _) = collect_leaves(&mut tx, None, Some(max_size)).await?;

    let mut prev_hash: Option<&str> = None;
    for cp in &checkpoints {
        // Chain integrity: the recorded checkpoint_hash must match its
        // contents, and prev must link to the previous checkpoint.
        let expected = checkpoint_hash(
            cp.tree_size,
            &cp.root_hash,
            cp.prev_checkpoint_hash.as_deref(),
        );
        if expected != cp.checkpoint_hash {
            return Ok(TreeVerification::ChainBroken {
                detail: format!(
                    "checkpoint at tree_size {} has hash {} but its contents hash to {expected}",
                    cp.tree_size, cp.checkpoint_hash
                ),
            });
        }
        if cp.prev_checkpoint_hash.as_deref() != prev_hash {
            return Ok(TreeVerification::ChainBroken {
                detail: format!(
                    "checkpoint at tree_size {} links to prev {:?}, expected {:?}",
                    cp.tree_size, cp.prev_checkpoint_hash, prev_hash
                ),
            });
        }
        prev_hash = Some(&cp.checkpoint_hash);

        // Tamper check: recompute the root over the log prefix.
        let size = cp.tree_size as usize;
        if size > leaves.len() {
            return Ok(TreeVerification::Tampered {
                tree_size: cp.tree_size,
                recorded_root: cp.root_hash.clone(),
                recomputed_root: format!("only {} rows present", leaves.len()),
            });
        }
        let recomputed = render_hash(&merkle_root(&leaves[..size]));
        if recomputed != cp.root_hash {
            return Ok(TreeVerification::Tampered {
                tree_size: cp.tree_size,
                recorded_root: cp.root_hash.clone(),
                recomputed_root: recomputed,
            });
        }
    }

    Ok(TreeVerification::Intact {
        checkpoints: checkpoints.len(),
        tree_size: max_size,
    })
}
