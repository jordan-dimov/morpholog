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
//!
//! **Signing (see [`crate::signing`]).** A checkpoint may carry Ed25519
//! signatures over its tree head, which make the anchor *attributable*: a
//! self-consistent rewrite cannot be re-signed without the private key, so
//! a verifier holding the trusted public key catches it even without a
//! prior anchor. `verify` checks that every signature present is genuine
//! over its tree head ([`TreeVerification::SignatureInvalid`] otherwise),
//! and that the signing key was *authorised* - an admitted `AuditSigningKey`
//! claim for that exact `(key_id, purpose, public_key)` as of the signed
//! prefix ([`TreeVerification::UnauthorizedKey`] otherwise; the as-of fold
//! lives in [`crate::keys`]). Signing makes key authority governed and
//! revocable; it does not conjure a root of trust - the first authorisation
//! is trusted the way the schema is.

use chrono::{DateTime, Utc};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::audit::{AuditRow, REPLAY_CHUNK, list_audit_rows_page};
use crate::error::{PgError, classify_checked_query};
use crate::merkle::{Hash, audit_leaf_hash, merkle_root, render_hash};
use crate::signing;
use crate::txn::{TxIsolation, begin_isolated_tx};

/// What an `AuditSigningKey` claim authorises a key for. Bound into the
/// signed payload, so a key authorised for checkpoints cannot sign a
/// future artefact kind (an evidence pack, a schema manifest) by
/// accident. The authority check matches this exact `purpose`.
pub const AUDIT_CHECKPOINT_PURPOSE: &str = "audit_checkpoint_v1";

/// A signing identity for [`create_checkpoint`]: the private key plus the
/// `key_id` it is published under. The private key is held by the caller
/// (read from a file in the CLI); it never enters the database.
pub struct CheckpointSigner {
    pub key_id: String,
    pub key: SigningKey,
}

/// Transaction-level advisory-lock key serialising checkpoint creation,
/// so two concurrent `checkpoint` runs cannot fork the chain. Arbitrary
/// fixed constant, namespaced to this feature.
const CHECKPOINT_LOCK_KEY: i64 = 0x4D4F_5250_4F4C_4701; // "MORPOLG\x01"

/// One Ed25519 attestation over a tree head: the signer (`key_id` +
/// `public_key`), what the key is authorised for (`purpose`), and the
/// signature, both rendered `ed25519-pub:`/`ed25519-sig:` hex. Carried
/// with the checkpoint so an externally held anchor is attributable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeHeadSignature {
    pub key_id: String,
    pub purpose: String,
    pub public_key: String,
    pub signature: String,
}

/// A checkpoint as it is stored, printed, and held externally as an
/// anchor. `tree_size` + `root_hash` are the cryptographic commitment;
/// `checkpoint_hash` is this checkpoint's identity in the chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub tree_size: i64,
    pub root_hash: String,
    pub prev_checkpoint_hash: Option<String>,
    pub checkpoint_hash: String,
    /// Tree-head attestations; empty (and omitted from JSON) when the
    /// checkpoint is unsigned, which stays valid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signatures: Vec<TreeHeadSignature>,
}

