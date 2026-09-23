//! Tamper-evident audit checkpoints.
//!
//! A checkpoint commits to a prefix of the audit log: the RFC 6962 Merkle
//! root of the first `tree_size` rows in `(committed_at, transition_id)`
//! order. Each checkpoint links to the previous one
//! (`prev_checkpoint_hash`), so the table is itself append-only.
//!
//! **Threat model.** Recomputing the root catches an edit to `audit` (or
//! `claims`, via [`crate::verify_replay`]) by someone who did *not* also
//! rewrite `audit_checkpoints`. An attacker with full write access can
//! rewrite both into a self-consistent false history. The real trust
//! anchor is a checkpoint that has **left the database**, held externally;
//! [`verify_audit_tree`] fails if the stored checkpoint at that size
//! disagrees. The chain raises the cost of forgery; the external anchor
//! makes tampering provable.
//!
//! **Signing (see [`crate::signing`]).** A checkpoint may carry Ed25519
//! signatures over its tree head. A rewrite cannot be re-signed without
//! the private key, so a verifier with the trusted public key catches it
//! even without an anchor. Verification checks each signature is genuine
//! ([`TreeVerification::SignatureInvalid`]) and that its key was
//! authorised by an admitted `AuditSigningKey` claim for that exact
//! `(key_id, purpose, public_key)` as of the signed prefix
//! ([`TreeVerification::UnauthorizedKey`]; see [`crate::keys`]). This
//! makes key authority governed and revocable, but it is not a root of
//! trust: the first authorisation is trusted the way the schema is.

use ed25519_dalek::SigningKey;
use jiff::Timestamp;
use jiff_sqlx::ToSqlx;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::audit::AuditRow;
use crate::audit_pages::AuditPages;
use crate::error::{PgError, classify, classify_checked_query};
use crate::merkle::{Digest, Hash, audit_leaf_hash, merkle_root};
use crate::signing;
use crate::txn::{TxIsolation, begin_isolated_tx};

/// What an `AuditSigningKey` claim authorises a key for. Bound into the
/// signed payload, so a checkpoint key cannot sign another kind of
/// artefact by accident. The authority check matches this exact value.
pub const AUDIT_CHECKPOINT_PURPOSE: &str = "audit_checkpoint_v1";

/// A signing identity for [`create_checkpoint`]: the private key and the
/// `key_id` it is published under. The key never enters the database.
pub struct CheckpointSigner {
    pub key_id: String,
    pub key: SigningKey,
}

/// Advisory-lock key serialising checkpoint creation, so two concurrent
/// runs cannot fork the chain.
const CHECKPOINT_LOCK_KEY: i64 = 0x4D4F_5250_4F4C_4701; // "MORPOLG\x01"

/// One Ed25519 signature over a tree head, with its signer (`key_id`,
/// `public_key`) and `purpose`. Key and signature are rendered
/// `ed25519-pub:`/`ed25519-sig:` hex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TreeHeadSignature {
    pub key_id: String,
    pub purpose: String,
    pub public_key: String,
    pub signature: String,
}

/// Which external scheme a witness proof comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WitnessScheme {
    Rfc3161,
}

/// One external witness to a tree head: the exact bytes a timestamp
/// authority returned, never re-encoded, and where they came from.
///
/// Nothing derived (attested time, validity) is stored: a witness sits
/// outside `checkpoint_hash`, so a stored derivation could be edited
/// freely. The verifier reads both from the proof. Byte-identical proofs
/// are one witness whatever URL served them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Witness {
    pub scheme: WitnessScheme,
    /// The scheme's proof, base64 (standard alphabet, padded): for RFC 3161
    /// the whole `TimeStampResp` as received.
    pub proof: String,
    /// Provenance, not identity: the endpoint the proof came from.
    pub submitted_to: String,
}

/// A checkpoint as it is stored, printed, and held externally as an
/// anchor. `tree_size` + `root_hash` are the cryptographic commitment;
/// `checkpoint_hash` is this checkpoint's identity in the chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub tree_size: i64,
    pub root_hash: Digest,
    pub prev_checkpoint_hash: Option<Digest>,
    pub checkpoint_hash: Digest,
    /// Tree-head attestations; empty (and omitted from JSON) when the
    /// checkpoint is unsigned, which stays valid.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub signatures: Vec<TreeHeadSignature>,
    /// External witnesses to the head; empty (and omitted from JSON) when
    /// none was obtained, which stays valid. A witness adds "existed no
    /// later than T"; it never changes the tree's own verdict.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub witnesses: Vec<Witness>,
}

