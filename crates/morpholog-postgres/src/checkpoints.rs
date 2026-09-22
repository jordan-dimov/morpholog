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

use ed25519_dalek::SigningKey;
use jiff::Timestamp;
use jiff_sqlx::ToSqlx;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use sqlx::PgPool;
use uuid::Uuid;

use crate::audit::{AuditRow, REPLAY_CHUNK, list_audit_rows_page};
use crate::error::{PgError, classify, classify_checked_query};
use crate::merkle::{Digest, Hash, audit_leaf_hash, merkle_root};
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

/// Which external scheme a witness proof comes from. Rung one: RFC 3161
/// timestamp tokens. OpenTimestamps joins as a second variant when its
/// pending-then-upgraded lifecycle is built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WitnessScheme {
    Rfc3161,
}

/// One external witness to a tree head: the exact bytes a timestamp
/// authority returned, stored opaque and never re-encoded, and where it
/// was obtained. Nothing derived is stored - not the attested time, not
/// whether it verifies - because a witness sits outside `checkpoint_hash`
/// and a stored derivation would be a mutable duplicate of what the
/// proof already proves; the verifier reads both from the proof. Two
/// byte-identical proofs are one witness whatever URL served them.
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
    /// none was obtained, which stays valid - a witness adds "existed no
    /// later than T", it never subtracts from the tree's own verdict.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub witnesses: Vec<Witness>,
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
        recorded_root: Digest,
        /// Usually the recomputed digest; when the log is shorter than
        /// the checkpoint claims, a sentence saying so instead, so the
        /// field stays prose-capable on the wire.
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
        anchor_checkpoint_hash: Digest,
        stored_checkpoint_hash: Option<Digest>,
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
    /// The verifier pinned a signing key, the tree is otherwise intact -
    /// every signature it carries genuine and authorised - but this
    /// checkpoint carries no signature by that key. Policy, layered over
    /// the intrinsic verdict: the pin narrows which authorised signer the
    /// verifier accepts, and never makes an unauthorised one acceptable.
    SigningKeyRequired { tree_size: i64, public_key: String },
}

/// What a verifier requires of checkpoint signatures, over and above
/// the intrinsic verdict: from which tree size on a checkpoint must be
/// signed, and by which key if one is pinned. Applied only to an intact
/// tree, so every signature it inspects is already proven genuine and
/// authorised as of its prefix - a pin is an intersection with that
/// fold, never a substitute for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignaturePolicy {
    /// Checkpoints at or after this tree size must satisfy the policy;
    /// earlier ones - honest history from before signing began - are
    /// not asked. Zero asks of every checkpoint.
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
    /// The first checkpoint at or after the threshold with no signature
    /// at all. The one check a verifier that cannot judge key authority
    /// (a window or selective pack) may still apply.
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

/// The checkpoints as the verifier effectively holds them: where an
/// externally held anchor matches one by tree size - which an intact
/// verdict has already proven - its signatures count for that
/// checkpoint too, exactly as the intrinsic authority check judges the
/// anchor's own signatures. A stripped database copy beside a signed
/// anchor is attributable, and the policy sees it that way.
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

/// A hash column read back: the runtime wrote it, so a malformed one is
/// the database holding something impossible, refused at the boundary.
fn stored_digest(text: &str) -> Result<Digest, PgError> {
    text.parse()
        .map_err(|e| PgError::InvalidState(format!("a stored checkpoint hash is malformed: {e}")))
}