/// Outcome of [`create_checkpoint`]. Both variants carry a full
/// [`Checkpoint`] flattened under a `status` tag, so the command's output
/// is *always* a usable anchor: `checkpoint > anchor.json` is safe even on
/// a no-op run (it re-prints the current head rather than a stub).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CheckpointOutcome {
    /// A new checkpoint was recorded over a longer prefix.
    Created(Checkpoint),
    /// The stable prefix had not grown since the latest checkpoint; the
    /// existing head is returned unchanged.
    NoNewRows(Checkpoint),
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
    /// An evidence pack could not be parsed into a checkable tree. Only
    /// the offline `audit verify-pack` path produces this; the live
    /// `verify` reads structured rows from the database and never does.
    MalformedPack { detail: String },
    /// A checkpoint carries a signature that does not verify over its tree
    /// head (a corrupted signature, or a signed checkpoint whose fields
    /// were altered without re-signing). This proves the attestation is
    /// genuine; whether the signing key is *authorised* is a separate
    /// judgment (the keys-as-claims layer).
    SignatureInvalid {
        tree_size: i64,
        key_id: String,
        purpose: String,
        public_key: String,
    },
    /// A checkpoint carries a genuine signature, but the signing key was
    /// not authorised (no admitted `AuditSigningKey` claim for that exact
    /// `(key_id, purpose, public_key)`) as of the checkpoint's prefix. The
    /// signature is real; the signer was not permitted at that point.
    UnauthorizedKey {
        tree_size: i64,
        key_id: String,
        purpose: String,
        public_key: String,
    },
    /// `--require-signatures` was asked for, the tree is otherwise intact,
    /// but this checkpoint carries no signature. A policy verdict the
    /// verifier opts into (compliance mode), not an intrinsic tamper - an
    /// unsigned checkpoint is valid by default.
    SignatureRequired { tree_size: i64 },
}

/// This checkpoint's identity hash: `SHA-256(tree_size_le ||
/// root_hash_bytes || prev_bytes)`, rendered `sha256:<hex>`. A genesis
/// checkpoint hashes the empty string for `prev`.
pub(crate) fn checkpoint_hash(tree_size: i64, root_hash: &str, prev: Option<&str>) -> String {
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
        r#"SELECT tree_size, root_hash, prev_checkpoint_hash, checkpoint_hash,
                  signatures as "signatures: sqlx::types::Json<Vec<TreeHeadSignature>>"
           FROM morpholog.audit_checkpoints
           ORDER BY tree_size DESC
           LIMIT 1"#,
    )
    .fetch_optional(&mut *conn)
    .await
    .map_err(classify_checked_query)?;
    Ok(row.map(|r| Checkpoint {
        tree_size: r.tree_size,
        root_hash: r.root_hash,
        prev_checkpoint_hash: r.prev_checkpoint_hash,
        checkpoint_hash: r.checkpoint_hash,
        signatures: r.signatures.0,
    }))
}

/// Record a checkpoint over the current watermark-stable prefix of the
/// audit log. The heavy root computation runs under `SERIALIZABLE READ
/// Attest a tree head with the signer's key for the audit-checkpoint
/// purpose. The signature is deterministic (Ed25519), so re-signing the
/// same head with the same key yields the same bytes.
fn make_signature(signer: &CheckpointSigner, head: &signing::TreeHead<'_>) -> TreeHeadSignature {
    let sig = signing::sign_tree_head(&signer.key, AUDIT_CHECKPOINT_PURPOSE, &signer.key_id, head);
    TreeHeadSignature {
        key_id: signer.key_id.clone(),
        purpose: AUDIT_CHECKPOINT_PURPOSE.to_string(),
        public_key: signing::render_public_key(&signer.key.verifying_key()),
        signature: signing::render_signature(&sig),
    }
}

