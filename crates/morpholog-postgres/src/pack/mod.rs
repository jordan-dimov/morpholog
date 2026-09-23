//! Evidence packs: a portable, offline-verifiable export of a prefix of
//! the audit log.
//!
//! A third party recomputes the Merkle root from the pack's rows and
//! checks it against its checkpoints with no database access; against an
//! externally held anchor, that catches a coordinated rewrite.
//!
//! Three kinds: the v1 *complete prefix* (every covered row), the v2
//! *window* (consistency plus per-row inclusion between two checkpoints),
//! and the v3 *selective* pack (chosen rows, each proven included, nothing
//! else revealed). None proves that a pack holds *all* of one subject's
//! history; that needs a subject-indexed commitment.

use crate::role_rebindings::{RebindingFold, RebindingScope, RoleRebindings};
use crate::witnesses::PackVerdict;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

use crate::audit::AuditRow;
use crate::audit_pages::AuditPages;
use crate::checkpoints::{
    Checkpoint, TreeVerification, checkpoint_hash, load_checkpoint_chain, same_tree_head,
    signature_crypto_violation, verify_tree,
};
use crate::error::{PgError, classify};
use crate::merkle::{
    Digest, Hash, ProofError, audit_leaf_hash, consistency_proof, inclusion_proof,
    verify_consistency_proof, verify_inclusion_proof,
};
use crate::txn::{TxIsolation, begin_isolated_tx};

const PACK_FORMAT_V1: u32 = 1;
const PACK_FORMAT_V2: u32 = 2;
const PACK_KIND_WINDOW: &str = "window";
const PACK_FORMAT_V3: u32 = 3;
const PACK_KIND_SELECTIVE: &str = "selective";

/// A convenience header summarising the covering checkpoint for a human
/// reader. The authoritative data is `checkpoints` and `rows`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackManifest {
    pub pack_format_version: u32,
    pub tree_size: i64,
    pub root_hash: Digest,
    pub checkpoint_hash: Digest,
}

/// A portable evidence pack: everything an offline verifier needs to
/// recompute and check the covered prefix of the audit log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidencePack {
    pub manifest: PackManifest,
    /// The checkpoint chain up to and including the covering checkpoint.
    pub checkpoints: Vec<Checkpoint>,
    /// The covered audit rows, whole, so the verifier recomputes each leaf
    /// with the runtime's own encoding.
    pub rows: Vec<AuditRow>,
}

/// The checkpoint a pack is exported against: the one at `tree_size`,
/// or the latest when none is named.
fn covering_checkpoint(
    checkpoints: &[Checkpoint],
    tree_size: Option<i64>,
) -> Result<Checkpoint, PgError> {
    match tree_size {
        Some(n) => checkpoints.iter().find(|c| c.tree_size == n),
        None => checkpoints.last(),
    }
    .cloned()
    .ok_or(PgError::NoCheckpoint)
}

/// The first `tree_size` audit rows in canonical order. A checkpoint's
/// rows are all visible (it is watermark-bounded), so fewer means an
/// incomplete prefix, refused rather than exported.
async fn load_prefix_rows(
    conn: &mut sqlx::PgConnection,
    tree_size: i64,
) -> Result<Vec<AuditRow>, PgError> {
    let mut rows: Vec<AuditRow> = Vec::new();
    let mut pages = AuditPages::new(None);
    while (rows.len() as i64) < tree_size {
        let page = pages.next(conn).await?;
        if page.is_empty() {
            break;
        }
        for row in page {
            rows.push(row);
            if (rows.len() as i64) >= tree_size {
                break;
            }
        }
    }
    if rows.len() as i64 != tree_size {
        return Err(PgError::AuditPrefixIncomplete {
            tree_size,
            rows_present: rows.len() as i64,
        });
    }
    Ok(rows)
}

/// The rows in canonical order, the order leaves are computed in. Two
/// rows at one coordinate are refused: a Merkle position holds one row.
fn canonically_sorted(rows: &[AuditRow]) -> Result<Vec<AuditRow>, PackError> {
    let mut rows = rows.to_vec();
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
    Ok(rows)
}

/// Whether a checkpoint's identity hash matches its contents, so a forged
/// one is refused before any proof trusts it.
fn identity_matches(cp: &Checkpoint) -> bool {
    checkpoint_hash(
        cp.tree_size,
        &cp.root_hash,
        cp.prev_checkpoint_hash.as_ref(),
    ) == cp.checkpoint_hash
}

