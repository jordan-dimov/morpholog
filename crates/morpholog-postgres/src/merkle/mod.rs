//! RFC 6962 (Certificate Transparency) Merkle history tree over the
//! audit log.
//!
//! The audit log is an append-only ordered sequence of transitions; each
//! row is a leaf. The Merkle Tree Hash (MTH) of the first `n` leaves is a
//! single root commitment to that prefix - a later edit to any covered
//! row changes its leaf and therefore the root, which a stored checkpoint
//! catches (see `checkpoints`). We use the RFC 6962 construction (not a
//! naive hash chain) so the same leaves later yield logarithmic inclusion
//! and consistency proofs without recomputation.
//!
//! Proof *generation* follows RFC 6962 (sections 2.1.1 and 2.1.2); proof
//! *verification* follows the explicit step-by-step algorithms in RFC 9162
//! (Certificate Transparency 2.0, sections 2.1.3.2 and 2.1.4.2) - its
//! successor over the same tree, because RFC 6962 specifies the construction
//! but not a verifier procedure. The two are interoperable; the per-function
//! doc comments cite whichever the code there implements.
//!
//! This module is pure and synchronous: leaf encoding + tree hashing,
//! no I/O.

use chrono::{DateTime, Utc};
use sha2::{Digest as _, Sha256};

use crate::audit::AuditRow;
use morpholog_core::EvalValue;

/// Domain-separation prefixes from RFC 6962 section 2.1: a leaf hash is
/// `SHA-256(0x00 || data)`, an interior node is
/// `SHA-256(0x01 || left || right)`. The distinct prefixes stop a leaf
/// from being reinterpreted as an interior node (second-preimage
/// defence).
const LEAF_PREFIX: u8 = 0x00;
const NODE_PREFIX: u8 = 0x01;

/// Version bytes for the canonical leaf encoding. A codec change
/// becomes a new leaf version rather than a silent change to historical
/// roots. The version a row hashes under is derived from the row's own
/// content - nothing selects the original encoding, an attestation the
/// attested one, an attestation with parameter names the
/// self-describing one - so a verifier needs no side channel, and
/// moving a field across a boundary in either direction changes the
/// leaf and breaks the root.
const LEAF_FORMAT_V1: u8 = 1;
const LEAF_FORMAT_V2: u8 = 2;
const LEAF_FORMAT_V3: u8 = 3;

/// A 32-byte SHA-256 digest.
pub(crate) type Hash = [u8; 32];

/// `SHA-256(0x00 || data)` - the RFC 6962 leaf hash.
fn leaf_hash(data: &[u8]) -> Hash {
    let mut h = Sha256::new();
    h.update([LEAF_PREFIX]);
    h.update(data);
    h.finalize().into()
}

/// `SHA-256(0x01 || left || right)` - the RFC 6962 interior-node hash.
fn node_hash(left: &Hash, right: &Hash) -> Hash {
    let mut h = Sha256::new();
    h.update([NODE_PREFIX]);
    h.update(left);
    h.update(right);
    h.finalize().into()
}

/// The largest power of two strictly less than `n` (the RFC 6962 split
/// point `k` for `n > 1`).
fn split_point(n: usize) -> usize {
    debug_assert!(n > 1);
    let mut k = 1;
    while k << 1 < n {
        k <<= 1;
    }
    k
}

/// The RFC 6962 Merkle Tree Hash over an ordered sequence of already
/// computed leaf hashes. Empty -> `SHA-256("")`; one leaf -> that leaf;
/// otherwise split at the largest power of two below the length and hash
/// the two subtrees. Left-full, so appending leaves only ever rebuilds
/// the right spine - the property the proofs later rely on.
pub(crate) fn merkle_root(leaves: &[Hash]) -> Hash {
    match leaves.len() {
        0 => Sha256::new().finalize().into(),
        1 => leaves[0],
        n => {
            let k = split_point(n);
            node_hash(&merkle_root(&leaves[..k]), &merkle_root(&leaves[k..]))
        }
    }
}