/// ONLY DEFERRABLE` (zero SSI footprint); the short append takes a
/// transaction advisory lock and re-reads the chain head, so concurrent
/// runs cannot fork it. When the stable prefix has not grown it records
/// no new tree, but with a signer it still attaches the attestation to
/// the existing head (a monotonic addition, de-duplicated on re-sign).
pub async fn create_checkpoint(
    pool: &PgPool,
    signer: Option<&CheckpointSigner>,
    writers: Option<&[String]>,
) -> Result<CheckpointOutcome, PgError> {
    // Watermark first, then the deferrable read - the lossless-resume
    // ordering: only rows below the horizon are stable enough that no
    // in-flight writer can later insert inside the prefix.
    let horizon = crate::audit::audit_resume_watermark(pool, writers).await?;

    let mut read_tx = begin_isolated_tx(pool, TxIsolation::SerializableReadOnlyDeferrable).await?;
    let (leaves, last) = collect_leaves(&mut read_tx, Some(horizon), None).await?;
    let tree_size = leaves.len() as i64;
    // When signing, fold the same prefix to resolve authority - within the
    // one deferrable snapshot, so the check and the leaves agree.
    let signer_rows = match signer {
        Some(_) => Some(load_audit_rows(&mut read_tx, tree_size).await?),
        None => None,
    };
    read_tx.commit().await.map_err(classify_checked_query)?;

    let root_hash = render_hash(&merkle_root(&leaves));

    // Refuse to sign with a key the ledger has not authorised as of this
    // prefix - signing must not produce a checkpoint that verification
    // would then reject as `UnauthorizedKey`. (The no-new-rows path signs
    // the existing head, whose tree_size equals this prefix's.)
    if let (Some(s), Some(rows)) = (signer, &signer_rows) {
        let triple = (
            s.key_id.clone(),
            AUDIT_CHECKPOINT_PURPOSE.to_string(),
            signing::render_public_key(&s.key.verifying_key()),
        );
        if !crate::keys::authorized_keys_as_of(rows, tree_size).contains(&triple) {
            return Err(PgError::InvalidState(format!(
                "signing key is not authorised as AuditSigningKey({}, {}, {}) as of tree_size {}",
                triple.0, triple.1, triple.2, tree_size
            )));
        }
    }

    let mut tx = pool.begin().await.map_err(classify_checked_query)?;
    sqlx::query!("SELECT pg_advisory_xact_lock($1)", CHECKPOINT_LOCK_KEY)
        .execute(&mut *tx)
        .await
        .map_err(classify_checked_query)?;

    let prev = latest_checkpoint(&mut tx).await?;
    if let Some(p) = &prev
        && tree_size <= p.tree_size
    {
        // No new rows. With a signer, attach its attestation to the
        // existing head instead of returning it unsigned - signing an
        // already-recorded checkpoint does not fork the tree (the
        // signature is not part of `checkpoint_hash`). An exact re-sign
        // is de-duplicated.
        let outcome = match signer {
            Some(s) => {
                let head = signing::TreeHead {
                    tree_size: p.tree_size,
                    root_hash: &p.root_hash,
                    prev_checkpoint_hash: p.prev_checkpoint_hash.as_deref(),
                    checkpoint_hash: &p.checkpoint_hash,
                };
                let new_sig = make_signature(s, &head);
                let mut signatures = p.signatures.clone();
                if !signatures.iter().any(|x| {
                    x.key_id == new_sig.key_id
                        && x.purpose == new_sig.purpose
                        && x.public_key == new_sig.public_key
                }) {
                    signatures.push(new_sig);
                    sqlx::query!(
                        "UPDATE morpholog.audit_checkpoints
                         SET signatures = $1 WHERE checkpoint_hash = $2",
                        sqlx::types::Json(&signatures) as _,
                        p.checkpoint_hash,
                    )
                    .execute(&mut *tx)
                    .await
                    .map_err(classify_checked_query)?;
                }
                tx.commit().await.map_err(classify_checked_query)?;
                Checkpoint {
                    signatures,
                    ..p.clone()
                }
            }
            None => {
                tx.rollback().await.map_err(classify_checked_query)?;
                p.clone()
            }
        };
        return Ok(CheckpointOutcome::NoNewRows(outcome));
    }

    let prev_hash = prev.as_ref().map(|p| p.checkpoint_hash.clone());
    let cp_hash = checkpoint_hash(tree_size, &root_hash, prev_hash.as_deref());
    let (last_tid, last_at) = match last {
        Some((tid, at)) => (Some(tid), Some(at)),
        None => (None, None),
    };

    // Sign the new tree head if a key was supplied - the signer's
    // authority as of this prefix was already checked above, so an
    // unauthorised key never reaches this attestation.
    let signatures: Vec<TreeHeadSignature> = match signer {
        Some(s) => {
            let head = signing::TreeHead {
                tree_size,
                root_hash: &root_hash,
                prev_checkpoint_hash: prev_hash.as_deref(),
                checkpoint_hash: &cp_hash,
            };
            vec![make_signature(s, &head)]
        }
        None => Vec::new(),
    };

    sqlx::query!(
        "INSERT INTO morpholog.audit_checkpoints (
            checkpoint_id, tree_size, root_hash, prev_checkpoint_hash,
            checkpoint_hash, covered_until, last_transition_id, last_committed_at,
            signatures
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        Uuid::now_v7(),
        tree_size,
        root_hash,
        prev_hash,
        cp_hash,
        horizon,
        last_tid,
        last_at,
        sqlx::types::Json(&signatures) as _,
    )
    .execute(&mut *tx)
    .await
    .map_err(classify_checked_query)?;
    tx.commit().await.map_err(classify_checked_query)?;

    Ok(CheckpointOutcome::Created(Checkpoint {
        tree_size,
        root_hash,
        prev_checkpoint_hash: prev_hash,
        checkpoint_hash: cp_hash,
        signatures,
    }))
}