/// Check every disclosed row's inclusion proof under `cp`'s root. A
/// malformed proof is a malformed pack; a proof that misses the root is
/// `Ok(Err(leaf_index))`, for the caller to report.
fn check_inclusions(
    rows: &[AuditRow],
    proofs: &[RowInclusionProof],
    cp: &Checkpoint,
) -> Result<Result<(), i64>, PackError> {
    for (row, rp) in rows.iter().zip(proofs) {
        let leaf = audit_leaf_hash(row)?;
        let proof = proof_bytes(&rp.proof);
        match verify_inclusion_proof(
            rp.leaf_index as usize,
            cp.tree_size as usize,
            &leaf,
            cp.root_hash.bytes(),
            &proof,
        ) {
            Ok(()) => {}
            Err(ProofError::Malformed | ProofError::BadParameters) => {
                return Err(PackError::Malformed {
                    detail: format!("inclusion proof for leaf {} is malformed", rp.leaf_index),
                });
            }
            Err(ProofError::RootMismatch) => return Ok(Err(rp.leaf_index)),
        }
    }
    Ok(Ok(()))
}

/// Export a complete-prefix evidence pack covering a checkpoint (the
/// latest, or the one at `tree_size` if given). Reads under `SERIALIZABLE
/// READ ONLY DEFERRABLE`. Errors if there is no such checkpoint.
pub async fn export_pack(pool: &PgPool, tree_size: Option<i64>) -> Result<EvidencePack, PgError> {
    let mut tx = begin_isolated_tx(pool, TxIsolation::SerializableReadOnlyDeferrable).await?;

    let mut checkpoints = load_checkpoint_chain(&mut tx).await?;

    let covering = covering_checkpoint(&checkpoints, tree_size)?;
    checkpoints.retain(|c| c.tree_size <= covering.tree_size);

    let rows = load_prefix_rows(&mut tx, covering.tree_size).await?;
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

/// A pack that is not well-formed. Distinct from a [`TreeVerification`]
/// verdict: a malformed pack proves nothing, while `Tampered` and the rest
/// are real divergences of a well-formed one.
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

/// Verify an evidence pack offline, with no database.
///
/// The pack is hostile input with no database constraints behind it, so
/// its envelope is validated first. Leaves are recomputed from the rows in
/// canonical order (file order does not matter) and checked by the same
/// core as the live verifier, optionally against an external anchor.
pub fn verify_pack(
    pack: &EvidencePack,
    anchor: Option<&Checkpoint>,
) -> Result<TreeVerification, PackError> {
    validate_envelope(pack)?;

    // The leaves and the authority check both use this order, as live.
    let rows = canonically_sorted(&pack.rows)?;

    let leaves: Vec<Hash> = rows.iter().map(audit_leaf_hash).collect::<Result<_, _>>()?;
    let verdict = verify_tree(&leaves, &pack.checkpoints, anchor);
    // An intact signed pack must still show, from its own rows, that each
    // signing key (the anchor's included) was authorised as of its prefix.
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

/// The v1 envelope rules, checked before any cryptography. Stricter than
/// the live verifier on purpose: a pack is untrusted JSON.
fn validate_envelope(pack: &EvidencePack) -> Result<(), PackError> {
    let malformed = |detail: String| PackError::Malformed { detail };

    let Some(covering) = pack.checkpoints.last() else {
        return Err(malformed("the checkpoint chain is empty".into()));
    };
    // The database enforces `tree_size >= 0`; a pack must be checked.
    if let Some(bad) = pack.checkpoints.iter().find(|c| c.tree_size < 0) {
        return Err(malformed(format!(
            "checkpoint tree_size is negative: {}",
            bad.tree_size
        )));
    }
    // Strictly increasing sizes, so the covering checkpoint is the last.
    for pair in pack.checkpoints.windows(2) {
        if pair[1].tree_size <= pair[0].tree_size {
            return Err(malformed(format!(
                "checkpoint sizes are not strictly increasing: {} then {}",
                pair[0].tree_size, pair[1].tree_size
            )));
        }
    }
    // Exactly the rows the covering checkpoint commits to: extra rows would
    // ride along unproven.
    if pack.rows.len() as i64 != covering.tree_size {
        return Err(malformed(format!(
            "pack carries {} rows but the covering checkpoint commits to {}",
            pack.rows.len(),
            covering.tree_size
        )));
    }
    // The crypto ignores the manifest, but people read it, so it must not lie.
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
// Window evidence packs.
//
// A window pack proves two things about the interval `[from, to)`, and needs
// both because neither implies the other:
//   1. the later checkpoint extends the earlier one (a consistency proof:
//      the prior period was not rewritten), and
//   2. each row sits at its declared position in the later checkpoint
//      (inclusion proofs: the rows are the real suffix).
//
// It lacks the `[0, from)` rows, so it cannot judge signing-key authority;
// it checks signatures cryptographically only.
// ---------------------------------------------------------------------------

/// One row's inclusion proof: the row sits at `leaf_index`, proven by
/// `proof` (sibling hashes, leaf to root).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RowInclusionProof {
    pub leaf_index: i64,
    pub proof: Vec<Digest>,
}

/// The window pack's convenience header, restating the two endpoints for a
/// human reader. It carries no authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowPackManifest {
    pub pack_format_version: u32,
    pub pack_kind: String,
    pub from_tree_size: i64,
    pub to_tree_size: i64,
    pub from_checkpoint_hash: Digest,
    pub to_checkpoint_hash: Digest,
    pub from_root_hash: Digest,
    pub to_root_hash: Digest,
}

/// A windowed evidence pack: proof that `[from_tree_size, to_tree_size)`
/// is a faithful, contiguous, append-only continuation of the earlier
/// checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowEvidencePack {
    pub manifest: WindowPackManifest,
    pub from_checkpoint: Checkpoint,
    pub to_checkpoint: Checkpoint,
    /// RFC 6962 consistency proof from `from` to `to` (rendered hashes).
    pub consistency_proof: Vec<Digest>,
    /// The window rows, in canonical order: exactly `to - from` of them.
    pub rows: Vec<AuditRow>,
    /// One inclusion proof per window row, declaring its leaf index.
    pub inclusion_proofs: Vec<RowInclusionProof>,
}