/// Page the audit log in canonical order, hashing each row to its leaf.
/// `horizon` bounds by `committed_at` (the resume watermark, for
/// checkpoint creation); `max` stops after that many rows (verification,
/// which only needs the checkpointed prefix). Returns the leaf hashes and
/// the last row's coordinates.
async fn collect_leaves(
    conn: &mut sqlx::PgConnection,
    horizon: Option<Timestamp>,
    max: Option<i64>,
) -> Result<(Vec<[u8; 32]>, Option<(Uuid, Timestamp)>), PgError> {
    let mut leaves = Vec::new();
    let mut cursor: Option<(Timestamp, Uuid)> = None;
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

/// A checkpoint row as the table holds it; every digest is text there
/// and is parsed once, here, on the way out.
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

/// Record a checkpoint over the current watermark-stable prefix of the
/// audit log. The heavy root computation runs under `SERIALIZABLE READ
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
    // one deferrable snapshot, so the check and the leaves agree. On
    // failure the withheld-row count is taken in the same snapshot too
    // (so it cannot contradict tree_size); a signer already authorised in
    // the candidate prefix pays no diagnostic query. The verdict is held,
    // not returned: it judges this snapshot's prefix, and whether that is
    // the head being signed is only known once the chain head is read
    // under the lock below.
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
        // No new rows. With a signer, attach its attestation to the
        // existing head instead of returning it unsigned - signing an
        // already-recorded checkpoint does not fork the tree (the
        // signature is not part of `checkpoint_hash`). An exact re-sign
        // is de-duplicated.
        let outcome = match signer {
            Some(s) => {
                // Authority is resolved against the head that actually
                // receives the signature, never the snapshot's own
                // prefix: when a concurrent run has advanced the head
                // (or this run's horizon was dragged back), the two
                // differ, and a key revoked in between must not sign
                // the newer head. The head's rows are stable - its own
                // checkpoint already covered them - so the re-read
                // under the lock cannot shift.
                if p.tree_size == tree_size {
                    if let Some(refusal) = candidate_refusal {
                        return Err(refusal);
                    }
                } else {
                    let rows = load_audit_rows(&mut tx, p.tree_size).await?;
                    if let Some(refusal) = signer_authority_violation(s, &rows, p.tree_size) {
                        // Two separate questions: the key is not
                        // authorised at the head this run can sign - and
                        // is there a committed suffix beyond that head
                        // the horizon keeps uncheckpointable? Rows
                        // between this snapshot's prefix and the head
                        // cannot help (the head already incorporates
                        // them); rows beyond the head could carry an
                        // authorisation a future checkpoint would
                        // honour, so the refusal must name the
                        // withholding, not impugn the key.
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

    // Creating a new checkpoint at this snapshot's prefix, so the
    // candidate verdict IS the head's verdict.
    if let Some(refusal) = candidate_refusal {
        return Err(refusal);
    }

    let prev_hash = prev.as_ref().map(|p| p.checkpoint_hash);
    let cp_hash = checkpoint_hash(tree_size, &root_hash, prev_hash.as_ref());
    let (last_tid, last_at) = match last {
        Some((tid, at)) => (Some(tid), Some(at)),
        None => (None, None),
    };

    // Sign the new tree head if a key was supplied - the refusal above
    // already judged this exact prefix, so an unauthorised key never
    // reaches this attestation.
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

/// Upgrade a plain authority refusal to the truncated-prefix diagnosis:
/// the judged prefix is not the whole committed log, so a later
/// authorisation may sit in the withheld suffix and the operator should
/// suspect the workload before the key. Leaves any other error alone.
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

/// The candidate-prefix arm of the diagnosis: count the committed rows
/// the horizon withheld from this snapshot, and upgrade the refusal
/// when there are any.
async fn with_horizon_diagnosis(
    refusal: PgError,
    conn: &mut sqlx::PgConnection,
    horizon: Timestamp,
) -> Result<PgError, PgError> {
    // `>=` mirrors the pager's strict `<` clamp: a row at the horizon is
    // withheld. count(*) is never NULL, so the override is sound.
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

/// Load the whole checkpoint chain, ascending by size - the read shared by
/// the live verifier and pack export.
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

/// Attach an external witness to the checkpoint at `tree_size`, which
/// must still be the head the proof was obtained for: the caller went to
/// the authority outside any transaction, and a chain rebuilt in between
/// would make the proof about a head that no longer exists here.
/// Monotonic and exact-deduplicated - a proof already present is not
/// stored twice, whatever endpoint served it - and never a downgrade:
/// nothing existing is removed, which the row lock guarantees when two
/// attachments arrive together. Returns the checkpoint as now stored.
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

/// Verify the audit tree against its checkpoints. Reads under
/// `SERIALIZABLE READ ONLY DEFERRABLE`. Checks, strongest last: every
/// checkpoint's root recomputes from the current log; the checkpoint
/// chain is internally consistent (hash + `prev` links); and, if `anchor`
/// is supplied, the stored checkpoint at the anchor's size matches the
/// externally held copy.
pub async fn verify_audit_tree(
    pool: &PgPool,
    anchor: Option<Checkpoint>,
) -> Result<TreeVerification, PgError> {
    verify_audit_tree_under(pool, anchor, None).await
}

/// [`verify_audit_tree`], then the verifier's signature policy over the
/// same checkpoint chain the intrinsic verdict was computed from - one
/// snapshot, so a checkpoint appended between the two could not be
/// judged by the policy without having been judged intrinsically.
pub async fn verify_audit_tree_under(
    pool: &PgPool,
    anchor: Option<Checkpoint>,
    policy: Option<&SignaturePolicy>,
) -> Result<TreeVerification, PgError> {
    verify_audit_tree_with_chain(pool, anchor, policy)
        .await
        .map(|(verdict, _)| verdict)
}

/// [`verify_audit_tree_under`], also handing back the checkpoint chain the
/// verdict was computed over - the same snapshot - so the witness axis is
/// judged on exactly the checkpoints the tree verdict saw.
pub async fn verify_audit_tree_with_chain(
    pool: &PgPool,
    anchor: Option<Checkpoint>,
    policy: Option<&SignaturePolicy>,
) -> Result<(TreeVerification, Vec<Checkpoint>), PgError> {
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
                anchor_checkpoint_hash: anchor.checkpoint_hash,
                stored_checkpoint_hash: stored_at_size.map(|c| c.checkpoint_hash),
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

    let mut prev_hash: Option<&Digest> = None;
    for cp in checkpoints {
        // Chain integrity: the recorded checkpoint_hash must match its
        // contents, and prev must link to the previous checkpoint.
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

        // Tamper check: recompute the root over the log prefix.
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
    let mut cursor: Option<(Timestamp, Uuid)> = None;
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
