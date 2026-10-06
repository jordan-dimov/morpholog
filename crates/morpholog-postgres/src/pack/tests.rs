use super::*;

/// A digest from its rendering. Forgeries use different valid digests,
/// never prose, because a checkpoint cannot hold prose.
fn digest(text: &str) -> Digest {
    text.parse().unwrap()
}

/// A dummy audit row, built through the pack's own `Deserialize`. Only
/// its coordinates matter: envelope validation runs before any hashing.
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

/// A dummy checkpoint with placeholder hashes: envelope checks never
/// recompute the Merkle root.
fn checkpoint(tree_size: i64) -> Checkpoint {
    Checkpoint {
        tree_size,
        root_hash: digest(&format!("sha256:{:0>64}", tree_size.unsigned_abs())),
        prev_checkpoint_hash: None,
        checkpoint_hash: digest(&format!("sha256:c{:0>63}", tree_size.unsigned_abs())),
        signatures: Vec::new(),
        witnesses: Vec::new(),
    }
}

fn manifest_for(c: &Checkpoint) -> PackManifest {
    PackManifest {
        pack_format_version: PACK_FORMAT_V1,
        tree_size: c.tree_size,
        root_hash: c.root_hash,
        checkpoint_hash: c.checkpoint_hash,
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
    // Rows past the covering checkpoint must NOT ride along unproven.
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
fn a_negative_checkpoint_size_is_malformed() {
    // Hostile JSON the runtime never produces, rejected before indexing.
    let cp = checkpoint(-1);
    let pack = EvidencePack {
        manifest: manifest_for(&cp),
        checkpoints: vec![cp],
        rows: vec![],
    };
    malformed("negative", &pack);
}

#[test]
fn each_manifest_field_alone_is_enough_to_disagree() {
    // Any ONE lying field must fail the pack.
    for field in ["tree_size", "root_hash", "checkpoint_hash"] {
        let cp = checkpoint(1);
        let mut manifest = manifest_for(&cp);
        match field {
            "tree_size" => manifest.tree_size = 999,
            "root_hash" => manifest.root_hash = digest(&format!("sha256:{}", "f".repeat(64))),
            _ => manifest.checkpoint_hash = digest(&format!("sha256:{}", "e".repeat(64))),
        }
        let pack = EvidencePack {
            manifest,
            checkpoints: vec![checkpoint(1)],
            rows: vec![row(
                "2026-06-24T00:00:00Z",
                "00000000-0000-0000-0000-0000000000a1",
            )],
        };
        malformed("manifest disagrees", &pack);
    }
}

#[test]
fn an_unauthorized_signed_anchor_is_judged_even_on_an_unsigned_chain() {
    // Authority is judged when the chain OR the anchor is signed. With no
    // key claims in the rows, the anchor's key is unauthorized.
    let rows = rows_tagged(2, 'a');
    let leaves: Vec<Hash> = rows.iter().map(|r| audit_leaf_hash(r).unwrap()).collect();
    let cp = real_checkpoint(&leaves, 2, None);
    let key = crate::signing::generate_signing_key();
    let head = crate::signing::TreeHead {
        tree_size: cp.tree_size,
        root_hash: &cp.root_hash,
        prev_checkpoint_hash: cp.prev_checkpoint_hash.as_ref(),
        checkpoint_hash: &cp.checkpoint_hash,
    };
    let signature = crate::signing::sign_tree_head(
        &key,
        crate::checkpoints::AUDIT_CHECKPOINT_PURPOSE,
        "k-unauthorized",
        &head,
    );
    let mut anchor = cp.clone();
    anchor.signatures = vec![crate::checkpoints::TreeHeadSignature {
        key_id: "k-unauthorized".to_string(),
        purpose: crate::checkpoints::AUDIT_CHECKPOINT_PURPOSE.to_string(),
        public_key: crate::signing::render_public_key(&key.verifying_key()),
        signature: crate::signing::render_signature(&signature),
    }];
    let pack = EvidencePack {
        manifest: manifest_for(&cp),
        checkpoints: vec![cp],
        rows,
    };
    let verdict = verify_pack(&pack, Some(&anchor)).unwrap();
    assert!(
        matches!(verdict, TreeVerification::UnauthorizedKey { .. }),
        "the signed anchor's authority must be judged: {verdict:?}"
    );
}

#[test]
fn a_negative_window_endpoint_is_malformed_on_either_side() {
    for side in ["from", "to"] {
        let (mut pack, _) = valid_window(3, 7);
        if side == "from" {
            pack.from_checkpoint.tree_size = -1;
        } else {
            pack.to_checkpoint.tree_size = -1;
        }
        match verify_window(&pack, None) {
            Err(PackError::Malformed { detail }) => {
                assert!(detail.contains("negative"), "{side}: {detail}")
            }
            other => panic!("{side}: expected Malformed, got {other:?}"),
        }
    }
}

#[test]
fn each_window_manifest_field_alone_is_enough_to_disagree() {
    for field in 0..6 {
        let (mut pack, _) = valid_window(3, 7);
        match field {
            0 => pack.manifest.from_tree_size = 999,
            1 => pack.manifest.to_tree_size = 999,
            2 => pack.manifest.from_checkpoint_hash = digest(&format!("sha256:{}", "f".repeat(64))),
            3 => pack.manifest.to_checkpoint_hash = digest(&format!("sha256:{}", "f".repeat(64))),
            4 => pack.manifest.from_root_hash = digest(&format!("sha256:{}", "f".repeat(64))),
            _ => pack.manifest.to_root_hash = digest(&format!("sha256:{}", "f".repeat(64))),
        }
        match verify_window(&pack, None) {
            Err(PackError::Malformed { detail }) => {
                assert!(
                    detail.contains("manifest disagrees"),
                    "field {field}: {detail}"
                )
            }
            other => panic!("field {field}: expected Malformed, got {other:?}"),
        }
    }
}

#[test]
fn an_unauthorized_signature_on_the_chain_itself_is_judged() {
    // With no anchor, signatures on the pack's own checkpoints are judged.
    let rows = rows_tagged(2, 'a');
    let leaves: Vec<Hash> = rows.iter().map(|r| audit_leaf_hash(r).unwrap()).collect();
    let mut cp = real_checkpoint(&leaves, 2, None);
    let key = crate::signing::generate_signing_key();
    let head = crate::signing::TreeHead {
        tree_size: cp.tree_size,
        root_hash: &cp.root_hash,
        prev_checkpoint_hash: cp.prev_checkpoint_hash.as_ref(),
        checkpoint_hash: &cp.checkpoint_hash,
    };
    let signature = crate::signing::sign_tree_head(
        &key,
        crate::checkpoints::AUDIT_CHECKPOINT_PURPOSE,
        "k-chain",
        &head,
    );
    cp.signatures = vec![crate::checkpoints::TreeHeadSignature {
        key_id: "k-chain".to_string(),
        purpose: crate::checkpoints::AUDIT_CHECKPOINT_PURPOSE.to_string(),
        public_key: crate::signing::render_public_key(&key.verifying_key()),
        signature: crate::signing::render_signature(&signature),
    }];
    let pack = EvidencePack {
        manifest: manifest_for(&cp),
        checkpoints: vec![cp],
        rows,
    };
    let verdict = verify_pack(&pack, None).unwrap();
    assert!(
        matches!(verdict, TreeVerification::UnauthorizedKey { .. }),
        "chain signatures must be judged without an anchor: {verdict:?}"
    );
}

#[test]
fn zero_is_the_genesis_boundary_not_a_negative_size() {
    // Zero is not negative: a v1 chain from genesis verifies, and a zero
    // endpoint on the other kinds is never refused AS negative.
    let rows = rows_tagged(2, 'a');
    let leaves: Vec<Hash> = rows.iter().map(|r| audit_leaf_hash(r).unwrap()).collect();
    let genesis = real_checkpoint(&[], 0, None);
    let covering = real_checkpoint(&leaves, 2, Some(&genesis));
    let pack = EvidencePack {
        manifest: manifest_for(&covering),
        checkpoints: vec![genesis.clone(), covering],
        rows,
    };
    assert!(
        matches!(
            verify_pack(&pack, None).unwrap(),
            TreeVerification::Intact { .. }
        ),
        "a chain starting at genesis verifies"
    );

    let (mut window, _) = valid_window(3, 7);
    window.from_checkpoint.tree_size = 0;
    if let Err(PackError::Malformed { detail }) = verify_window(&window, None) {
        assert!(
            !detail.contains("negative"),
            "zero is not negative: {detail}"
        );
    }

    let (mut selective, _) = valid_selective(4, &[1]);
    selective.checkpoint.tree_size = 0;
    if let Err(PackError::Malformed { detail }) = verify_selective(&selective, None) {
        assert!(
            !detail.contains("negative"),
            "zero is not negative: {detail}"
        );
    }
}

#[test]
fn a_negative_selective_checkpoint_size_is_malformed() {
    let (mut pack, _) = valid_selective(4, &[1]);
    pack.checkpoint.tree_size = -1;
    match verify_selective(&pack, None) {
        Err(PackError::Malformed { detail }) => assert!(detail.contains("negative")),
        other => panic!("expected Malformed, got {other:?}"),
    }
}

#[test]
fn a_window_start_names_its_tree_size_exactly() {
    assert_eq!(WindowStart::TreeSize(5).tree_size(), 5);
    assert_eq!(WindowStart::Anchor(checkpoint(7)).tree_size(), 7);
}

#[test]
fn the_selective_assembly_error_says_what_went_wrong() {
    let rendered = format!("{}", AssembleSelectiveError::UnknownTransition(Uuid::nil()));
    assert!(!rendered.is_empty() && rendered.contains("00000000"));
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

// --- window packs ---

use crate::merkle::merkle_root;

/// `n` rows in canonical order, each tagged so a different `tag` yields
/// a different leaf at the same coordinates (a rewritten-prefix forgery).
fn rows_tagged(n: usize, tag: char) -> Vec<AuditRow> {
    (0..n)
        .map(|i| {
            serde_json::from_value(serde_json::json!({
                "transition_id": format!("00000000-0000-0000-0000-{:012x}", i + 1),
                "transformation_name": format!("X{tag}"),
                "arguments": [],
                "actor": { "type": "subject", "value": "00000000-0000-0000-0000-000000000001" },
                "invariant_epoch": 1,
                "invariants_checked": [],
                "asserted_claims": [],
                "retracted_claims": [],
                "emitted_intents": [],
                "committed_at": format!("2026-06-24T00:00:{:02}Z", i),
            }))
            .unwrap()
        })
        .collect()
}

fn real_checkpoint(leaves: &[Hash], size: usize, prev: Option<&Checkpoint>) -> Checkpoint {
    let root = Digest::from_bytes(merkle_root(&leaves[..size]));
    let prev_hash = prev.map(|c| c.checkpoint_hash);
    let checkpoint_hash = checkpoint_hash(size as i64, &root, prev_hash.as_ref());
    Checkpoint {
        tree_size: size as i64,
        root_hash: root,
        prev_checkpoint_hash: prev_hash,
        checkpoint_hash,
        signatures: Vec::new(),
        witnesses: Vec::new(),
    }
}

/// A valid window pack over a single `tag`-history of `to` rows, with the
/// from-checkpoint at `from`. Returns the pack and the from-checkpoint
/// (the prior anchor a verifier would hold).
fn valid_window(from: usize, to: usize) -> (WindowEvidencePack, Checkpoint) {
    let rows = rows_tagged(to, 'a');
    let leaves: Vec<Hash> = rows.iter().map(|r| audit_leaf_hash(r).unwrap()).collect();
    let from_cp = real_checkpoint(&leaves, from, None);
    let to_cp = real_checkpoint(&leaves, to, Some(&from_cp));
    let pack = assemble_window_pack(&rows, from_cp.clone(), to_cp).unwrap();
    (pack, from_cp)
}

#[test]
fn a_window_round_trips_and_matches_its_anchor() {
    let (pack, anchor) = valid_window(3, 7);
    let intact = WindowVerification::Intact {
        from_tree_size: 3,
        to_tree_size: 7,
        rows: 4,
    };
    assert_eq!(verify_window(&pack, None).unwrap(), intact);
    assert_eq!(verify_window(&pack, Some(&anchor)).unwrap(), intact);
}

#[test]
fn a_wrong_anchor_is_caught_even_when_the_proof_verifies() {
    let (pack, _) = valid_window(3, 7);
    // An anchor from an unrelated history at the same size.
    let other: Vec<Hash> = rows_tagged(3, 'z')
        .iter()
        .map(|r| audit_leaf_hash(r).unwrap())
        .collect();
    let wrong_anchor = real_checkpoint(&other, 3, None);
    assert!(matches!(
        verify_window(&pack, Some(&wrong_anchor)),
        Ok(WindowVerification::AnchorMismatch { .. })
    ));
}

#[test]
fn a_tampered_window_row_is_not_included() {
    // A consistency proof does not protect the rows; only inclusion does.
    // A changed row body (same coordinates, so the envelope passes) is
    // caught by its inclusion proof.
    let (mut pack, _) = valid_window(3, 7);
    pack.rows[0].invariant_epoch = 999;
    assert_eq!(
        verify_window(&pack, None).unwrap(),
        WindowVerification::RowNotIncluded { leaf_index: 3 }
    );
}

#[test]
fn a_rewritten_prior_prefix_is_an_inconsistent_extension() {
    // The from-checkpoint claims a rewritten prior period. Consistency must
    // reject it as a genuine fork, not a corrupted proof.
    let rows = rows_tagged(7, 'a');
    let leaves: Vec<Hash> = rows.iter().map(|r| audit_leaf_hash(r).unwrap()).collect();
    let to_cp = real_checkpoint(&leaves, 7, None);
    let forged: Vec<Hash> = rows_tagged(3, 'b')
        .iter()
        .map(|r| audit_leaf_hash(r).unwrap())
        .collect();
    let from_cp = real_checkpoint(&forged, 3, None);
    let pack = assemble_window_pack(&rows, from_cp, to_cp).unwrap();
    assert_eq!(
        verify_window(&pack, None).unwrap(),
        WindowVerification::InconsistentExtension {
            from_tree_size: 3,
            to_tree_size: 7,
        }
    );
}

#[test]
fn a_wrong_window_row_count_is_malformed() {
    let (mut pack, _) = valid_window(3, 7);
    pack.rows.pop();
    pack.inclusion_proofs.pop();
    match verify_window(&pack, None) {
        Err(PackError::Malformed { detail }) => assert!(detail.contains("covers 4 rows")),
        other => panic!("expected Malformed, got {other:?}"),
    }
}

#[test]
fn a_wrong_declared_leaf_index_is_malformed() {
    let (mut pack, _) = valid_window(3, 7);
    pack.inclusion_proofs[1].leaf_index = 99;
    match verify_window(&pack, None) {
        Err(PackError::Malformed { detail }) => assert!(detail.contains("leaf_index")),
        other => panic!("expected Malformed, got {other:?}"),
    }
}

#[test]
fn a_forged_checkpoint_hash_is_malformed() {
    let (mut pack, _) = valid_window(3, 7);
    pack.to_checkpoint.checkpoint_hash = digest(&format!("sha256:{}", "f".repeat(64)));
    pack.manifest.to_checkpoint_hash = digest(&format!("sha256:{}", "f".repeat(64)));
    match verify_window(&pack, None) {
        Err(PackError::Malformed { detail }) => {
            assert!(detail.contains("does not match its contents"))
        }
        other => panic!("expected Malformed, got {other:?}"),
    }
}

#[test]
fn an_unknown_window_field_is_rejected() {
    let (pack, _) = valid_window(3, 7);
    let mut v = serde_json::to_value(&pack).unwrap();
    v["surprise"] = serde_json::json!("not part of the proof");
    assert!(serde_json::from_value::<WindowEvidencePack>(v).is_err());
}

// -- selective packs ----------------------------------------------------

/// A valid selective pack over a `to`-row history, disclosing the rows
/// at `pick` (indices). Returns the pack and its covering checkpoint.
fn valid_selective(to: usize, pick: &[usize]) -> (SelectiveEvidencePack, Checkpoint) {
    let rows = rows_tagged(to, 'a');
    let leaves: Vec<Hash> = rows.iter().map(|r| audit_leaf_hash(r).unwrap()).collect();
    let covering = real_checkpoint(&leaves, to, None);
    let ids: Vec<Uuid> = pick.iter().map(|&i| rows[i].transition_id).collect();
    let pack = assemble_selective_pack(&rows, covering.clone(), &ids).unwrap();
    (pack, covering)
}

#[test]
fn a_selective_pack_round_trips_and_matches_its_anchor() {
    let (pack, anchor) = valid_selective(7, &[1, 4, 6]);
    let intact = SelectiveVerification::Intact {
        tree_size: 7,
        rows_disclosed: 3,
    };
    assert_eq!(verify_selective(&pack, None).unwrap(), intact);
    assert_eq!(verify_selective(&pack, Some(&anchor)).unwrap(), intact);
}

#[test]
fn a_selection_is_assembled_in_leaf_order_regardless_of_request_order() {
    let rows = rows_tagged(5, 'a');
    let leaves: Vec<Hash> = rows.iter().map(|r| audit_leaf_hash(r).unwrap()).collect();
    let covering = real_checkpoint(&leaves, 5, None);
    let ids = vec![rows[4].transition_id, rows[0].transition_id];
    let pack = assemble_selective_pack(&rows, covering, &ids).unwrap();
    let indices: Vec<i64> = pack.inclusion_proofs.iter().map(|p| p.leaf_index).collect();
    assert_eq!(indices, vec![0, 4]);
    assert!(matches!(
        verify_selective(&pack, None),
        Ok(SelectiveVerification::Intact { .. })
    ));
}

#[test]
fn a_tampered_disclosed_row_is_not_included() {
    let (mut pack, _) = valid_selective(7, &[2, 5]);
    pack.rows[1].transformation_name = "forged".into();
    assert_eq!(
        verify_selective(&pack, None).unwrap(),
        SelectiveVerification::RowNotIncluded { leaf_index: 5 }
    );
}

#[test]
fn swapped_inclusion_proofs_are_row_not_included() {
    // Only the declared leaf index binds a row to its position, so swapping
    // two proofs must fail inclusion.
    let (mut pack, _) = valid_selective(7, &[2, 5]);
    let a = pack.inclusion_proofs[0].proof.clone();
    let b = pack.inclusion_proofs[1].proof.clone();
    pack.inclusion_proofs[0].proof = b;
    pack.inclusion_proofs[1].proof = a;
    assert!(matches!(
        verify_selective(&pack, None),
        Ok(SelectiveVerification::RowNotIncluded { .. })
    ));
}

#[test]
fn prover_refusals_are_errors_not_packs() {
    let rows = rows_tagged(3, 'a');
    let leaves: Vec<Hash> = rows.iter().map(|r| audit_leaf_hash(r).unwrap()).collect();
    let covering = real_checkpoint(&leaves, 3, None);

    let ghost = Uuid::from_u128(99);
    assert!(matches!(
        assemble_selective_pack(&rows, covering.clone(), &[ghost]),
        Err(AssembleSelectiveError::UnknownTransition(id)) if id == ghost
    ));
    let dup = rows[1].transition_id;
    assert!(matches!(
        assemble_selective_pack(&rows, covering.clone(), &[dup, dup]),
        Err(AssembleSelectiveError::DuplicateTransition(id)) if id == dup
    ));
    assert!(matches!(
        assemble_selective_pack(&rows, covering, &[]),
        Err(AssembleSelectiveError::EmptySelection)
    ));
}

#[test]
fn selective_envelope_rules_are_each_enforced() {
    let expect_malformed =
        |pack: &SelectiveEvidencePack, needle: &str| match verify_selective(pack, None) {
            Err(PackError::Malformed { detail }) => assert!(
                detail.contains(needle),
                "expected detail to mention {needle:?}, got {detail:?}"
            ),
            other => panic!("expected Malformed for {needle:?}, got {other:?}"),
        };

    let (pack, _) = valid_selective(7, &[1, 4]);

    let mut p = pack.clone();
    p.manifest.pack_format_version = 9;
    expect_malformed(&p, "pack_format_version");

    let mut p = pack.clone();
    p.manifest.pack_kind = "window".to_string();
    expect_malformed(&p, "pack_kind");

    let mut p = pack.clone();
    p.manifest.tree_size = 6;
    expect_malformed(&p, "manifest disagrees");

    let mut p = pack.clone();
    p.manifest.root_hash = digest(&format!("sha256:{}", "b".repeat(64)));
    expect_malformed(&p, "manifest disagrees");

    let mut p = pack.clone();
    p.checkpoint.checkpoint_hash = digest(&format!("sha256:{}", "f".repeat(64)));
    expect_malformed(&p, "does not match its contents");

    let mut p = pack.clone();
    p.inclusion_proofs[0].leaf_index = -1;
    expect_malformed(&p, "outside the checkpoint");

    let mut p = pack.clone();
    p.inclusion_proofs[1].leaf_index = 7;
    expect_malformed(&p, "outside the checkpoint");

    let mut p = pack.clone();
    p.inclusion_proofs.swap(0, 1);
    expect_malformed(&p, "strictly increasing");

    let mut p = pack.clone();
    p.inclusion_proofs.pop();
    expect_malformed(&p, "inclusion proofs");

    let mut p = pack.clone();
    p.rows.clear();
    p.inclusion_proofs.clear();
    expect_malformed(&p, "at least one row");
}

#[test]
fn a_wrong_anchor_is_a_selective_anchor_mismatch() {
    let (pack, _) = valid_selective(7, &[1, 4]);
    let other: Vec<Hash> = rows_tagged(7, 'z')
        .iter()
        .map(|r| audit_leaf_hash(r).unwrap())
        .collect();
    let wrong_anchor = real_checkpoint(&other, 7, None);
    assert!(matches!(
        verify_selective(&pack, Some(&wrong_anchor)),
        Ok(SelectiveVerification::AnchorMismatch { .. })
    ));
}

#[test]
fn a_selective_pack_reveals_nothing_about_undisclosed_rows() {
    // Row 0 is disclosed deliberately: its id coincides with the
    // fixture's shared actor value, so it cannot witness a leak.
    let (pack, _) = valid_selective(7, &[0, 4]);
    let bytes = serde_json::to_string(&pack).unwrap();
    let rows = rows_tagged(7, 'a');
    for (i, row) in rows.iter().enumerate() {
        let id = row.transition_id.to_string();
        if i == 0 || i == 4 {
            assert!(bytes.contains(&id), "disclosed row {i} must be present");
        } else {
            assert!(
                !bytes.contains(&id),
                "undisclosed row {i} leaked its transition id"
            );
        }
    }
}

#[test]
fn a_selective_pack_rejects_unknown_fields() {
    let (pack, _) = valid_selective(3, &[0]);
    let mut v = serde_json::to_value(&pack).unwrap();
    v["surprise"] = serde_json::json!("not part of the proof");
    assert!(serde_json::from_value::<SelectiveEvidencePack>(v).is_err());
}

// Complete-prefix packs as NDJSON. Attacker capability modelled: full
// control of the pack file (reorder, drop, add, edit or truncate lines, or
// append data); the verifier holds nothing but the file and an optional
// anchor.

/// A genuine streamed pack over `n` rows, checkpointed at `sizes`.
fn streamed_pack(n: usize, sizes: &[usize]) -> (Vec<String>, Vec<Checkpoint>) {
    let rows = rows_tagged(n, 'a');
    let leaves: Vec<Hash> = rows.iter().map(|r| audit_leaf_hash(r).unwrap()).collect();
    let mut chain: Vec<Checkpoint> = Vec::new();
    for &size in sizes {
        let cp = real_checkpoint(&leaves, size, chain.last());
        chain.push(cp);
    }
    let covering = chain.last().unwrap();
    let manifest = PrefixPackManifest {
        pack_format_version: 4,
        pack_kind: "prefix".into(),
        tree_size: covering.tree_size,
        root_hash: covering.root_hash,
        checkpoint_hash: covering.checkpoint_hash,
        checkpoint_count: chain.len() as u64,
    };
    let mut lines = vec![serde_json::to_string(&manifest).unwrap()];
    lines.extend(chain.iter().map(|c| serde_json::to_string(c).unwrap()));
    lines.extend(rows.iter().map(|r| serde_json::to_string(r).unwrap()));
    (lines, chain)
}

fn bytes(lines: &[String]) -> Vec<u8> {
    lines
        .iter()
        .flat_map(|l| format!("{l}\n").into_bytes())
        .collect()
}

fn stream_verdict(
    input: &[u8],
    anchor: Option<&Checkpoint>,
) -> Result<TreeVerification, PackError> {
    verify_prefix_stream(input, anchor).map(|report| report.verdict)
}

fn stream_malformed(input: &[u8], detail_contains: &str) {
    match stream_verdict(input, None) {
        Err(PackError::Malformed { detail }) => assert!(
            detail.contains(detail_contains),
            "expected detail to mention {detail_contains:?}, got {detail:?}"
        ),
        other => panic!("expected Malformed ({detail_contains}), got {other:?}"),
    }
}

/// Line 1, the chain, then the rows, in the header's own counts.
const HEADER: usize = 1 + 2;

#[test]
fn a_streamed_pack_verifies_and_agrees_with_the_same_rows_as_one_document() {
    let (lines, chain) = streamed_pack(5, &[2, 5]);
    let report = verify_prefix_stream(&bytes(&lines)[..], None).unwrap();
    let rows: Vec<AuditRow> = lines[HEADER..]
        .iter()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let covering = chain.last().unwrap();
    let document = EvidencePack {
        manifest: manifest_for(covering),
        checkpoints: chain.clone(),
        rows,
    };
    assert!(matches!(report.verdict, TreeVerification::Intact { .. }));
    assert_eq!(report.verdict, verify_pack(&document, None).unwrap());
    assert_eq!(report.checkpoints, chain);
    assert_eq!(streamed_pack_version(lines[0].as_bytes()), Some(4));
    let one_document = serde_json::to_string(&document).unwrap();
    assert_eq!(streamed_pack_version(one_document.as_bytes()), None);
    assert_eq!(pack_format_version(one_document.as_bytes()), Some(1));
}

#[test]
fn every_break_in_the_line_format_is_malformed() {
    let (lines, _) = streamed_pack(5, &[2, 5]);
    let edit = |f: &dyn Fn(&mut Vec<String>)| {
        let mut l = lines.clone();
        f(&mut l);
        bytes(&l)
    };

    stream_malformed(
        &edit(&|l| l[0] = l[0].replace("\"prefix\"", "\"window\"")),
        "not a complete-prefix pack",
    );
    stream_malformed(
        &edit(&|l| l[0] = l[0].replacen('{', "{\"extra\":1,", 1)),
        "the manifest that does not parse",
    );
    stream_malformed(
        &edit(&|l| l[0] = l[0].replace("\"checkpoint_count\":2", "\"checkpoint_count\":3")),
        "a checkpoint that does not parse",
    );
    stream_malformed(
        &edit(&|l| l.swap(HEADER + 1, HEADER + 2)),
        "not in strictly increasing log order",
    );
    stream_malformed(
        &edit(&|l| l[HEADER + 2] = l[HEADER + 1].clone()),
        "not in strictly increasing log order",
    );
    stream_malformed(
        &edit(&|l| {
            l.pop();
        }),
        "pack carries 4 rows but the covering checkpoint commits to 5",
    );
    stream_malformed(
        &edit(&|l| l.push(l[HEADER].clone())),
        "data after the 5 rows",
    );
    stream_malformed(&edit(&|l| l.push(String::new())), "data after the 5 rows");
    stream_malformed(
        &edit(&|l| l.insert(HEADER, String::new())),
        "an audit row that does not parse",
    );

    let mut unterminated = bytes(&lines);
    unterminated.pop();
    stream_malformed(&unterminated, "does not end in a newline");
    let full = bytes(&lines);
    stream_malformed(&full[..full.len() - 20], "does not end in a newline");
    stream_malformed(
        &full[..lines[0].len() + 1],
        "the pack ends where a checkpoint was expected",
    );
    stream_malformed(b"", "the pack ends where the manifest was expected");
}

/// A break in the format outranks a verdict already reached about the
/// rows before it: the file as a whole proves nothing.
#[test]
fn a_late_break_in_the_format_outranks_an_earlier_tamper() {
    let (lines, chain) = streamed_pack(5, &[2, 5]);
    let mut tampered = lines.clone();
    tampered[HEADER] = tampered[HEADER].replace("\"Xa\"", "\"forged\"");
    assert!(matches!(
        stream_verdict(&bytes(&tampered), None).unwrap(),
        TreeVerification::Tampered { tree_size: 2, .. }
    ));
    let mut and_extra = tampered.clone();
    and_extra.push(tampered[HEADER].clone());
    stream_malformed(&bytes(&and_extra), "data after the 5 rows");

    let mut foreign = chain[0].clone();
    foreign.checkpoint_hash = digest(&format!("sha256:{}", "e".repeat(64)));
    assert!(matches!(
        stream_verdict(&bytes(&lines), Some(&foreign)).unwrap(),
        TreeVerification::AnchorMismatch { .. }
    ));
    let mut late = bytes(&lines);
    late.extend_from_slice(b"{}\n");
    match verify_prefix_stream(&late[..], Some(&foreign)) {
        Err(PackError::Malformed { .. }) => {}
        other => panic!("a malformed pack must outrank the anchor mismatch, got {other:?}"),
    }
}

/// The same rows on the top rung: each names its programme (leaf V4).
fn naming_their_programme(rows: Vec<AuditRow>) -> Vec<AuditRow> {
    rows.into_iter()
        .map(|r| AuditRow {
            attestation: Some(crate::attestation::AuditAttestation::Gateway {
                authenticated_by: "writer".to_string(),
                authenticated_by_oid: None,
            }),
            parameters: Some(Vec::new()),
            model_hash: Some(format!("sha256:{}", "d".repeat(64))),
            ..r
        })
        .collect()
}

/// A genuine streamed pack over `rows`, one checkpoint over all of them,
/// announcing `version`.
fn streamed_pack_of(rows: &[AuditRow], version: u32) -> Vec<u8> {
    let leaves: Vec<Hash> = rows.iter().map(|r| audit_leaf_hash(r).unwrap()).collect();
    let cp = real_checkpoint(&leaves, rows.len(), None);
    let manifest = PrefixPackManifest {
        pack_format_version: version,
        pack_kind: "prefix".into(),
        tree_size: cp.tree_size,
        root_hash: cp.root_hash,
        checkpoint_hash: cp.checkpoint_hash,
        checkpoint_count: 1,
    };
    let mut lines = vec![
        serde_json::to_string(&manifest).unwrap(),
        serde_json::to_string(&cp).unwrap(),
    ];
    lines.extend(rows.iter().map(|r| serde_json::to_string(r).unwrap()));
    bytes(&lines)
}

/// Older rows, then rows naming their programme: the real chronology.
fn mixed_history() -> Vec<AuditRow> {
    let mut rows = rows_tagged(4, 'm');
    let newer = naming_their_programme(rows.split_off(2));
    rows.extend(newer);
    rows
}

#[test]
fn a_stream_disclosing_a_programme_naming_row_is_version_5_and_verifies() {
    let rows = mixed_history();
    assert!(matches!(
        stream_verdict(&streamed_pack_of(&rows, 5), None),
        Ok(TreeVerification::Intact { .. })
    ));
}

/// A version-4 reader drops a field it does not know and hashes the rest,
/// reporting tamper on honest history; so version 4 may not carry one.
#[test]
fn a_version_4_stream_cannot_disclose_a_programme_naming_row() {
    match stream_verdict(&streamed_pack_of(&mixed_history(), 4), None) {
        Err(PackError::Malformed { detail }) => {
            assert!(detail.contains("such a pack is version 5"), "{detail}");
        }
        other => panic!("expected the version to be refused, got {other:?}"),
    }
}

/// The fence holds both ways: version 5 announces what the pack discloses.
#[test]
fn a_version_5_stream_must_disclose_one() {
    match stream_verdict(&streamed_pack_of(&rows_tagged(3, 'u'), 5), None) {
        Err(PackError::Malformed { detail }) => {
            assert!(detail.contains("such a pack is version 4"), "{detail}");
        }
        other => panic!("expected the version to be refused, got {other:?}"),
    }
}

/// The single-document spelling follows the same rule: read whole, a
/// version-5 stream is a version-8 document; labelled 1, it is refused.
#[test]
fn a_programme_naming_document_is_version_8() {
    let pack = read_prefix_stream(&streamed_pack_of(&mixed_history(), 5)[..]).unwrap();
    assert_eq!(pack.manifest.pack_format_version, 8);
    assert!(matches!(
        verify_pack(&pack, None),
        Ok(TreeVerification::Intact { .. })
    ));
    let mut relabelled = pack.clone();
    relabelled.manifest.pack_format_version = PACK_FORMAT_V1;
    malformed("such a pack is version 8", &relabelled);
    let unstamped = read_prefix_stream(&streamed_pack_of(&rows_tagged(2, 'v'), 4)[..]).unwrap();
    let mut overclaimed = unstamped.clone();
    overclaimed.manifest.pack_format_version = 8;
    malformed("such a pack is version 1", &overclaimed);
}

/// Older rows, rows naming their programme, then rows naming their
/// semantics too: the whole ladder, in the real chronology.
fn semantics_history() -> Vec<AuditRow> {
    let mut rows = mixed_history();
    let newest: Vec<AuditRow> = rows_tagged(2, 's')
        .into_iter()
        .enumerate()
        .map(|(i, r)| AuditRow {
            transition_id: uuid::Uuid::from_u128(0x100 + i as u128),
            committed_at: format!("2026-06-24T00:01:{i:02}Z").parse().unwrap(),
            semantics_version: Some(morpholog_core::SEMANTICS_VERSION),
            ..naming_their_programme(vec![r]).remove(0)
        })
        .collect();
    rows.extend(newest);
    rows
}

/// A stream whose newest row names its semantics is version 9 and
/// verifies whole, the older rungs before it included.
#[test]
fn a_stream_disclosing_a_semantics_naming_row_is_version_9_and_verifies() {
    assert!(matches!(
        stream_verdict(&streamed_pack_of(&semantics_history(), 9), None),
        Ok(TreeVerification::Intact { .. })
    ));
}

/// A version-5 reader would drop the semantics field and report tamper on
/// honest history, so version 5 may not carry such a row; and version 9
/// claims one, so it must.
#[test]
fn the_semantics_rung_is_held_both_ways() {
    match stream_verdict(&streamed_pack_of(&semantics_history(), 5), None) {
        Err(PackError::Malformed { detail }) => {
            assert!(detail.contains("such a pack is version 9"), "{detail}");
        }
        other => panic!("expected version 5 to be refused, got {other:?}"),
    }
    match stream_verdict(&streamed_pack_of(&mixed_history(), 9), None) {
        Err(PackError::Malformed { detail }) => {
            assert!(detail.contains("such a pack is version 5"), "{detail}");
        }
        other => panic!("expected version 9 to be refused, got {other:?}"),
    }
}

/// Read whole, a version-9 stream is a version-12 document, held to the
/// same rule.
#[test]
fn a_semantics_naming_document_is_version_12() {
    let pack = read_prefix_stream(&streamed_pack_of(&semantics_history(), 9)[..]).unwrap();
    assert_eq!(pack.manifest.pack_format_version, 12);
    assert!(matches!(
        verify_pack(&pack, None),
        Ok(TreeVerification::Intact { .. })
    ));
    let mut relabelled = pack.clone();
    relabelled.manifest.pack_format_version = 8;
    malformed("such a pack is version 12", &relabelled);
}

/// The whole ladder, then rows that record their draws too: one drew two
/// subjects, one drew none.
fn subjects_history() -> Vec<AuditRow> {
    let mut rows = semantics_history();
    let newest: Vec<AuditRow> = [vec!["s_one", "s_two"], vec![]]
        .into_iter()
        .enumerate()
        .map(|(i, drawn)| {
            let mut row = rows[rows.len() - 1].clone();
            row.transition_id = uuid::Uuid::from_u128(0x200 + i as u128);
            row.committed_at = format!("2026-06-24T00:02:{i:02}Z").parse().unwrap();
            row.drawn_subjects = Some(
                drawn
                    .into_iter()
                    .map(morpholog_core::Subject::from)
                    .collect(),
            );
            row
        })
        .collect();
    rows.extend(newest);
    rows
}

/// A stream whose newest row records its draws is version 13 and verifies
/// whole, every older rung before it included.
#[test]
fn a_stream_disclosing_a_draw_recording_row_is_version_13_and_verifies() {
    assert!(matches!(
        stream_verdict(&streamed_pack_of(&subjects_history(), 13), None),
        Ok(TreeVerification::Intact { .. })
    ));
}

/// A version-9 reader would drop the draws and report tamper on honest
/// history, so version 9 may not carry such a row; and version 13 claims
/// one, so it must.
#[test]
fn the_subjects_rung_is_held_both_ways() {
    match stream_verdict(&streamed_pack_of(&subjects_history(), 9), None) {
        Err(PackError::Malformed { detail }) => {
            assert!(detail.contains("such a pack is version 13"), "{detail}");
        }
        other => panic!("expected version 9 to be refused, got {other:?}"),
    }
    match stream_verdict(&streamed_pack_of(&semantics_history(), 13), None) {
        Err(PackError::Malformed { detail }) => {
            assert!(detail.contains("such a pack is version 9"), "{detail}");
        }
        other => panic!("expected version 13 to be refused, got {other:?}"),
    }
}

/// Read whole, a version-13 stream is a version-16 document, held to the
/// same rule.
#[test]
fn a_draw_recording_document_is_version_16() {
    let pack = read_prefix_stream(&streamed_pack_of(&subjects_history(), 13)[..]).unwrap();
    assert_eq!(pack.manifest.pack_format_version, 16);
    assert!(matches!(
        verify_pack(&pack, None),
        Ok(TreeVerification::Intact { .. })
    ));
    let mut relabelled = pack.clone();
    relabelled.manifest.pack_format_version = 12;
    malformed("such a pack is version 16", &relabelled);
}

/// Every version names one kind of pack, every rung of each kind's ladder
/// included, and nothing past the newest.
#[test]
fn every_version_names_one_kind() {
    let kinds: Vec<_> = (1..=super::NEWEST_PACK_FORMAT)
        .map(|v| super::pack_kind(v.into()))
        .collect();
    assert!(kinds.iter().all(Option::is_some), "{kinds:?}");
    assert_eq!(super::pack_kind(9), Some(super::PackKind::PrefixStream));
    assert_eq!(super::pack_kind(10), Some(super::PackKind::Window));
    assert_eq!(super::pack_kind(11), Some(super::PackKind::Selective));
    assert_eq!(super::pack_kind(12), Some(super::PackKind::PrefixDocument));
    assert_eq!(super::pack_kind(13), Some(super::PackKind::PrefixStream));
    assert_eq!(super::pack_kind(14), Some(super::PackKind::Window));
    assert_eq!(super::pack_kind(15), Some(super::PackKind::Selective));
    assert_eq!(super::pack_kind(16), Some(super::PackKind::PrefixDocument));
    assert_eq!(
        super::pack_kind(u64::from(super::NEWEST_PACK_FORMAT) + 1),
        None
    );
}