/// The verdict of verifying a window pack. Separate from
/// [`TreeVerification`] because a window proves consistency and inclusion,
/// not a recomputed prefix, so it fails in different ways.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum WindowVerification {
    /// The later checkpoint extends the earlier, every row is included at
    /// its declared position, and the to-checkpoint's signatures (if any)
    /// verify cryptographically.
    Intact {
        from_tree_size: i64,
        to_tree_size: i64,
        rows: usize,
    },
    /// The later checkpoint does not extend the earlier one: the prior
    /// period was altered.
    InconsistentExtension {
        from_tree_size: i64,
        to_tree_size: i64,
    },
    /// A row is not at its declared position in the later checkpoint: the
    /// rows are not the genuine suffix.
    RowNotIncluded { leaf_index: i64 },
    /// An externally held anchor disagrees with the pack's from-checkpoint.
    AnchorMismatch {
        tree_size: i64,
        anchor_checkpoint_hash: Digest,
        pack_checkpoint_hash: Digest,
    },
    /// The to-checkpoint carries a signature that does not verify over its
    /// tree head (cryptographic check only; authority is not judged here).
    SignatureInvalid {
        tree_size: i64,
        key_id: String,
        purpose: String,
        public_key: String,
    },
    /// Signatures were required and the to-checkpoint has none. A policy
    /// verdict the verifier opts into, not tampering.
    SignatureRequired { tree_size: i64 },
    /// Not a well-formed v2 pack (bad envelope or JSON), so it proves
    /// nothing; distinct from a divergence of a well-formed pack.
    Malformed { detail: String },
}

/// Where a window starts. `TreeSize` trusts the database's checkpoint at
/// that size; `Anchor` carries the externally held checkpoint, and export
/// REFUSES unless the stored start still matches it.
#[derive(Debug, Clone)]
pub enum WindowStart {
    TreeSize(i64),
    Anchor(Checkpoint),
}

impl WindowStart {
    fn tree_size(&self) -> i64 {
        match self {
            WindowStart::TreeSize(n) => *n,
            WindowStart::Anchor(c) => c.tree_size,
        }
    }
}