/// A SHA-256 digest as the record carries it: `sha256:<64 hex>`, the
/// project's self-describing hash convention, so the algorithm is
/// legible if it ever has to change. Parsed once, at every boundary a
/// hash crosses - a checkpoint read from the database, a pack read
/// from a file, an anchor handed to a verifier - so a value of this
/// type is well-formed by construction and no verifier re-parses a
/// hash at the point of use. A malformed hash is refused where it
/// arrives, as the malformed input it is. The hex is lowercase only,
/// so parsing a rendering and rendering a parse are both the identity
/// and one digest has one spelling everywhere it is compared.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Digest(Hash);

impl Digest {
    pub(crate) fn from_bytes(hash: Hash) -> Self {
        Self(hash)
    }

    pub(crate) fn bytes(&self) -> &Hash {
        &self.0
    }
}

impl std::fmt::Display for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "sha256:{}", crate::hex::encode(&self.0))
    }
}

impl std::fmt::Debug for Digest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self}")
    }
}

/// Why a string is not a digest.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("not a sha256:<hex> digest: {0}")]
pub struct DigestError(String);

impl std::str::FromStr for Digest {
    type Err = DigestError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let malformed = || DigestError(s.to_string());
        let hex = s.strip_prefix("sha256:").ok_or_else(malformed)?;
        if hex.len() != 64 || hex.bytes().any(|b| b.is_ascii_uppercase()) {
            return Err(malformed());
        }
        let mut out = [0u8; 32];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2).ok_or_else(malformed)?, 16)
                .map_err(|_| malformed())?;
        }
        Ok(Self(out))
    }
}

impl serde::Serialize for Digest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Digest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = <String as serde::Deserialize>::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// Append `bytes` length-prefixed (u32 little-endian length, then the
/// bytes) so the field concatenation is injective: no two distinct field
/// sequences can produce the same buffer.
fn push_field(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    buf.extend_from_slice(bytes);
}

/// The canonical, version-tagged byte encoding of one audit row - the
/// leaf payload. Every column that carries legitimacy is covered, in a
/// fixed order, each field length-prefixed. `committed_at` is encoded as
/// integer microseconds (PostgreSQL `timestamptz` precision), so an
/// order-preserving timestamp shift still changes the leaf. The JSONB
/// columns reuse the existing deterministic tagged codec (the same
/// determinism `compute_idempotency_key` relies on).
fn canonical_leaf_bytes(row: &AuditRow) -> Result<Vec<u8>, serde_json::Error> {
    // A row no writer could have produced gets no encoding at all,
    // rather than the nearest one: hostile input must not hash.
    row.validate_shape().map_err(serde::ser::Error::custom)?;
    let mut buf = Vec::new();
    match (&row.attestation, &row.parameters) {
        // The original encoding, frozen forever: rows written before
        // attestation existed hash exactly as they always did, so every
        // historical root still verifies. Its one quirk stays with it -
        // the actor serialises as the transparent bare string here,
        // although the column and the envelope carry the tagged form.
        (None, _) => {
            buf.push(LEAF_FORMAT_V1);
            push_transition_fields(&mut buf, row, &serde_json::to_vec(&row.actor)?)?;
        }
        // The attested encoding: same field order, the actor in the
        // same tagged form the column and the envelope use, and the
        // attestation object covered whole - its internal shape can
        // grow (new modes, new fields) without another leaf version,
        // because the leaf commits to its exact bytes either way.
        (Some(attestation), None) => {
            buf.push(LEAF_FORMAT_V2);
            let actor = EvalValue::Subject(row.actor.clone());
            push_transition_fields(&mut buf, row, &serde_json::to_vec(&actor)?)?;
            push_field(&mut buf, &serde_json::to_vec(attestation)?);
        }
        // The self-describing encoding: the attested fields, then the
        // parameter names as one JSON array - the signature a reader's
        // absence claims rest on, covered by the same leaf.
        (Some(attestation), Some(parameters)) => {
            buf.push(LEAF_FORMAT_V3);
            let actor = EvalValue::Subject(row.actor.clone());
            push_transition_fields(&mut buf, row, &serde_json::to_vec(&actor)?)?;
            push_field(&mut buf, &serde_json::to_vec(attestation)?);
            push_field(&mut buf, &serde_json::to_vec(parameters)?);
        }
    }
    Ok(buf)
}