/// Verify the audit tree against its checkpoints. Reads under
/// `SERIALIZABLE READ ONLY DEFERRABLE`. Checks, strongest last: every
/// checkpoint's root recomputes from the current log; the checkpoint
/// chain is internally consistent (hash + `prev` links); and, if `anchor`
/// is supplied, the stored checkpoint at the anchor's size matches the
/// externally held copy.
/// Load the whole checkpoint chain, ascending by size - the read shared by
/// the live verifier and pack export.
pub(crate) async fn load_checkpoint_chain(
    conn: &mut sqlx::PgConnection,
) -> Result<Vec<Checkpoint>, PgError> {
    let stored = sqlx::query!(
        r#"SELECT tree_size, root_hash, prev_checkpoint_hash, checkpoint_hash,
                  signatures as "signatures: sqlx::types::Json<Vec<TreeHeadSignature>>"
           FROM morpholog.audit_checkpoints
           ORDER BY tree_size ASC"#,
    )
    .fetch_all(conn)
    .await
    .map_err(classify_checked_query)?;
    Ok(stored
        .into_iter()
        .map(|r| Checkpoint {
            tree_size: r.tree_size,
            root_hash: r.root_hash,
            prev_checkpoint_hash: r.prev_checkpoint_hash,
            checkpoint_hash: r.checkpoint_hash,
            signatures: r.signatures.0,
        })
        .collect())
}

pub async fn verify_audit_tree(
    pool: &PgPool,
    anchor: Option<Checkpoint>,
) -> Result<TreeVerification, PgError> {
    let mut tx = begin_isolated_tx(pool, TxIsolation::SerializableReadOnlyDeferrable).await?;

    let checkpoints = load_checkpoint_chain(&mut tx).await?;

    let max_size = checkpoints.last().map(|c| c.tree_size).unwrap_or(0);
    let (leaves, _) = collect_leaves(&mut tx, None, Some(max_size)).await?;

    // `verify_tree` is the authoritative structural + crypto check,
    // including the anchor match and the anchor's own signatures.
    let verdict = verify_tree(&leaves, &checkpoints, anchor.as_ref());
    // A structurally intact, genuinely signed tree still has to answer the
    // authority question: was each signing key admitted as of its prefix?
    // The supplied anchor is judged the same way. Rows are loaded only when
    // there are signatures to judge.
    let signed = |c: &Checkpoint| !c.signatures.is_empty();
    if matches!(verdict, TreeVerification::Intact { .. })
        && (checkpoints.iter().any(signed) || anchor.as_ref().is_some_and(signed))
    {
        let rows = load_audit_rows(&mut tx, max_size).await?;
        if let Some(violation) = authority_violation(&checkpoints, anchor.as_ref(), &rows) {
            return Ok(violation);
        }
    }
    Ok(verdict)
}