/// Export a windowed evidence pack between two existing checkpoints, under
/// `SERIALIZABLE READ ONLY DEFERRABLE`. `to_tree_size` defaults to the
/// latest.
///
/// Errors if either endpoint is not a checkpoint, if `from` is not strictly
/// before `to`, or if an `Anchor` start has diverged from the stored one.
pub async fn export_window(
    pool: &PgPool,
    start: WindowStart,
    to_tree_size: Option<i64>,
) -> Result<WindowEvidencePack, PgError> {
    let mut tx = begin_isolated_tx(pool, TxIsolation::SerializableReadOnlyDeferrable).await?;
    let checkpoints = load_checkpoint_chain(&mut tx).await?;

    let to_checkpoint = covering_checkpoint(&checkpoints, to_tree_size)?;
    let Some(from_checkpoint) = checkpoints
        .iter()
        .find(|c| c.tree_size == start.tree_size())
        .cloned()
    else {
        return Err(PgError::NoCheckpoint);
    };
    // The stored start must still match the caller's anchor (tree heads,
    // signatures excluded), or the window would silently start from a
    // checkpoint that has diverged from what they hold.
    if let WindowStart::Anchor(anchor) = &start
        && !same_tree_head(anchor, &from_checkpoint)
    {
        return Err(PgError::AnchorDivergedFromStart {
            tree_size: from_checkpoint.tree_size,
        });
    }
    if from_checkpoint.tree_size >= to_checkpoint.tree_size {
        return Err(PgError::InvalidState(format!(
            "window from tree_size {} must be strictly before to tree_size {}",
            from_checkpoint.tree_size, to_checkpoint.tree_size
        )));
    }

    // The proofs need the whole `[0, to)` prefix.
    let rows = load_prefix_rows(&mut tx, to_checkpoint.tree_size).await?;
    tx.commit().await.map_err(classify)?;

    assemble_window_pack(&rows, from_checkpoint, to_checkpoint)
        .map_err(|e| PgError::InvalidState(format!("could not encode an audit row: {e}")))
}