/// The fields both leaf versions share, in their fixed order; the actor
/// bytes are supplied by the caller because the two versions encode the
/// actor differently.
fn push_transition_fields(
    buf: &mut Vec<u8>,
    row: &AuditRow,
    actor_bytes: &[u8],
) -> Result<(), serde_json::Error> {
    push_field(buf, row.transition_id.as_bytes());
    push_field(buf, row.transformation_name.as_str().as_bytes());
    push_field(buf, &committed_at_micros(row.committed_at).to_le_bytes());
    push_field(buf, actor_bytes);
    push_field(buf, &row.invariant_epoch.to_le_bytes());
    push_field(buf, &serde_json::to_vec(&row.invariants_checked)?);
    push_field(buf, &serde_json::to_vec(&row.arguments)?);
    push_field(buf, &serde_json::to_vec(&row.asserted_claims)?);
    push_field(buf, &serde_json::to_vec(&row.retracted_claims)?);
    push_field(buf, &serde_json::to_vec(&row.emitted_intents)?);
    Ok(())
}

fn committed_at_micros(ts: DateTime<Utc>) -> i64 {
    ts.timestamp_micros()
}

/// The leaf hash of one audit row.
pub(crate) fn audit_leaf_hash(row: &AuditRow) -> Result<Hash, serde_json::Error> {
    Ok(leaf_hash(&canonical_leaf_bytes(row)?))
}

/// Why a Merkle proof failed to verify. One enum serves both proof kinds:
/// the failure modes are identical, and two near-identical types would be
/// surface for no distinction (the pack layer maps `RootMismatch` to the
/// kind-specific verdict from its own context).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProofError {
    /// The size/index arguments are out of range (e.g. `first > second`,
    /// or a leaf index past the tree size).
    BadParameters,
    /// The proof has the wrong length for the given tree sizes - too few
    /// or too many hashes. A structurally malformed proof, not a data
    /// disagreement.
    Malformed,
    /// The proof is well-formed but did not reconstruct the expected root.
    /// For inclusion: the leaf is not at that position. For consistency:
    /// the later tree is not an append-only extension of the earlier one.
    RootMismatch,
}

/// RFC 6962 section 2.1.1 Merkle audit path for the `index`-th leaf of the
/// tree over `leaves` (`PATH(index, D[leaves.len()])`): the sibling hashes,
/// leaf-to-root, that recompute the root. Recurses on the left-full split,
/// appending the other subtree's root at each level.
pub(crate) fn inclusion_proof(leaves: &[Hash], index: usize) -> Vec<Hash> {
    debug_assert!(index < leaves.len(), "leaf index past tree size");
    let n = leaves.len();
    if n == 1 {
        return Vec::new();
    }
    let k = split_point(n);
    if index < k {
        let mut path = inclusion_proof(&leaves[..k], index);
        path.push(merkle_root(&leaves[k..]));
        path
    } else {
        let mut path = inclusion_proof(&leaves[k..], index - k);
        path.push(merkle_root(&leaves[..k]));
        path
    }
}

/// Verify an inclusion proof by the RFC 9162 section 2.1.3.2 algorithm:
/// reconstruct the root from the leaf hash walking up the audit path, and
/// compare to `root`. `Malformed` if the path is the wrong length for the
/// tree, `RootMismatch` if the reconstructed root disagrees.
pub(crate) fn verify_inclusion_proof(
    index: usize,
    tree_size: usize,
    leaf: &Hash,
    root: &Hash,
    proof: &[Hash],
) -> Result<(), ProofError> {
    if index >= tree_size {
        return Err(ProofError::BadParameters);
    }
    let mut fnode = index;
    let mut snode = tree_size - 1;
    let mut r = *leaf;
    for p in proof {
        if snode == 0 {
            return Err(ProofError::Malformed); // proof too long
        }
        if fnode & 1 == 1 || fnode == snode {
            r = node_hash(p, &r);
            if fnode & 1 == 0 {
                while fnode & 1 == 0 && fnode != 0 {
                    fnode >>= 1;
                    snode >>= 1;
                }
            }
        } else {
            r = node_hash(&r, p);
        }
        fnode >>= 1;
        snode >>= 1;
    }
    if snode != 0 {
        return Err(ProofError::Malformed); // proof too short
    }
    if r == *root {
        Ok(())
    } else {
        Err(ProofError::RootMismatch)
    }
}

