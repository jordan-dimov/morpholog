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
        attestation: None,
        parameters: None,
    }
}

/// The frozen leaf hash of [`fixed_row`] under the original encoding.
/// Any change to the bytes (field order, length prefixes, codec, version
/// byte) moves it; such a change must come as a NEW leaf version, never
/// an edit to the encoding historical roots use.
#[test]
fn frozen_v1_leaf_hash_pins_the_canonical_encoding() {
    let hash = audit_leaf_hash(&fixed_row()).unwrap();
    assert_eq!(
        Digest::from_bytes(hash).to_string(),
        "sha256:d9b263c7ced1cdbebae9371350204a30da05879720cf414e1cae0bf23c174be9"
    );
}

/// A pack is hashed as written, so a leaf can see digits finer than
/// the database keeps; they floor to the microsecond, before 1970 too.
#[test]
fn a_sub_microsecond_instant_hashes_as_its_floored_microsecond() {
    let leaf = |at: &str| {
        audit_leaf_hash(&AuditRow {
            committed_at: at.parse().unwrap(),
            ..fixed_row()
        })
        .unwrap()
    };
    assert_eq!(
        leaf("1969-12-31T23:59:59.999999500Z"),
        leaf("1969-12-31T23:59:59.999999Z")
    );
    assert_eq!(
        leaf("1970-01-01T00:00:00.000000500Z"),
        leaf("1970-01-01T00:00:00Z")
    );
    assert_eq!(
        leaf("1969-12-31T23:59:58.9999995Z"),
        leaf("1969-12-31T23:59:58.999999Z")
    );
}

/// [`fixed_row`] with a gateway attestation, hashing under the attested
/// encoding.
fn attested_fixed_row() -> AuditRow {
    AuditRow {
        attestation: Some(crate::AuditAttestation::Gateway {
            authenticated_by: "morpholog_writer".to_string(),
            authenticated_by_oid: None,
        }),
        ..fixed_row()
    }
}

fn attested_with_oid(oid: Option<u32>) -> AuditRow {
    AuditRow {
        attestation: Some(crate::AuditAttestation::Gateway {
            authenticated_by: "morpholog_writer".to_string(),
            authenticated_by_oid: oid,
        }),
        ..stamped_fixed_row()
    }
}

/// The role's OID is inside the leaf: changing only the OID changes the
/// hash, and a row without one hashes exactly as rows always have (the
/// frozen vectors above hold that byte for byte).
#[test]
fn the_role_oid_is_committed_to_by_the_leaf() {
    let leaf = |oid| audit_leaf_hash(&attested_with_oid(oid)).unwrap();
    assert_ne!(leaf(Some(100)), leaf(Some(200)));
    assert_ne!(leaf(Some(100)), leaf(None));
    assert_eq!(leaf(None), audit_leaf_hash(&stamped_fixed_row()).unwrap());
}

/// The attestation is hashed as its JSON bytes, so those bytes are the
/// encoding: the OID follows the name, and is absent when unknown.
#[test]
fn the_attestation_bytes_with_and_without_an_oid() {
    let bytes = |oid| {
        serde_json::to_string(&crate::AuditAttestation::Gateway {
            authenticated_by: "gm_human".to_string(),
            authenticated_by_oid: oid,
        })
        .unwrap()
    };
    assert_eq!(
        bytes(Some(9_778_843)),
        r#"{"mode":"gateway","authenticated_by":"gm_human","authenticated_by_oid":9778843}"#
    );
    assert_eq!(
        bytes(None),
        r#"{"mode":"gateway","authenticated_by":"gm_human"}"#
    );
}

fn stamped_fixed_row() -> AuditRow {
    AuditRow {
        parameters: Some(vec!["entry".to_string(), "amount".to_string()]),
        ..attested_fixed_row()
    }
}

/// The frozen leaf hash of the self-describing twin, computed separately
/// in Python from the documented layout: version byte 3, the attested
/// fields, then the names as one JSON array.
#[test]
fn frozen_v3_leaf_hash_pins_the_self_describing_encoding() {
    let hash = audit_leaf_hash(&stamped_fixed_row()).unwrap();
    assert_eq!(
        Digest::from_bytes(hash).to_string(),
        "sha256:e026e4d49353c7437c6b3b82a5938848fe26ea6be95fa53c927894323508c61f"
    );
}

/// Names on an unattested row, or names that do not match the arguments,
/// describe no row the runtime writes, so there is nothing to hash. A pack
/// carrying one is malformed, never intact under the nearest version.
#[test]
fn an_impossible_row_shape_gets_no_leaf() {
    let unattested = AuditRow {
        attestation: None,
        ..stamped_fixed_row()
    };
    assert!(audit_leaf_hash(&unattested).is_err());
    let short = AuditRow {
        parameters: Some(vec!["entry".to_string()]),
        ..stamped_fixed_row()
    };
    assert!(audit_leaf_hash(&short).is_err());
}