/// Build a window pack from the canonical `[0, to)` rows and the two
/// checkpoints: the pure core of [`export_window`].
fn assemble_window_pack(
    rows: &[AuditRow],
    from_checkpoint: Checkpoint,
    to_checkpoint: Checkpoint,
) -> Result<WindowEvidencePack, serde_json::Error> {
    let leaves: Vec<Hash> = rows.iter().map(audit_leaf_hash).collect::<Result<_, _>>()?;
    let from = from_checkpoint.tree_size as usize;

    let consistency_proof = consistency_proof(&leaves, from)
        .into_iter()
        .map(Digest::from_bytes)
        .collect();
    let inclusion_proofs = (from..leaves.len())
        .map(|index| RowInclusionProof {
            leaf_index: index as i64,
            proof: inclusion_proof(&leaves, index)
                .into_iter()
                .map(Digest::from_bytes)
                .collect(),
        })
        .collect();

    let manifest = WindowPackManifest {
        pack_format_version: PACK_FORMAT_V2,
        pack_kind: PACK_KIND_WINDOW.to_string(),
        from_tree_size: from_checkpoint.tree_size,
        to_tree_size: to_checkpoint.tree_size,
        from_checkpoint_hash: from_checkpoint.checkpoint_hash,
        to_checkpoint_hash: to_checkpoint.checkpoint_hash,
        from_root_hash: from_checkpoint.root_hash,
        to_root_hash: to_checkpoint.root_hash,
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

/// Verify a window pack offline, with no database.
///
/// Validates the envelope, matches the anchor against the from-checkpoint,
/// then checks the consistency proof, every row's inclusion proof, and the
/// to-checkpoint's signatures cryptographically. Signer authority is NOT
/// judged: a window lacks the `[0, from)` rows it needs.
pub fn verify_window(
    pack: &WindowEvidencePack,
    anchor: Option<&Checkpoint>,
) -> Result<WindowVerification, PackError> {
    validate_window_envelope(pack)?;
    let from = &pack.from_checkpoint;
    let to = &pack.to_checkpoint;

    // A window proves it extends the checkpoint the reader already holds.
    // Compare tree heads (signatures excluded), even if the proofs verify.
    if let Some(anchor) = anchor
        && !same_tree_head(anchor, from)
    {
        return Ok(WindowVerification::AnchorMismatch {
            tree_size: from.tree_size,
            anchor_checkpoint_hash: anchor.checkpoint_hash,
            pack_checkpoint_hash: from.checkpoint_hash,
        });
    }

    let malformed = |detail: String| PackError::Malformed { detail };
    let consistency = proof_bytes(&pack.consistency_proof);
    match verify_consistency_proof(
        from.tree_size as usize,
        from.root_hash.bytes(),
        to.tree_size as usize,
        to.root_hash.bytes(),
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

    if let Err(leaf_index) = check_inclusions(&pack.rows, &pack.inclusion_proofs, to)? {
        return Ok(WindowVerification::RowNotIncluded { leaf_index });
    }

    // The to-checkpoint's signatures must be genuine; the from-checkpoint is
    // trusted through the anchor.
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

/// The raw bytes of a proof's digests. Infallible: non-digest proofs fail
/// to deserialise.
fn proof_bytes(digests: &[Digest]) -> Vec<Hash> {
    digests.iter().map(|d| *d.bytes()).collect()
}

/// The v2 envelope rules, checked before any proof. A pack is untrusted
/// JSON.
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

    for (label, cp) in [("from", from), ("to", to)] {
        if !identity_matches(cp) {
            return Err(malformed(format!(
                "{label}-checkpoint hash {} does not match its contents",
                cp.checkpoint_hash
            )));
        }
    }

    // Exactly `to - from` rows, one proof each, with leaf indices exactly
    // `from .. to-1` in order: every position covered once.
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

    canonically_sorted(&pack.rows)?;

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

// ---------------------------------------------------------------------------
// Selective evidence packs.
//
// A CHOSEN subset of the rows a checkpoint commits to, each with its
// inclusion proof; other rows are absent in any form. Each disclosed row is
// proven genuine at its position by its Merkle path, never by its place in
// the pack. It does NOT prove the selection is complete, and like a window it
// checks signatures cryptographically only. Leaf indices reveal positions and
// count.
// ---------------------------------------------------------------------------

/// The selective pack's convenience header: the covering checkpoint's
/// coordinates. It carries no authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectivePackManifest {
    pub pack_format_version: u32,
    pub pack_kind: String,
    pub tree_size: i64,
    pub root_hash: Digest,
    pub checkpoint_hash: Digest,
}

/// A selective evidence pack: a chosen subset of audit rows, each proven
/// included at its declared position in the covering checkpoint's tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectiveEvidencePack {
    pub manifest: SelectivePackManifest,
    pub checkpoint: Checkpoint,
    /// The disclosed rows, ordered by ascending leaf index.
    pub rows: Vec<AuditRow>,
    /// One inclusion proof per disclosed row, in the same order.
    pub inclusion_proofs: Vec<RowInclusionProof>,
}

/// The verdict of verifying a selective pack. Like [`WindowVerification`]
/// without consistency: it proves inclusion under one checkpoint only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum SelectiveVerification {
    /// Every disclosed row is at its declared position, and the
    /// checkpoint's signatures (if any) verify cryptographically.
    /// `rows_disclosed` counts only what the pack shows, not what it left out.
    Intact {
        tree_size: i64,
        rows_disclosed: usize,
    },
    /// A disclosed row is not the one the checkpoint committed to at its
    /// declared position.
    RowNotIncluded { leaf_index: i64 },
    /// An externally held anchor disagrees with the pack's checkpoint.
    AnchorMismatch {
        tree_size: i64,
        anchor_checkpoint_hash: Digest,
        pack_checkpoint_hash: Digest,
    },
    /// The checkpoint carries a signature that does not verify over its
    /// tree head (cryptographic check only; authority is not judged here).
    SignatureInvalid {
        tree_size: i64,
        key_id: String,
        purpose: String,
        public_key: String,
    },
    /// Signatures were required and the checkpoint has none: a policy
    /// verdict the verifier opts into.
    SignatureRequired { tree_size: i64 },
    /// Not a well-formed v3 pack, so it proves nothing.
    Malformed { detail: String },
}

/// Why a selection cannot be assembled. The verifier sees only disclosed
/// rows, so bad selections must be refused at export.
#[derive(Debug)]
enum AssembleSelectiveError {
    Encoding(serde_json::Error),
    UnknownTransition(Uuid),
    DuplicateTransition(Uuid),
    EmptySelection,
}

impl std::fmt::Display for AssembleSelectiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AssembleSelectiveError::Encoding(e) => {
                write!(f, "could not encode an audit row: {e}")
            }
            AssembleSelectiveError::UnknownTransition(id) => write!(
                f,
                "transition {id} is not in the prefix the covering checkpoint commits to"
            ),
            AssembleSelectiveError::DuplicateTransition(id) => {
                write!(f, "transition {id} was selected more than once")
            }
            AssembleSelectiveError::EmptySelection => {
                write!(f, "a selective pack must disclose at least one row")
            }
        }
    }
}