/// The smallest `tree_size` of a checkpoint that carries no signature, or
/// `None` if every checkpoint is signed (or there are none). The
/// `--require-signatures` compliance policy reads this; signing stays
/// opt-in by default, so this is not part of the intrinsic verdict.
pub async fn first_unsigned_checkpoint_size(pool: &PgPool) -> Result<Option<i64>, PgError> {
    let row = sqlx::query!(
        r#"SELECT MIN(tree_size) AS "size"
           FROM morpholog.audit_checkpoints
           WHERE signatures = '[]'::jsonb"#
    )
    .fetch_one(pool)
    .await
    .map_err(classify_checked_query)?;
    Ok(row.size)
}

/// The pure tamper-evidence check shared by [`verify_audit_tree`] (live,
/// against Postgres) and the offline pack verifier: given the log's leaf
/// hashes in canonical order and the checkpoint chain, confirm every
/// Tree-head identity: the cryptographic commitment, *excluding* the
/// signatures attached to it. Two checkpoints with the same tree head but
/// different signatures are the same commitment, so the anchor check must
/// compare heads, not whole artefacts - otherwise a signature difference
/// reads as a mismatch whose two `checkpoint_hash`es are identical.
pub(crate) fn same_tree_head(a: &Checkpoint, b: &Checkpoint) -> bool {
    a.tree_size == b.tree_size
        && a.root_hash == b.root_hash
        && a.prev_checkpoint_hash == b.prev_checkpoint_hash
        && a.checkpoint_hash == b.checkpoint_hash
}

/// checkpoint's root recomputes from the leaves, the chain is internally
/// consistent, and (if supplied) the anchor matches the stored checkpoint
/// at its size. One core, so the offline verifier cannot drift from the
/// live one.
pub(crate) fn verify_tree(
    leaves: &[Hash],
    checkpoints: &[Checkpoint],
    anchor: Option<&Checkpoint>,
) -> TreeVerification {
    // Anchor check first: a coordinated rewrite is internally consistent,
    // so only the external copy can expose it.
    if let Some(anchor) = anchor {
        let stored_at_size = checkpoints.iter().find(|c| c.tree_size == anchor.tree_size);
        if !stored_at_size.is_some_and(|c| same_tree_head(c, anchor)) {
            return TreeVerification::AnchorMismatch {
                tree_size: anchor.tree_size,
                anchor_checkpoint_hash: anchor.checkpoint_hash.clone(),
                stored_checkpoint_hash: stored_at_size.map(|c| c.checkpoint_hash.clone()),
            };
        }
        // The signed anchor is the attestation the operator actually held
        // outside the database, so its own signatures must verify - a
        // stored checkpoint with its signatures stripped must not let an
        // anchor's signature go unchecked. (Authority is judged below.)
        if let Some(violation) = signature_crypto_violation(anchor) {
            return violation;
        }
    }

    let mut prev_hash: Option<&str> = None;
    for cp in checkpoints {
        // Chain integrity: the recorded checkpoint_hash must match its
        // contents, and prev must link to the previous checkpoint.
        let expected = checkpoint_hash(
            cp.tree_size,
            &cp.root_hash,
            cp.prev_checkpoint_hash.as_deref(),
        );
        if expected != cp.checkpoint_hash {
            return TreeVerification::ChainBroken {
                detail: format!(
                    "checkpoint at tree_size {} has hash {} but its contents hash to {expected}",
                    cp.tree_size, cp.checkpoint_hash
                ),
            };
        }
        if cp.prev_checkpoint_hash.as_deref() != prev_hash {
            return TreeVerification::ChainBroken {
                detail: format!(
                    "checkpoint at tree_size {} links to prev {:?}, expected {:?}",
                    cp.tree_size, cp.prev_checkpoint_hash, prev_hash
                ),
            };
        }
        prev_hash = Some(&cp.checkpoint_hash);

        // Tamper check: recompute the root over the log prefix.
        let size = cp.tree_size as usize;
        if size > leaves.len() {
            return TreeVerification::Tampered {
                tree_size: cp.tree_size,
                recorded_root: cp.root_hash.clone(),
                recomputed_root: format!("only {} rows present", leaves.len()),
            };
        }
        let recomputed = render_hash(&merkle_root(&leaves[..size]));
        if recomputed != cp.root_hash {
            return TreeVerification::Tampered {
                tree_size: cp.tree_size,
                recorded_root: cp.root_hash.clone(),
                recomputed_root: recomputed,
            };
        }

        // Signature integrity: every attestation present must verify over
        // this tree head. A signature that does not is corruption or a
        // signed checkpoint altered without re-signing. (Whether the key
        // is *authorised* is the keys-as-claims layer's judgment.)
        if let Some(violation) = signature_crypto_violation(cp) {
            return violation;
        }
    }

    TreeVerification::Intact {
        checkpoints: checkpoints.len(),
        tree_size: checkpoints.last().map(|c| c.tree_size).unwrap_or(0),
    }
}