/// The frozen leaf hash of the attested twin. Pins the attested
/// encoding byte-exactly: version byte, tagged actor, and the
/// attestation bytes covered whole.
#[test]
fn frozen_v2_leaf_hash_pins_the_attested_encoding() {
    let hash = audit_leaf_hash(&attested_fixed_row()).unwrap();
    assert_eq!(
        Digest::from_bytes(hash).to_string(),
        "sha256:95b17b38bd6318b725b2a901ff5fbbd1e4bf0d4269357a01a9b0d5aa5a55d6f4"
    );
}

/// An attacker who can edit a stored row strips or grafts an attestation.
/// Either way the row changes encoding, so the leaf breaks.
#[test]
fn attestation_presence_selects_the_encoding() {
    let bare = audit_leaf_hash(&fixed_row()).unwrap();
    let attested = audit_leaf_hash(&attested_fixed_row()).unwrap();
    let stamped = audit_leaf_hash(&stamped_fixed_row()).unwrap();
    assert_ne!(bare, attested);
    assert_ne!(attested, stamped);
    assert_ne!(bare, stamped);
}

/// The empty tree is `SHA-256("")` - the fixed RFC 6962 constant.
#[test]
fn empty_tree_is_sha256_of_empty() {
    assert_eq!(
        Digest::from_bytes(merkle_root(&[])).to_string(),
        "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
}

/// A single-leaf tree's root IS the leaf hash.
#[test]
fn single_leaf_root_is_the_leaf() {
    let l = leaf_hash(b"d0");
    assert_eq!(merkle_root(&[l]), l);
}

/// `merkle_root` matches an independent RFC 6962 reference for n = 1..=8:
/// split at the largest power of two below n, hash the two subtrees.
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

/// The frontier gives `merkle_root`'s answer at every size, across
/// empty, single-leaf, power-of-two and ragged trees.
#[test]
fn the_frontier_root_is_the_merkle_root_at_every_size() {
    let leaves: Vec<Hash> = (0u16..300).map(|i| leaf_hash(&i.to_le_bytes())).collect();
    let mut frontier = Frontier::default();
    for n in 0..=leaves.len() {
        assert_eq!(frontier.len(), n);
        assert_eq!(
            frontier.root(),
            merkle_root(&leaves[..n]),
            "the frontier disagrees with merkle_root at n={n}"
        );
        if let Some(leaf) = leaves.get(n) {
            frontier.push(*leaf);
        }
    }
}

/// The two-leaf root is exactly `node(leaf(a), leaf(b))`.
#[test]
fn two_leaf_root_is_one_node() {
    let a = leaf_hash(b"a");
    let b = leaf_hash(b"b");
    assert_eq!(merkle_root(&[a, b]), node_hash(&a, &b));
}

/// A frozen root over leaves `"a"` and `"b"`, computed with `sha256sum`
/// rather than these functions, so it pins compatibility with RFC 6962,
/// not just self-consistency.
#[test]
fn frozen_two_leaf_root_matches_an_independent_sha256() {
    let root = merkle_root(&[leaf_hash(b"a"), leaf_hash(b"b")]);
    assert_eq!(
        Digest::from_bytes(root).to_string(),
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

/// Parsing inverts rendering and refuses anything else.
#[test]
fn a_digest_parses_from_its_rendering_and_refuses_anything_else() {
    let d = Digest::from_bytes(leaf_hash(b"roundtrip"));
    assert_eq!(d.to_string().parse::<Digest>(), Ok(d));
    assert!("no-prefix".parse::<Digest>().is_err());
    assert!("sha256:abcd".parse::<Digest>().is_err()); // too short
    assert!(
        format!("sha256:{}", "zz".repeat(32))
            .parse::<Digest>()
            .is_err()
    ); // non-hex
    assert!(
        d.to_string().to_uppercase().parse::<Digest>().is_err(),
        "uppercase hex is not the record's spelling"
    );
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
        // A different leaf here must not verify, or fake rows could ship
        // under a genuine tree.
        let wrong = leaf_hash(b"forged");
        assert_eq!(
            verify_inclusion_proof(index, t.d.len(), &wrong, &t.root, &proof),
            Err(ProofError::RootMismatch)
        );
    }
}

/// Inclusion proofs round-trip for every (size, index) up to a small
/// bound, beyond the single RFC tree.
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

/// An empty first tree is consistent with any later tree via the empty
/// proof: prover and verifier agree on `first_size == 0`.
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

/// An attacker presents forged suffix rows beside a genuine consistency
/// proof. That proof relates two roots and says nothing about rows; only
/// an inclusion proof binds a leaf to a position, so the forgery fails
/// there.
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