/// Outcome of [`create_checkpoint`]. Both variants carry a full
/// [`Checkpoint`], so the output is always a usable anchor, even on a
/// no-op run.
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
    /// from the current log: the log was edited under a checkpoint.
    Tampered {
        tree_size: i64,
        recorded_root: Digest,
        /// The recomputed digest, or a sentence saying the log is shorter
        /// than the checkpoint claims.
        recomputed_root: String,
    },
    /// The checkpoint chain is internally inconsistent (a hash does not
    /// match its contents, or a `prev` link is broken): the checkpoint
    /// table itself was edited.
    ChainBroken { detail: String },
    /// An externally held anchor disagrees with the stored checkpoint at
    /// its tree size. The strongest signal: it catches a self-consistent
    /// rewrite of audit and checkpoints together.
    AnchorMismatch {
        tree_size: i64,
        anchor_checkpoint_hash: Digest,
        stored_checkpoint_hash: Option<Digest>,
    },
    /// An evidence pack could not be parsed into a checkable tree. Only
    /// the offline pack verifier produces this.
    MalformedPack { detail: String },
    /// A checkpoint carries a signature that does not verify over its tree
    /// head: corrupted, or the checkpoint was altered without re-signing.
    /// Whether a key is *authorised* is a separate check.
    SignatureInvalid {
        tree_size: i64,
        key_id: String,
        purpose: String,
        public_key: String,
    },
    /// A genuine signature by a key with no admitted `AuditSigningKey`
    /// claim for that exact `(key_id, purpose, public_key)` as of the
    /// checkpoint's prefix.
    UnauthorizedKey {
        tree_size: i64,
        key_id: String,
        purpose: String,
        public_key: String,
    },
    /// The verifier requires signatures, the tree is otherwise intact, but
    /// this checkpoint is unsigned. A policy verdict the verifier opts
    /// into, not tampering: unsigned checkpoints are valid by default.
    SignatureRequired { tree_size: i64 },
    /// The verifier pinned a signing key, the tree is otherwise intact
    /// (every signature genuine and authorised), but this checkpoint has
    /// no signature by that key. The pin only narrows which authorised
    /// signers are accepted; it never admits an unauthorised one.
    SigningKeyRequired { tree_size: i64, public_key: String },
}

/// What a verifier requires of checkpoint signatures beyond the intrinsic
/// verdict: from which tree size checkpoints must be signed, and by which
/// key if one is pinned.
///
/// Applied only to an intact tree, so every signature it sees is already
/// genuine and authorised. A pin narrows that set; it never replaces the
/// authority check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignaturePolicy {
    /// Checkpoints at or after this tree size must satisfy the policy;
    /// earlier ones (history from before signing began) are exempt. Zero
    /// covers every checkpoint.
    pub from_tree_size: i64,
    /// The `ed25519-pub:<hex>` key at least one signature must be by.
    pub required_public_key: Option<String>,
}

/// The first policy failure over a checkpoint sequence, lowest tree size
/// first: the verdict a verifier reports in place of `Intact`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignaturePolicyViolation {
    SignatureRequired { tree_size: i64 },
    SigningKeyRequired { tree_size: i64, public_key: String },
}