/// The cryptographic half of signature checking for one checkpoint: every
/// attached signature must verify over the checkpoint's tree head, else
/// `SignatureInvalid`. Pure; whether the key is *authorised* is
/// [`authority_violation`]'s separate judgment.
pub(crate) fn signature_crypto_violation(cp: &Checkpoint) -> Option<TreeVerification> {
    let head = signing::TreeHead {
        tree_size: cp.tree_size,
        root_hash: &cp.root_hash,
        prev_checkpoint_hash: cp.prev_checkpoint_hash.as_deref(),
        checkpoint_hash: &cp.checkpoint_hash,
    };
    for sig in &cp.signatures {
        let valid = match (
            signing::parse_public_key(&sig.public_key),
            signing::parse_signature(&sig.signature),
        ) {
            (Ok(pk), Ok(parsed)) => {
                signing::verify_tree_head(&pk, &parsed, &sig.purpose, &sig.key_id, &head)
            }
            _ => false,
        };
        if !valid {
            return Some(TreeVerification::SignatureInvalid {
                tree_size: cp.tree_size,
                key_id: sig.key_id.clone(),
                purpose: sig.purpose.clone(),
                public_key: sig.public_key.clone(),
            });
        }
    }
    None
}

/// The keys-as-claims authority check, run on a tree that is already
/// structurally intact and whose signatures are genuine: for each signed
/// checkpoint - and the supplied `anchor`, judged the same way - every
/// signature's `(key_id, purpose, public_key)` must match an
/// `AuditSigningKey` claim in force as of that checkpoint's prefix (the
/// `rows`, canonical order). Returns the first violation, or `None` if
/// every signature is by an authorised key. Pure - the live and offline
/// verifiers pass their own rows.
pub(crate) fn authority_violation(
    checkpoints: &[Checkpoint],
    anchor: Option<&Checkpoint>,
    rows: &[AuditRow],
) -> Option<TreeVerification> {
    for cp in checkpoints.iter().chain(anchor) {
        if cp.signatures.is_empty() {
            continue;
        }
        let authorized = crate::keys::authorized_keys_as_of(rows, cp.tree_size);
        for sig in &cp.signatures {
            let triple = (
                sig.key_id.clone(),
                sig.purpose.clone(),
                sig.public_key.clone(),
            );
            if !authorized.contains(&triple) {
                return Some(TreeVerification::UnauthorizedKey {
                    tree_size: cp.tree_size,
                    key_id: sig.key_id.clone(),
                    purpose: sig.purpose.clone(),
                    public_key: sig.public_key.clone(),
                });
            }
        }
    }
    None
}

/// Read the audit rows of the first `max` of the canonical prefix - what
/// the authority check folds for in-force signing keys.
async fn load_audit_rows(
    conn: &mut sqlx::PgConnection,
    max: i64,
) -> Result<Vec<AuditRow>, PgError> {
    let mut rows = Vec::new();
    let mut cursor: Option<(DateTime<Utc>, Uuid)> = None;
    while (rows.len() as i64) < max {
        let page = list_audit_rows_page(conn, cursor, None, REPLAY_CHUNK).await?;
        if page.is_empty() {
            break;
        }
        for row in page {
            cursor = Some((row.committed_at, row.transition_id));
            rows.push(row);
            if rows.len() as i64 >= max {
                break;
            }
        }
    }
    Ok(rows)
}