/// Build a selective pack from the canonical `[0, tree_size)` rows and the
/// covering checkpoint: the pure core of [`export_selective`]. Rows come
/// out in leaf-index order whatever the selection's order.
fn assemble_selective_pack(
    rows: &[AuditRow],
    checkpoint: Checkpoint,
    selected: &[Uuid],
) -> Result<SelectiveEvidencePack, AssembleSelectiveError> {
    if selected.is_empty() {
        return Err(AssembleSelectiveError::EmptySelection);
    }
    let index_by_id: std::collections::HashMap<Uuid, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| (r.transition_id, i))
        .collect();
    let mut seen = std::collections::HashSet::with_capacity(selected.len());
    let mut indices: Vec<usize> = Vec::with_capacity(selected.len());
    for id in selected {
        let index = *index_by_id
            .get(id)
            .ok_or(AssembleSelectiveError::UnknownTransition(*id))?;
        if !seen.insert(*id) {
            return Err(AssembleSelectiveError::DuplicateTransition(*id));
        }
        indices.push(index);
    }
    indices.sort_unstable();

    let leaves: Vec<Hash> = rows
        .iter()
        .map(audit_leaf_hash)
        .collect::<Result<_, _>>()
        .map_err(AssembleSelectiveError::Encoding)?;
    let inclusion_proofs = indices
        .iter()
        .map(|&index| RowInclusionProof {
            leaf_index: index as i64,
            proof: inclusion_proof(&leaves, index)
                .into_iter()
                .map(Digest::from_bytes)
                .collect(),
        })
        .collect();

    let manifest = SelectivePackManifest {
        pack_format_version: PACK_FORMAT_V3,
        pack_kind: PACK_KIND_SELECTIVE.to_string(),
        tree_size: checkpoint.tree_size,
        root_hash: checkpoint.root_hash,
        checkpoint_hash: checkpoint.checkpoint_hash,
    };
    Ok(SelectiveEvidencePack {
        manifest,
        checkpoint,
        rows: indices.iter().map(|&i| rows[i].clone()).collect(),
        inclusion_proofs,
    })
}

/// Export a selective evidence pack: the chosen transitions, each proven
/// included under the covering checkpoint (at `tree_size`, or the latest).
/// Reads the whole prefix under `SERIALIZABLE READ ONLY DEFERRABLE`, since
/// proofs need every leaf, but the pack carries only the selection.
pub async fn export_selective(
    pool: &PgPool,
    tree_size: Option<i64>,
    transitions: &[Uuid],
) -> Result<SelectiveEvidencePack, PgError> {
    let mut tx = begin_isolated_tx(pool, TxIsolation::SerializableReadOnlyDeferrable).await?;
    let checkpoints = load_checkpoint_chain(&mut tx).await?;
    let covering = covering_checkpoint(&checkpoints, tree_size)?;

    let to_size = covering.tree_size;
    let rows = load_prefix_rows(&mut tx, to_size).await?;
    tx.commit().await.map_err(classify)?;

    assemble_selective_pack(&rows, covering, transitions).map_err(|e| match e {
        AssembleSelectiveError::UnknownTransition(id) => PgError::TransitionNotCovered {
            id,
            tree_size: to_size,
        },
        other => PgError::InvalidState(other.to_string()),
    })
}