impl SignaturePolicy {
    /// The first checkpoint at or after the threshold with no signature.
    /// The one check a verifier that cannot judge key authority (a window
    /// or selective pack) can still apply.
    pub fn unsigned_at_or_after<'a>(
        &self,
        checkpoints: impl IntoIterator<Item = &'a Checkpoint>,
    ) -> Option<i64> {
        checkpoints
            .into_iter()
            .filter(|c| c.tree_size >= self.from_tree_size && c.signatures.is_empty())
            .map(|c| c.tree_size)
            .min()
    }

    /// The first policy failure over checkpoints whose signatures are
    /// already proven genuine and authorised. A missing signature
    /// outranks a missing pinned key at the same checkpoint; across
    /// checkpoints the lowest tree size wins.
    pub fn violation<'a>(
        &self,
        checkpoints: impl IntoIterator<Item = &'a Checkpoint>,
    ) -> Option<SignaturePolicyViolation> {
        checkpoints
            .into_iter()
            .filter(|c| c.tree_size >= self.from_tree_size)
            .filter_map(|c| {
                if c.signatures.is_empty() {
                    return Some(SignaturePolicyViolation::SignatureRequired {
                        tree_size: c.tree_size,
                    });
                }
                let key = self.required_public_key.as_ref()?;
                if c.signatures.iter().any(|s| &s.public_key == key) {
                    return None;
                }
                Some(SignaturePolicyViolation::SigningKeyRequired {
                    tree_size: c.tree_size,
                    public_key: key.clone(),
                })
            })
            .min_by_key(|v| match v {
                SignaturePolicyViolation::SignatureRequired { tree_size }
                | SignaturePolicyViolation::SigningKeyRequired { tree_size, .. } => *tree_size,
            })
    }
}

/// The checkpoints with the anchor's signatures merged into the one at
/// the anchor's tree size. An intact verdict has already proven they
/// match, so a stripped database copy beside a signed anchor still counts
/// as signed.
pub fn with_anchor_signatures(
    checkpoints: &[Checkpoint],
    anchor: Option<&Checkpoint>,
) -> Vec<Checkpoint> {
    checkpoints
        .iter()
        .map(|c| {
            let mut merged = c.clone();
            if let Some(a) = anchor
                && a.tree_size == c.tree_size
            {
                for sig in &a.signatures {
                    if !merged.signatures.contains(sig) {
                        merged.signatures.push(sig.clone());
                    }
                }
            }
            merged
        })
        .collect()
}

impl From<SignaturePolicyViolation> for TreeVerification {
    fn from(v: SignaturePolicyViolation) -> Self {
        match v {
            SignaturePolicyViolation::SignatureRequired { tree_size } => {
                TreeVerification::SignatureRequired { tree_size }
            }
            SignaturePolicyViolation::SigningKeyRequired {
                tree_size,
                public_key,
            } => TreeVerification::SigningKeyRequired {
                tree_size,
                public_key,
            },
        }
    }
}

/// This checkpoint's identity hash: `SHA-256(tree_size_le ||
/// root_hash_bytes || prev_bytes)`, rendered `sha256:<hex>`. A genesis
/// checkpoint hashes the empty string for `prev`.
pub(crate) fn checkpoint_hash(tree_size: i64, root_hash: &Digest, prev: Option<&Digest>) -> Digest {
    let mut h = Sha256::new();
    h.update(tree_size.to_le_bytes());
    h.update(root_hash.to_string().as_bytes());
    h.update(prev.map(ToString::to_string).unwrap_or_default().as_bytes());
    Digest::from_bytes(h.finalize().into())
}

/// Parse a stored hash. The runtime wrote it, so a malformed one is
/// refused as impossible state.
fn stored_digest(text: &str) -> Result<Digest, PgError> {
    text.parse()
        .map_err(|e| PgError::InvalidState(format!("a stored checkpoint hash is malformed: {e}")))
}

/// Page the audit log in canonical order, hashing each row to its leaf.
/// `horizon` bounds by `committed_at`; `max` stops after that many rows.
/// Returns the leaves and the last row's coordinates.
async fn collect_leaves(
    conn: &mut sqlx::PgConnection,
    horizon: Option<Timestamp>,
    max: Option<i64>,
) -> Result<(Vec<[u8; 32]>, Option<(Uuid, Timestamp)>), PgError> {
    let mut leaves = Vec::new();
    let mut last = None;
    let mut pages = AuditPages::new(horizon);
    loop {
        let page = pages.next(conn).await?;
        if page.is_empty() {
            break;
        }
        for row in &page {
            leaves.push(audit_leaf_hash(row)?);
            last = Some((row.transition_id, row.committed_at));
            if max.is_some_and(|m| leaves.len() as i64 >= m) {
                return Ok((leaves, last));
            }
        }
    }
    Ok((leaves, last))
}

/// A checkpoint row as stored, digests as text.
struct StoredCheckpoint {
    tree_size: i64,
    root_hash: String,
    prev_checkpoint_hash: Option<String>,
    checkpoint_hash: String,
    signatures: sqlx::types::Json<Vec<TreeHeadSignature>>,
    witnesses: sqlx::types::Json<Vec<Witness>>,
}