/// RFC 6962 section 2.1.2 Merkle consistency proof (`PROOF(first_size,
/// D[leaves.len()])`): the node hashes that let a verifier holding the two
/// roots confirm the second tree is an append-only extension of the first.
/// `first_size == leaves.len()` yields the empty proof.
pub(crate) fn consistency_proof(leaves: &[Hash], first_size: usize) -> Vec<Hash> {
    let n = leaves.len();
    debug_assert!(first_size <= n, "first_size past tree size");
    // The empty tree is consistent with anything and the whole tree with
    // itself; both are the empty proof, matching the verifier's handling of
    // `first_size == 0` and `first_size == second_size`.
    if first_size == 0 || first_size >= n {
        return Vec::new();
    }
    subproof(first_size, leaves, true)
}

/// The RFC 6962 `SUBPROOF(m, D[n], b)` recursion. `b` tracks whether the
/// node covering the first `m` leaves is on the verifier's own path (true:
/// it can recompute it, so it is omitted) or must be supplied (false: the
/// root of the fully covered subtree is emitted).
fn subproof(m: usize, leaves: &[Hash], b: bool) -> Vec<Hash> {
    let n = leaves.len();
    if m == n {
        return if b {
            Vec::new()
        } else {
            vec![merkle_root(leaves)]
        };
    }
    let k = split_point(n);
    if m <= k {
        let mut proof = subproof(m, &leaves[..k], b);
        proof.push(merkle_root(&leaves[k..]));
        proof
    } else {
        let mut proof = subproof(m - k, &leaves[k..], false);
        proof.push(merkle_root(&leaves[..k]));
        proof
    }
}

/// Verify a consistency proof by the RFC 9162 section 2.1.4.2 algorithm:
/// reconstruct both the earlier root (`first_root`) and the later root
/// (`second_root`) from the proof and check both. `RootMismatch` means the
/// later tree is not an append-only extension of the earlier one (the
/// inconsistent-extension case the window verdict names).
pub(crate) fn verify_consistency_proof(
    first_size: usize,
    first_root: &Hash,
    second_size: usize,
    second_root: &Hash,
    proof: &[Hash],
) -> Result<(), ProofError> {
    if first_size > second_size {
        return Err(ProofError::BadParameters);
    }
    if first_size == second_size {
        if !proof.is_empty() {
            return Err(ProofError::Malformed);
        }
        return if first_root == second_root {
            Ok(())
        } else {
            Err(ProofError::RootMismatch)
        };
    }
    if first_size == 0 {
        // The empty tree is consistent with any later tree; the proof
        // carries nothing.
        return if proof.is_empty() {
            Ok(())
        } else {
            Err(ProofError::Malformed)
        };
    }

    let mut node = first_size - 1;
    let mut last = second_size - 1;
    while node & 1 == 1 {
        node >>= 1;
        last >>= 1;
    }

    let mut idx = 0;
    // When `first_size` is an exact power of two the earlier root is its
    // own seed; otherwise the first proof hash seeds both reconstructions.
    let (mut h1, mut h2) = if node != 0 {
        let seed = proof.get(idx).ok_or(ProofError::Malformed)?;
        idx += 1;
        (*seed, *seed)
    } else {
        (*first_root, *first_root)
    };

    while node != 0 {
        if node & 1 == 1 {
            let p = proof.get(idx).ok_or(ProofError::Malformed)?;
            idx += 1;
            h1 = node_hash(p, &h1);
            h2 = node_hash(p, &h2);
        } else if node < last {
            let p = proof.get(idx).ok_or(ProofError::Malformed)?;
            idx += 1;
            h2 = node_hash(&h2, p);
        }
        node >>= 1;
        last >>= 1;
    }

    // The remaining hashes extend the later tree's right edge.
    while last != 0 {
        let p = proof.get(idx).ok_or(ProofError::Malformed)?;
        idx += 1;
        h2 = node_hash(&h2, p);
        last >>= 1;
    }

    if idx != proof.len() {
        return Err(ProofError::Malformed); // proof too long
    }
    if h1 != *first_root || h2 != *second_root {
        return Err(ProofError::RootMismatch);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
