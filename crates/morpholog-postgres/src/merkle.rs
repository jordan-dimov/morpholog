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

/// Parse a `sha256:<hex>` string back into a digest - the inverse of
/// [`render_hash`]. `None` if the prefix is wrong or the hex is not exactly
/// 32 bytes; a window verifier parses proof hashes out of a pack, which is
/// hostile input.
pub(crate) fn parse_hash(s: &str) -> Option<Hash> {
    let hex = s.strip_prefix("sha256:")?;
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
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
mod tests {
    use super::*;

    fn sha256(parts: &[&[u8]]) -> Hash {
        let mut h = Sha256::new();
        for p in parts {
            h.update(p);
        }
        h.finalize().into()
    }

    /// A fully-populated audit row with every field fixed, so the leaf
    /// bytes are deterministic. Used to pin the canonical encoding.
    fn fixed_row() -> AuditRow {
        use morpholog_core::{EvalValue, Subject};
        AuditRow {
            transition_id: uuid::Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0001),
            transformation_name: "post_entry".into(),
            arguments: vec![
                EvalValue::Subject(Subject::from("e1")),
                EvalValue::Decimal("125.50".parse().unwrap()),
            ],
            actor: Subject::from("alex"),
            invariant_epoch: 1,
            invariants_checked: vec![crate::AuditedInvariantCheck {
                name: "books_balance".into(),
                version: 1,
            }],
            asserted_claims: vec![morpholog_core::ClaimInstance {
                predicate: "Entry".into(),
                args: vec![
                    EvalValue::Subject(Subject::from("e1")),
                    EvalValue::Decimal("125.50".parse().unwrap()),
                ],
            }],
            retracted_claims: vec![],
            emitted_intents: vec![morpholog_core::IntentInstance {
                name: "EntryPosted".into(),
                args: vec![EvalValue::Subject(Subject::from("e1"))],
            }],
            committed_at: "2026-01-02T03:04:05.123456Z".parse().unwrap(),
        }
    }

    /// The frozen leaf hash of [`fixed_row`] under the original leaf
    /// encoding. Computed once and pinned: any change to the canonical
    /// bytes - field order, length prefixes, codec, version byte -
    /// changes this hash, and such a change must arrive as a NEW leaf
    /// version, never as an edit to the encoding historical roots were
    /// computed under.
    #[test]
    fn frozen_v1_leaf_hash_pins_the_canonical_encoding() {
        let hash = audit_leaf_hash(&fixed_row()).unwrap();
        assert_eq!(
            render_hash(&hash),
            "sha256:d9b263c7ced1cdbebae9371350204a30da05879720cf414e1cae0bf23c174be9"
        );
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

    /// `parse_hash` inverts `render_hash` and rejects malformed input.
    #[test]
    fn parse_hash_inverts_render_hash() {
        let h = leaf_hash(b"roundtrip");
        assert_eq!(parse_hash(&render_hash(&h)), Some(h));
        assert_eq!(parse_hash("no-prefix"), None);
        assert_eq!(parse_hash("sha256:abcd"), None); // too short
        assert_eq!(parse_hash(&format!("sha256:{}", "zz".repeat(32))), None); // non-hex
    }

    /// The RFC 6962 section 2.1.3/2.1.4 worked example tree over seven
    /// inputs d0..d6, with the figure's node names. Returns the leaves and
    /// the named interior nodes the published proof vectors are spelled in.
    struct Rfc7 {
        d: Vec<Hash>,
        g: Hash,
        h: Hash,
        i: Hash,
        k: Hash,
        l: Hash,
        root: Hash,
    }

    fn rfc7() -> Rfc7 {
        let d: Vec<Hash> = (0..7)
            .map(|n| leaf_hash(format!("d{n}").as_bytes()))
            .collect();
        let g = node_hash(&d[0], &d[1]);
        let h = node_hash(&d[2], &d[3]);
        let i = node_hash(&d[4], &d[5]);
        let k = node_hash(&g, &h);
        let l = node_hash(&i, &d[6]); // j = leaf d6
        let root = node_hash(&k, &l);
        assert_eq!(
            merkle_root(&d),
            root,
            "test tree disagrees with merkle_root"
        );
        Rfc7 {
            d,
            g,
            h,
            i,
            k,
            l,
            root,
        }
    }

    /// Consistency proofs match the three RFC 6962 section 2.1.4 vectors:
    /// PROOF(3,7)=[c,d,g,l], PROOF(4,7)=[l], PROOF(6,7)=[i,j,k].
    #[test]
    fn consistency_proofs_match_rfc6962_vectors() {
        let t = rfc7();
        let (c, d, j) = (t.d[2], t.d[3], t.d[6]);
        assert_eq!(consistency_proof(&t.d, 3), vec![c, d, t.g, t.l]);
        assert_eq!(consistency_proof(&t.d, 4), vec![t.l]);
        assert_eq!(consistency_proof(&t.d, 6), vec![t.i, j, t.k]);
    }

    /// Inclusion (audit) paths match the four RFC 6962 section 2.1.3
    /// vectors: PATH(0,7)=[b,h,l], PATH(3,7)=[c,g,l], PATH(4,7)=[f,j,k],
    /// PATH(6,7)=[i,k].
    #[test]
    fn inclusion_proofs_match_rfc6962_vectors() {
        let t = rfc7();
        let (b, c, f, j) = (t.d[1], t.d[2], t.d[5], t.d[6]);
        assert_eq!(inclusion_proof(&t.d, 0), vec![b, t.h, t.l]);
        assert_eq!(inclusion_proof(&t.d, 3), vec![c, t.g, t.l]);
        assert_eq!(inclusion_proof(&t.d, 4), vec![f, j, t.k]);
        assert_eq!(inclusion_proof(&t.d, 6), vec![t.i, t.k]);
    }

    /// Every leaf of the RFC tree verifies against the real root, and a
    /// leaf at the wrong index (or a tampered leaf) is `RootMismatch`.
    #[test]
    fn inclusion_round_trips_and_rejects_tampering() {
        let t = rfc7();
        for index in 0..t.d.len() {
            let proof = inclusion_proof(&t.d, index);
            assert_eq!(
                verify_inclusion_proof(index, t.d.len(), &t.d[index], &t.root, &proof),
                Ok(())
            );
            // A different leaf at this position must not verify - the
            // guard against shipping fake rows under a genuine tree.
            let wrong = leaf_hash(b"forged");
            assert_eq!(
                verify_inclusion_proof(index, t.d.len(), &wrong, &t.root, &proof),
                Err(ProofError::RootMismatch)
            );
        }
    }

    /// Inclusion proofs round-trip for every (size, index) up to a small
    /// bound - the structural pin beyond the single RFC tree.
    #[test]
    fn inclusion_round_trips_for_all_small_trees() {
        let leaves: Vec<Hash> = (0u16..20).map(|n| leaf_hash(&n.to_le_bytes())).collect();
        for size in 1..=leaves.len() {
            let sub = &leaves[..size];
            let root = merkle_root(sub);
            for index in 0..size {
                let proof = inclusion_proof(sub, index);
                assert_eq!(
                    verify_inclusion_proof(index, size, &sub[index], &root, &proof),
                    Ok(()),
                    "inclusion failed at size={size} index={index}"
                );
            }
        }
    }

    /// Consistency proofs round-trip for every 0 < m <= n up to a small
    /// bound, and a proof against the wrong earlier or later root is
    /// `RootMismatch` (the inconsistent-extension case).
    #[test]
    fn consistency_round_trips_and_rejects_wrong_roots() {
        let leaves: Vec<Hash> = (0u16..20).map(|n| leaf_hash(&n.to_le_bytes())).collect();
        let wrong = merkle_root(&[leaf_hash(b"elsewhere")]);
        for n in 1..=leaves.len() {
            let second = &leaves[..n];
            let second_root = merkle_root(second);
            for m in 1..=n {
                let first_root = merkle_root(&leaves[..m]);
                let proof = consistency_proof(second, m);
                assert_eq!(
                    verify_consistency_proof(m, &first_root, n, &second_root, &proof),
                    Ok(()),
                    "consistency failed at m={m} n={n}"
                );
                if m < n {
                    assert_eq!(
                        verify_consistency_proof(m, &wrong, n, &second_root, &proof),
                        Err(ProofError::RootMismatch),
                        "accepted a wrong earlier root at m={m} n={n}"
                    );
                    assert_eq!(
                        verify_consistency_proof(m, &first_root, n, &wrong, &proof),
                        Err(ProofError::RootMismatch),
                        "accepted a wrong later root at m={m} n={n}"
                    );
                }
            }
        }
    }

    /// An empty first tree is consistent with any later tree, the empty
    /// proof - prover and verifier agree on `first_size == 0` (the case the
    /// prover used to debug-assert away).
    #[test]
    fn consistency_with_an_empty_first_tree_is_the_empty_proof() {
        let leaves: Vec<Hash> = (0u16..5).map(|n| leaf_hash(&n.to_le_bytes())).collect();
        let empty_root = merkle_root(&[]);
        let full_root = merkle_root(&leaves);
        assert!(consistency_proof(&leaves, 0).is_empty());
        assert_eq!(
            verify_consistency_proof(0, &empty_root, 5, &full_root, &[]),
            Ok(())
        );
    }

    /// A genuine consistency proof does not authenticate which rows are the
    /// suffix - the overclaim the row-inclusion proofs exist to close. The
    /// proof verifies between the two roots regardless of any rows; only an
    /// inclusion proof binds a specific leaf to a position. This pins that
    /// the two checks are independent.
    #[test]
    fn consistency_alone_says_nothing_about_specific_rows() {
        let leaves: Vec<Hash> = (0u16..7).map(|n| leaf_hash(&n.to_le_bytes())).collect();
        let (m, n) = (3, 7);
        let first_root = merkle_root(&leaves[..m]);
        let second_root = merkle_root(&leaves[..n]);
        let proof = consistency_proof(&leaves[..n], m);
        // Consistency holds...
        assert_eq!(
            verify_consistency_proof(m, &first_root, n, &second_root, &proof),
            Ok(())
        );
        // ...yet a forged "suffix row" has no inclusion at its position.
        let forged = leaf_hash(b"not the real row 5");
        assert_eq!(
            verify_inclusion_proof(
                5,
                n,
                &forged,
                &second_root,
                &inclusion_proof(&leaves[..n], 5)
            ),
            Err(ProofError::RootMismatch)
        );
    }

    /// A proof of the wrong length is `Malformed`, distinct from a data
    /// disagreement.
    #[test]
    fn wrong_length_proofs_are_malformed() {
        let leaves: Vec<Hash> = (0u16..7).map(|n| leaf_hash(&n.to_le_bytes())).collect();
        let root = merkle_root(&leaves);
        let mut too_long = inclusion_proof(&leaves, 0);
        too_long.push(leaf_hash(b"extra"));
        assert_eq!(
            verify_inclusion_proof(0, leaves.len(), &leaves[0], &root, &too_long),
            Err(ProofError::Malformed)
        );

        let first_root = merkle_root(&leaves[..3]);
        let mut short = consistency_proof(&leaves, 3);
        short.pop();
        assert_eq!(
            verify_consistency_proof(3, &first_root, leaves.len(), &root, &short),
            Err(ProofError::Malformed)
        );
    }
}