impl TryFrom<StoredCheckpoint> for Checkpoint {
    type Error = PgError;

    fn try_from(r: StoredCheckpoint) -> Result<Self, PgError> {
        Ok(Checkpoint {
            tree_size: r.tree_size,
            root_hash: stored_digest(&r.root_hash)?,
            prev_checkpoint_hash: r
                .prev_checkpoint_hash
                .as_deref()
                .map(stored_digest)
                .transpose()?,
            checkpoint_hash: stored_digest(&r.checkpoint_hash)?,
            signatures: r.signatures.0,
            witnesses: r.witnesses.0,
        })
    }
}

/// The latest checkpoint (highest `tree_size`), or `None` if the chain is
/// empty.
async fn latest_checkpoint(conn: &mut sqlx::PgConnection) -> Result<Option<Checkpoint>, PgError> {
    let row = sqlx::query_as!(
        StoredCheckpoint,
        r#"SELECT tree_size, root_hash, prev_checkpoint_hash, checkpoint_hash,
                  signatures as "signatures: sqlx::types::Json<Vec<TreeHeadSignature>>",
                  witnesses as "witnesses: sqlx::types::Json<Vec<Witness>>"
           FROM morpholog.audit_checkpoints
           ORDER BY tree_size DESC
           LIMIT 1"#,
    )
    .fetch_optional(&mut *conn)
    .await
    .map_err(classify_checked_query)?;
    row.map(Checkpoint::try_from).transpose()
}

/// Sign a tree head for the audit-checkpoint purpose. Ed25519 is
/// deterministic, so re-signing the same head gives the same bytes.
fn make_signature(signer: &CheckpointSigner, head: &signing::TreeHead<'_>) -> TreeHeadSignature {
    let sig = signing::sign_tree_head(&signer.key, AUDIT_CHECKPOINT_PURPOSE, &signer.key_id, head);
    TreeHeadSignature {
        key_id: signer.key_id.clone(),
        purpose: AUDIT_CHECKPOINT_PURPOSE.to_string(),
        public_key: signing::render_public_key(&signer.key.verifying_key()),
        signature: signing::render_signature(&sig),
    }
}