/// Verify a selective pack offline, with no database.
///
/// Validates the envelope, matches the anchor against the checkpoint, and
/// checks every inclusion proof and the signatures cryptographically. It
/// proves the disclosed rows genuine, never that the selection is complete.
pub fn verify_selective(
    pack: &SelectiveEvidencePack,
    anchor: Option<&Checkpoint>,
) -> Result<SelectiveVerification, PackError> {
    validate_selective_envelope(pack)?;
    let cp = &pack.checkpoint;

    if let Some(anchor) = anchor
        && !same_tree_head(anchor, cp)
    {
        return Ok(SelectiveVerification::AnchorMismatch {
            tree_size: cp.tree_size,
            anchor_checkpoint_hash: anchor.checkpoint_hash,
            pack_checkpoint_hash: cp.checkpoint_hash,
        });
    }

    if let Err(leaf_index) = check_inclusions(&pack.rows, &pack.inclusion_proofs, cp)? {
        return Ok(SelectiveVerification::RowNotIncluded { leaf_index });
    }

    if let Some(TreeVerification::SignatureInvalid {
        tree_size,
        key_id,
        purpose,
        public_key,
    }) = signature_crypto_violation(cp)
    {
        return Ok(SelectiveVerification::SignatureInvalid {
            tree_size,
            key_id,
            purpose,
            public_key,
        });
    }

    Ok(SelectiveVerification::Intact {
        tree_size: cp.tree_size,
        rows_disclosed: pack.rows.len(),
    })
}

/// The v3 envelope rules, checked before any proof. Leaf indices carry the
/// proof, not row order, but must still be ascending, in range, and unique,
/// so a malformed pack is named before any cryptography runs.
fn validate_selective_envelope(pack: &SelectiveEvidencePack) -> Result<(), PackError> {
    let malformed = |detail: String| PackError::Malformed { detail };
    let m = &pack.manifest;
    if m.pack_format_version != PACK_FORMAT_V3 {
        return Err(malformed(format!(
            "unsupported pack_format_version {}",
            m.pack_format_version
        )));
    }
    if m.pack_kind != PACK_KIND_SELECTIVE {
        return Err(malformed(format!("unexpected pack_kind {:?}", m.pack_kind)));
    }

    let cp = &pack.checkpoint;
    if cp.tree_size < 0 {
        return Err(malformed("the checkpoint tree_size is negative".into()));
    }
    if !identity_matches(cp) {
        return Err(malformed(format!(
            "checkpoint hash {} does not match its contents",
            cp.checkpoint_hash
        )));
    }

    if pack.rows.is_empty() {
        return Err(malformed(
            "a selective pack must disclose at least one row".into(),
        ));
    }
    if pack.inclusion_proofs.len() != pack.rows.len() {
        return Err(malformed(format!(
            "{} rows but {} inclusion proofs",
            pack.rows.len(),
            pack.inclusion_proofs.len()
        )));
    }
    for rp in &pack.inclusion_proofs {
        if rp.leaf_index < 0 || rp.leaf_index >= cp.tree_size {
            return Err(malformed(format!(
                "leaf_index {} is outside the checkpoint's tree of {} leaves",
                rp.leaf_index, cp.tree_size
            )));
        }
    }
    for pair in pack.inclusion_proofs.windows(2) {
        if pair[0].leaf_index >= pair[1].leaf_index {
            return Err(malformed(format!(
                "leaf indices must be strictly increasing; {} is followed by {}",
                pair[0].leaf_index, pair[1].leaf_index
            )));
        }
    }

    canonically_sorted(&pack.rows)?;

    if m.tree_size != cp.tree_size
        || m.root_hash != cp.root_hash
        || m.checkpoint_hash != cp.checkpoint_hash
    {
        return Err(malformed("manifest disagrees with the checkpoint".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests;

/// The role rebindings among a pack's rows, in log order, given the
/// verdict that pack received. Only an intact verdict establishes the
/// rows; any other leaves them unevaluated.
pub fn pack_role_rebindings(bytes: &[u8], verdict: &PackVerdict) -> RoleRebindings {
    if !verdict.is_intact() {
        return RoleRebindings::NotEvaluated;
    }
    let (scope, rows) = match verdict {
        PackVerdict::Prefix(_) => (
            RebindingScope::CompletePrefix,
            serde_json::from_slice::<EvidencePack>(bytes).map(|p| p.rows),
        ),
        PackVerdict::Window(_) => (
            RebindingScope::Window,
            serde_json::from_slice::<WindowEvidencePack>(bytes).map(|p| p.rows),
        ),
        PackVerdict::Selective(_) => (
            RebindingScope::Selective,
            serde_json::from_slice::<SelectiveEvidencePack>(bytes).map(|p| p.rows),
        ),
    };
    let Some(rows) = rows.ok().and_then(|rows| canonically_sorted(&rows).ok()) else {
        return RoleRebindings::NotEvaluated;
    };
    let mut fold = RebindingFold::default();
    for row in &rows {
        fold.observe(row);
    }
    fold.finish(scope, true)
}
