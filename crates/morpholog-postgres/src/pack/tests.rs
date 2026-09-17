use super::*;

/// A dummy audit row built through the same `Deserialize` the pack
/// uses. The content is irrelevant to envelope validation (which runs
/// before any hashing); only the `(committed_at, transition_id)`
/// coordinates matter here.
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

/// A dummy checkpoint. Its hashes are placeholders - envelope checks
/// compare strings, they do not recompute the Merkle root (that is
/// `verify_tree`, reached only after a well-formed envelope).
fn checkpoint(tree_size: i64) -> Checkpoint {
    Checkpoint {
        tree_size,
        root_hash: format!("sha256:{tree_size:0>64}"),
        prev_checkpoint_hash: None,
        checkpoint_hash: format!("cp-{tree_size}"),
        signatures: Vec::new(),
        witnesses: Vec::new(),
    }
}

fn manifest_for(c: &Checkpoint) -> PackManifest {
    PackManifest {
        pack_format_version: PACK_FORMAT_V1,
        tree_size: c.tree_size,
        root_hash: c.root_hash.clone(),
        checkpoint_hash: c.checkpoint_hash.clone(),
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
    // The load-bearing case from review: rows past the covering
    // checkpoint must NOT ride along unproven.
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
    // Hostile JSON the runtime could never produce: rejected
    // before anything indexes with it.
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
    // The disagreement check is a disjunction: any ONE lying field
    // must fail the pack, not only all of them together.
    for field in ["tree_size", "root_hash", "checkpoint_hash"] {
        let cp = checkpoint(1);
        let mut manifest = manifest_for(&cp);
        match field {
            "tree_size" => manifest.tree_size = 999,
            "root_hash" => manifest.root_hash = format!("sha256:{}", "f".repeat(64)),
            _ => manifest.checkpoint_hash = "cp-forged".to_string(),
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
    // The authority question is asked when the chain OR the anchor
    // carries signatures: an unsigned pack presented against a
    // signed anchor must still have that anchor's key judged - and
    // with no key claims in the rows, judged unauthorized.
    let rows = rows_tagged(2, 'a');
    let leaves: Vec<Hash> = rows.iter().map(|r| audit_leaf_hash(r).unwrap()).collect();
    let cp = real_checkpoint(&leaves, 2, None);
    let key = crate::signing::generate_signing_key();
    let head = crate::signing::TreeHead {
        tree_size: cp.tree_size,
        root_hash: &cp.root_hash,
        prev_checkpoint_hash: cp.prev_checkpoint_hash.as_deref(),
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
            2 => pack.manifest.from_checkpoint_hash = "forged".to_string(),
            3 => pack.manifest.to_checkpoint_hash = "forged".to_string(),
            4 => pack.manifest.from_root_hash = "forged".to_string(),
            _ => pack.manifest.to_root_hash = "forged".to_string(),
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
    // No anchor at all: signatures stored on the pack's own
    // checkpoints must trigger the authority question too.
    let rows = rows_tagged(2, 'a');
    let leaves: Vec<Hash> = rows.iter().map(|r| audit_leaf_hash(r).unwrap()).collect();
    let mut cp = real_checkpoint(&leaves, 2, None);
    let key = crate::signing::generate_signing_key();
    let head = crate::signing::TreeHead {
        tree_size: cp.tree_size,
        root_hash: &cp.root_hash,
        prev_checkpoint_hash: cp.prev_checkpoint_hash.as_deref(),
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
    // The negative-size rejections must not creep up to zero: a v1
    // chain starting at genesis verifies outright, and a zero
    // endpoint on the window and selective kinds is never refused
    // AS negative (whatever else their envelopes demand of it).
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
    let root = render_hash(&merkle_root(&leaves[..size]));
    let prev_hash = prev.map(|c| c.checkpoint_hash.clone());
    let checkpoint_hash = checkpoint_hash(size as i64, &root, prev_hash.as_deref());
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
    // The overclaim-1 guard at the pack level: a genuine consistency
    // proof does not protect the rows; only inclusion does. Mutating a
    // row's body (coordinates unchanged, so the envelope passes) is
    // caught by its inclusion proof, not the consistency proof.
    let (mut pack, _) = valid_window(3, 7);
    pack.rows[0].invariant_epoch = 999;
    assert_eq!(
        verify_window(&pack, None).unwrap(),
        WindowVerification::RowNotIncluded { leaf_index: 3 }
    );
}

#[test]
fn a_rewritten_prior_prefix_is_an_inconsistent_extension() {
    // The to-checkpoint is over the real history; the from-checkpoint
    // claims a different prefix root (the prior period was rewritten).
    // Consistency must reject - and this is a genuine fork, not a
    // corrupted proof.
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
    pack.to_checkpoint.checkpoint_hash = "cp-forged".into();
    pack.manifest.to_checkpoint_hash = "cp-forged".into();
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
    // Only the declared leaf index binds a row to its position; array
    // order carries no proof weight. Swapping two proofs must therefore
    // fail inclusion, not pass by coincidence of ordering.
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
    p.manifest.root_hash = format!("sha256:{}", "b".repeat(64));
    expect_malformed(&p, "manifest disagrees");

    let mut p = pack.clone();
    p.checkpoint.checkpoint_hash = "forged".to_string();
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