/// Record a checkpoint over the watermark-stable prefix of the audit log.
///
/// The root is computed under `SERIALIZABLE READ ONLY DEFERRABLE` (no SSI
/// footprint). The append takes an advisory lock and re-reads the chain
/// head, so concurrent runs cannot fork it. If the prefix has not grown,
/// no new tree is recorded, but a signer's signature is still added to
/// the existing head (deduplicated on re-sign).
pub async fn create_checkpoint(
    pool: &PgPool,
    signer: Option<&CheckpointSigner>,
    writers: Option<&[String]>,
) -> Result<CheckpointOutcome, PgError> {
    // Watermark before the snapshot: only below the horizon can no
    // in-flight writer still insert inside the prefix.
    let horizon = crate::audit::audit_resume_watermark(pool, writers).await?;

    let mut read_tx = begin_isolated_tx(pool, TxIsolation::SerializableReadOnlyDeferrable).await?;
    let (leaves, last) = collect_leaves(&mut read_tx, Some(horizon), None).await?;
    let tree_size = leaves.len() as i64;
    // When signing, judge authority over the same prefix in the same
    // snapshot, so the check and the leaves agree; on failure the withheld
    // row count comes from this snapshot too. The verdict is held, not
    // returned: whether this prefix is the head being signed is only known
    // once the head is read under the lock below.
    let candidate_refusal = match signer {
        Some(s) => {
            let rows = load_audit_rows(&mut read_tx, tree_size).await?;
            match signer_authority_violation(s, &rows, tree_size) {
                Some(refusal) => {
                    Some(with_horizon_diagnosis(refusal, &mut read_tx, horizon).await?)
                }
                None => None,
            }
        }
        None => None,
    };
    read_tx.commit().await.map_err(classify)?;

    let root_hash = Digest::from_bytes(merkle_root(&leaves));

    let mut tx = pool.begin().await.map_err(classify)?;
    sqlx::query!("SELECT pg_advisory_xact_lock($1)", CHECKPOINT_LOCK_KEY)
        .execute(&mut *tx)
        .await
        .map_err(classify_checked_query)?;

    let prev = latest_checkpoint(&mut tx).await?;
    if let Some(p) = &prev
        && tree_size <= p.tree_size
    {
        // No new rows. A signer's signature goes onto the existing head;
        // that cannot fork the tree, because signatures are not part of
        // `checkpoint_hash`. An exact re-sign is deduplicated.
        let outcome = match signer {
            Some(s) => {
                // Judge authority at the head that receives the signature,
                // not this snapshot's prefix: a concurrent run may have
                // advanced the head, and a key revoked in between must not
                // sign it. The head's rows are already checkpointed, so
                // this re-read cannot shift.
                if p.tree_size == tree_size {
                    if let Some(refusal) = candidate_refusal {
                        return Err(refusal);
                    }
                } else {
                    let rows = load_audit_rows(&mut tx, p.tree_size).await?;
                    if let Some(refusal) = signer_authority_violation(s, &rows, p.tree_size) {
                        // Committed rows beyond the head could carry an
                        // authorisation a later checkpoint would honour.
                        // If there are any, say the prefix was withheld
                        // rather than blaming the key.
                        let total =
                            sqlx::query!(r#"SELECT count(*) AS "count!" FROM morpholog.audit"#)
                                .fetch_one(&mut *tx)
                                .await
                                .map_err(classify_checked_query)?
                                .count;
                        let beyond_head = total - p.tree_size;
                        return Err(if beyond_head > 0 {
                            truncated_prefix_diagnosis(refusal, beyond_head, horizon)
                        } else {
                            refusal
                        });
                    }
                }
                let head = signing::TreeHead {
                    tree_size: p.tree_size,
                    root_hash: &p.root_hash,
                    prev_checkpoint_hash: p.prev_checkpoint_hash.as_ref(),
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
                        p.checkpoint_hash.to_string(),
                    )
                    .execute(&mut *tx)
                    .await
                    .map_err(classify_checked_query)?;
                }
                tx.commit().await.map_err(classify)?;
                Checkpoint {
                    signatures,
                    ..p.clone()
                }
            }
            None => {
                tx.rollback().await.map_err(classify)?;
                p.clone()
            }
        };
        return Ok(CheckpointOutcome::NoNewRows(outcome));
    }

    // The new head is this snapshot's prefix, so the held verdict applies.
    if let Some(refusal) = candidate_refusal {
        return Err(refusal);
    }

    let prev_hash = prev.as_ref().map(|p| p.checkpoint_hash);
    let cp_hash = checkpoint_hash(tree_size, &root_hash, prev_hash.as_ref());
    let (last_tid, last_at) = match last {
        Some((tid, at)) => (Some(tid), Some(at)),
        None => (None, None),
    };

    let signatures: Vec<TreeHeadSignature> = match signer {
        Some(s) => {
            let head = signing::TreeHead {
                tree_size,
                root_hash: &root_hash,
                prev_checkpoint_hash: prev_hash.as_ref(),
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
        root_hash.to_string(),
        prev_hash.map(|d| d.to_string()),
        cp_hash.to_string(),
        horizon.to_sqlx(),
        last_tid,
        last_at.map(ToSqlx::to_sqlx),
        sqlx::types::Json(&signatures) as _,
    )
    .execute(&mut *tx)
    .await
    .map_err(classify_checked_query)?;
    tx.commit().await.map_err(classify)?;

    Ok(CheckpointOutcome::Created(Checkpoint {
        tree_size,
        root_hash,
        prev_checkpoint_hash: prev_hash,
        checkpoint_hash: cp_hash,
        signatures,
        witnesses: Vec::new(),
    }))
}

/// Judge the signer against the `AuditSigningKey` claims in force as of
/// `tree_size`, returning the plain refusal when it is not authorised.
fn signer_authority_violation(
    signer: &CheckpointSigner,
    rows: &[AuditRow],
    tree_size: i64,
) -> Option<PgError> {
    let triple = (
        signer.key_id.clone(),
        AUDIT_CHECKPOINT_PURPOSE.to_string(),
        signing::render_public_key(&signer.key.verifying_key()),
    );
    if crate::keys::authorized_keys_as_of(rows, tree_size).contains(&triple) {
        None
    } else {
        Some(PgError::SigningKeyUnauthorised {
            key_id: triple.0,
            purpose: triple.1,
            public_key: triple.2,
            tree_size,
        })
    }
}

/// Turn an authority refusal into the truncated-prefix one: the judged
/// prefix is not the whole log, so the authorisation may sit in the
/// withheld rows. Leaves any other error alone.
fn truncated_prefix_diagnosis(
    refusal: PgError,
    committed_beyond_horizon: i64,
    horizon: Timestamp,
) -> PgError {
    match refusal {
        PgError::SigningKeyUnauthorised {
            key_id,
            purpose,
            public_key,
            tree_size,
        } => PgError::SigningKeyUnauthorisedAtTruncatedPrefix {
            key_id,
            purpose,
            public_key,
            tree_size,
            committed_beyond_horizon,
            horizon,
        },
        other => other,
    }
}

/// Count the committed rows the horizon withheld from this snapshot, and
/// upgrade the refusal if there are any.
async fn with_horizon_diagnosis(
    refusal: PgError,
    conn: &mut sqlx::PgConnection,
    horizon: Timestamp,
) -> Result<PgError, PgError> {
    // `>=` mirrors the pager's strict `<`: a row at the horizon is
    // withheld. count(*) is never NULL.
    let committed_beyond_horizon = sqlx::query!(
        r#"SELECT count(*) AS "committed_beyond_horizon!"
           FROM morpholog.audit WHERE committed_at >= $1"#,
        horizon.to_sqlx(),
    )
    .fetch_one(conn)
    .await
    .map_err(classify_checked_query)?
    .committed_beyond_horizon;
    Ok(if committed_beyond_horizon > 0 {
        truncated_prefix_diagnosis(refusal, committed_beyond_horizon, horizon)
    } else {
        refusal
    })
}

/// Load the whole checkpoint chain, ascending by size.
pub(crate) async fn load_checkpoint_chain(
    conn: &mut sqlx::PgConnection,
) -> Result<Vec<Checkpoint>, PgError> {
    let stored = sqlx::query_as!(
        StoredCheckpoint,
        r#"SELECT tree_size, root_hash, prev_checkpoint_hash, checkpoint_hash,
                  signatures as "signatures: sqlx::types::Json<Vec<TreeHeadSignature>>",
                  witnesses as "witnesses: sqlx::types::Json<Vec<Witness>>"
           FROM morpholog.audit_checkpoints
           ORDER BY tree_size ASC"#,
    )
    .fetch_all(conn)
    .await
    .map_err(classify_checked_query)?;
    stored.into_iter().map(Checkpoint::try_from).collect()
}

/// The checkpoint at exactly `tree_size`, if one was recorded there.
pub async fn load_checkpoint(pool: &PgPool, tree_size: i64) -> Result<Option<Checkpoint>, PgError> {
    let row = sqlx::query_as!(
        StoredCheckpoint,
        r#"SELECT tree_size, root_hash, prev_checkpoint_hash, checkpoint_hash,
                  signatures as "signatures: sqlx::types::Json<Vec<TreeHeadSignature>>",
                  witnesses as "witnesses: sqlx::types::Json<Vec<Witness>>"
           FROM morpholog.audit_checkpoints
           WHERE tree_size = $1"#,
        tree_size,
    )
    .fetch_optional(pool)
    .await
    .map_err(classify_checked_query)?;
    row.map(Checkpoint::try_from).transpose()
}

/// Attach an external witness to the checkpoint at `tree_size`.
///
/// The checkpoint must still have `checkpoint_hash`: the proof was
/// obtained outside any transaction, and the chain may have been rebuilt
/// since. A proof already present is not stored twice, and nothing is
/// ever removed; the row lock keeps that true for concurrent attachments.
/// Returns the checkpoint as now stored.
pub async fn attach_witness(
    pool: &PgPool,
    tree_size: i64,
    checkpoint_hash: &Digest,
    witness: Witness,
) -> Result<Checkpoint, PgError> {
    let mut tx = pool.begin().await.map_err(classify)?;
    let row = sqlx::query_as!(
        StoredCheckpoint,
        r#"SELECT tree_size, root_hash, prev_checkpoint_hash, checkpoint_hash,
                  signatures as "signatures: sqlx::types::Json<Vec<TreeHeadSignature>>",
                  witnesses as "witnesses: sqlx::types::Json<Vec<Witness>>"
           FROM morpholog.audit_checkpoints
           WHERE tree_size = $1
           FOR UPDATE"#,
        tree_size,
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(classify_checked_query)?;
    let Some(row) = row else {
        return Err(PgError::InvalidState(format!(
            "no checkpoint at tree size {tree_size} to attach a witness to"
        )));
    };
    let mut checkpoint = Checkpoint::try_from(row)?;
    if checkpoint.checkpoint_hash != *checkpoint_hash {
        return Err(PgError::InvalidState(format!(
            "the checkpoint at tree size {tree_size} is {} now, not {checkpoint_hash}: the \
             proof was obtained for a head this chain no longer holds",
            checkpoint.checkpoint_hash
        )));
    }
    if !checkpoint
        .witnesses
        .iter()
        .any(|w| w.scheme == witness.scheme && w.proof == witness.proof)
    {
        checkpoint.witnesses.push(witness);
        sqlx::query!(
            "UPDATE morpholog.audit_checkpoints
             SET witnesses = $1 WHERE tree_size = $2",
            sqlx::types::Json(&checkpoint.witnesses) as _,
            tree_size,
        )
        .execute(&mut *tx)
        .await
        .map_err(classify_checked_query)?;
    }
    tx.commit().await.map_err(classify)?;
    Ok(checkpoint)
}

/// Verify the audit tree against its checkpoints, under `SERIALIZABLE READ
/// ONLY DEFERRABLE`.
///
/// Checks that every checkpoint's root recomputes from the log, that the
/// chain is consistent (hashes and `prev` links), and, given `anchor`,
/// that the stored checkpoint at its size matches it.
pub async fn verify_audit_tree(
    pool: &PgPool,
    anchor: Option<Checkpoint>,
) -> Result<TreeVerification, PgError> {
    verify_audit_tree_under(pool, anchor, None).await
}

/// [`verify_audit_tree`], then the signature policy over the same chain in
/// the same snapshot, so the policy never judges a checkpoint the
/// intrinsic check did not.
pub async fn verify_audit_tree_under(
    pool: &PgPool,
    anchor: Option<Checkpoint>,
    policy: Option<&SignaturePolicy>,
) -> Result<TreeVerification, PgError> {
    verify_audit_tree_with_chain(pool, anchor, policy)
        .await
        .map(|(verdict, _)| verdict)
}

/// [`verify_audit_tree_under`], also returning the checkpoint chain the
/// verdict saw, so witnesses are judged on exactly those checkpoints.
pub async fn verify_audit_tree_with_chain(
    pool: &PgPool,
    anchor: Option<Checkpoint>,
    policy: Option<&SignaturePolicy>,
) -> Result<(TreeVerification, Vec<Checkpoint>), PgError> {
    let mut tx = begin_isolated_tx(pool, TxIsolation::SerializableReadOnlyDeferrable).await?;

    let checkpoints = load_checkpoint_chain(&mut tx).await?;

    let max_size = checkpoints.last().map(|c| c.tree_size).unwrap_or(0);
    let (leaves, _) = collect_leaves(&mut tx, None, Some(max_size)).await?;

    let verdict = verify_tree(&leaves, &checkpoints, anchor.as_ref());
    // An intact tree must still show each signing key, the anchor's
    // included, was authorised as of its prefix. Rows are loaded only when
    // there are signatures to judge.
    let signed = |c: &Checkpoint| !c.signatures.is_empty();
    if matches!(verdict, TreeVerification::Intact { .. })
        && (checkpoints.iter().any(signed) || anchor.as_ref().is_some_and(signed))
    {
        let rows = load_audit_rows(&mut tx, max_size).await?;
        if let Some(violation) = authority_violation(&checkpoints, anchor.as_ref(), &rows) {
            return Ok((violation, checkpoints));
        }
    }
    if let Some(policy) = policy
        && matches!(verdict, TreeVerification::Intact { .. })
        && let Some(violation) =
            policy.violation(&with_anchor_signatures(&checkpoints, anchor.as_ref()))
    {
        return Ok((violation.into(), checkpoints));
    }
    Ok((verdict, checkpoints))
}

/// Tree-head identity: the commitment, *excluding* its signatures. The
/// anchor check compares heads, so a signature difference is not a
/// mismatch.
pub(crate) fn same_tree_head(a: &Checkpoint, b: &Checkpoint) -> bool {
    a.tree_size == b.tree_size
        && a.root_hash == b.root_hash
        && a.prev_checkpoint_hash == b.prev_checkpoint_hash
        && a.checkpoint_hash == b.checkpoint_hash
}

/// The pure tamper-evidence check shared by [`verify_audit_tree`] and the
/// offline pack verifier, so the two cannot drift.
///
/// Given the leaf hashes in canonical order and the checkpoint chain,
/// confirms every checkpoint's root recomputes from the leaves, the chain
/// is consistent, and the anchor (if any) matches the stored checkpoint
/// at its size.
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
                anchor_checkpoint_hash: anchor.checkpoint_hash,
                stored_checkpoint_hash: stored_at_size.map(|c| c.checkpoint_hash),
            };
        }
        // The anchor's own signatures must verify, even if the stored copy
        // had its signatures stripped. Authority is judged later.
        if let Some(violation) = signature_crypto_violation(anchor) {
            return violation;
        }
    }

    let mut prev_hash: Option<&Digest> = None;
    for cp in checkpoints {
        let expected = checkpoint_hash(
            cp.tree_size,
            &cp.root_hash,
            cp.prev_checkpoint_hash.as_ref(),
        );
        if expected != cp.checkpoint_hash {
            return TreeVerification::ChainBroken {
                detail: format!(
                    "checkpoint at tree_size {} has hash {} but its contents hash to {expected}",
                    cp.tree_size, cp.checkpoint_hash
                ),
            };
        }
        if cp.prev_checkpoint_hash.as_ref() != prev_hash {
            return TreeVerification::ChainBroken {
                detail: format!(
                    "checkpoint at tree_size {} links to prev {:?}, expected {:?}",
                    cp.tree_size, cp.prev_checkpoint_hash, prev_hash
                ),
            };
        }
        prev_hash = Some(&cp.checkpoint_hash);

        let size = cp.tree_size as usize;
        if size > leaves.len() {
            return TreeVerification::Tampered {
                tree_size: cp.tree_size,
                recorded_root: cp.root_hash,
                recomputed_root: format!("only {} rows present", leaves.len()),
            };
        }
        let recomputed = Digest::from_bytes(merkle_root(&leaves[..size]));
        if recomputed != cp.root_hash {
            return TreeVerification::Tampered {
                tree_size: cp.tree_size,
                recorded_root: cp.root_hash,
                recomputed_root: recomputed.to_string(),
            };
        }

        if let Some(violation) = signature_crypto_violation(cp) {
            return violation;
        }
    }

    TreeVerification::Intact {
        checkpoints: checkpoints.len(),
        tree_size: checkpoints.last().map(|c| c.tree_size).unwrap_or(0),
    }
}

