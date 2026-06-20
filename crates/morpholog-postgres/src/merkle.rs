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
//! This module is pure and synchronous: leaf encoding + tree hashing,
//! no I/O.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::audit::AuditRow;

/// Domain-separation prefixes from RFC 6962 section 2.1: a leaf hash is
/// `SHA-256(0x00 || data)`, an interior node is
/// `SHA-256(0x01 || left || right)`. The distinct prefixes stop a leaf
/// from being reinterpreted as an interior node (second-preimage
/// defence).
const LEAF_PREFIX: u8 = 0x00;
const NODE_PREFIX: u8 = 0x01;

/// Version byte for the canonical leaf encoding. A future codec change
/// becomes a new leaf version rather than a silent change to historical
/// roots.
const LEAF_FORMAT_V1: u8 = 1;

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

/// Render a digest as `sha256:<hex>` - the project's self-describing
/// hash convention (matches the CLI model hash), so the algorithm is
/// legible if it ever has to change.
pub(crate) fn render_hash(hash: &Hash) -> String {
    let mut s = String::from("sha256:");
    for b in hash {
        s.push_str(&format!("{b:02x}"));
    }
    s
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
    let mut buf = Vec::new();
    buf.push(LEAF_FORMAT_V1);
    push_field(&mut buf, row.transition_id.as_bytes());
    push_field(&mut buf, row.transformation_name.as_str().as_bytes());
    push_field(
        &mut buf,
        &committed_at_micros(row.committed_at).to_le_bytes(),
    );
    push_field(&mut buf, &serde_json::to_vec(&row.actor)?);
    push_field(&mut buf, &row.invariant_epoch.to_le_bytes());
    push_field(&mut buf, &serde_json::to_vec(&row.invariants_checked)?);
    push_field(&mut buf, &serde_json::to_vec(&row.arguments)?);
    push_field(&mut buf, &serde_json::to_vec(&row.asserted_claims)?);
    push_field(&mut buf, &serde_json::to_vec(&row.retracted_claims)?);
    push_field(&mut buf, &serde_json::to_vec(&row.emitted_intents)?);
    Ok(buf)
}

fn committed_at_micros(ts: DateTime<Utc>) -> i64 {
    ts.timestamp_micros()
}

/// The leaf hash of one audit row.
pub(crate) fn audit_leaf_hash(row: &AuditRow) -> Result<Hash, serde_json::Error> {
    Ok(leaf_hash(&canonical_leaf_bytes(row)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha256(parts: &[&[u8]]) -> Hash {
        let mut h = Sha256::new();
        for p in parts {
            h.update(p);
        }
        h.finalize().into()
    }

    /// The empty tree is `SHA-256("")` - the fixed RFC 6962 constant.
    #[test]
    fn empty_tree_is_sha256_of_empty() {
        assert_eq!(
            render_hash(&merkle_root(&[])),
            "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// A single-leaf tree's root IS the leaf hash.
    #[test]
    fn single_leaf_root_is_the_leaf() {
        let l = leaf_hash(b"d0");
        assert_eq!(merkle_root(&[l]), l);
    }

    /// `merkle_root` matches the RFC 6962 recurrence for n = 1..=8: split
    /// at the largest power of two below n, hash the two subtrees. This
    /// independent reference pins that we speak the standard, not a
    /// dialect.
    #[test]
    fn matches_rfc6962_recurrence() {
        fn reference(leaves: &[Hash]) -> Hash {
            match leaves.len() {
                0 => sha256(&[]),
                1 => leaves[0],
                n => {
                    let mut k = 1usize;
                    while k * 2 < n {
                        k *= 2;
                    }
                    node_hash(&reference(&leaves[..k]), &reference(&leaves[k..]))
                }
            }
        }
        let leaves: Vec<Hash> = (0u8..8).map(|i| leaf_hash(&[i])).collect();
        for n in 0..=8 {
            assert_eq!(
                merkle_root(&leaves[..n]),
                reference(&leaves[..n]),
                "root disagrees with the RFC 6962 recurrence at n={n}"
            );
        }
    }

    /// The two-leaf root is exactly `node(leaf(a), leaf(b))`.
    #[test]
    fn two_leaf_root_is_one_node() {
        let a = leaf_hash(b"a");
        let b = leaf_hash(b"b");
        assert_eq!(merkle_root(&[a, b]), node_hash(&a, &b));
    }

    /// A frozen root over leaves `"a"` and `"b"`, computed independently
    /// with `sha256sum` (not these functions): a regression in the leaf
    /// or node hashing changes it. Pins byte-compatibility with the RFC
    /// 6962 construction, not just internal self-consistency.
    #[test]
    fn frozen_two_leaf_root_matches_an_independent_sha256() {
        let root = merkle_root(&[leaf_hash(b"a"), leaf_hash(b"b")]);
        assert_eq!(
            render_hash(&root),
            "sha256:b137985ff484fb600db93107c77b0365c80d78f5b429ded0fd97361d077999eb"
        );
    }

    /// Leaf and node prefixes differ, so a leaf can never collide with an
    /// interior node over the same bytes.
    #[test]
    fn leaf_and_node_domains_are_separated() {
        let a = leaf_hash(b"x");
        let b = leaf_hash(b"y");
        // node_hash(0x01 || a || b) must not equal leaf_hash(0x00 || a||b).
        let mut concat = a.to_vec();
        concat.extend_from_slice(&b);
        assert_ne!(node_hash(&a, &b), leaf_hash(&concat));
    }

    #[test]
    fn split_point_is_largest_power_of_two_below_n() {
        assert_eq!(split_point(2), 1);
        assert_eq!(split_point(3), 2);
        assert_eq!(split_point(4), 2);
        assert_eq!(split_point(5), 4);
        assert_eq!(split_point(7), 4);
        assert_eq!(split_point(8), 4);
        assert_eq!(split_point(9), 8);
    }
}