/// Every signature on the checkpoint must verify over its tree head, else
/// `SignatureInvalid`. Key authority is [`authority_violation`]'s job.
pub(crate) fn signature_crypto_violation(cp: &Checkpoint) -> Option<TreeVerification> {
    let head = signing::TreeHead {
        tree_size: cp.tree_size,
        root_hash: &cp.root_hash,
        prev_checkpoint_hash: cp.prev_checkpoint_hash.as_ref(),
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

/// The key authority check, run on an intact tree with genuine signatures.
///
/// For each signed checkpoint and the `anchor`, every signature's
/// `(key_id, purpose, public_key)` must match an `AuditSigningKey` claim
/// in force as of that checkpoint's prefix of `rows` (canonical order).
/// Returns the first violation.
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

/// Read the first `max` audit rows in canonical order.
async fn load_audit_rows(
    conn: &mut sqlx::PgConnection,
    max: i64,
) -> Result<Vec<AuditRow>, PgError> {
    let mut rows = Vec::new();
    let mut pages = AuditPages::new(None);
    while (rows.len() as i64) < max {
        let page = pages.next(conn).await?;
        if page.is_empty() {
            break;
        }
        for row in page {
            rows.push(row);
            if rows.len() as i64 >= max {
                break;
            }
        }
    }
    Ok(rows)
}
