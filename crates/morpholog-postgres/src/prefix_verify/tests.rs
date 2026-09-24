//! The streaming verifier against the verifier it replaces, fed the same
//! exact prefix. Attacker capability modelled: full write access to the
//! stored rows and checkpoints (edit a row, relink or rehash a checkpoint,
//! sign with a key the ledger never authorised, or at a size where it was
//! not yet or no longer authorised), with an optional external anchor as
//! the one copy out of reach. Each case also names the verdict it expects,
//! so a case that tests nothing cannot pass by agreeing.

use ed25519_dalek::SigningKey;

use super::*;
use crate::checkpoints::{
    AUDIT_CHECKPOINT_PURPOSE, TreeHeadSignature, authority_violation, verify_tree,
};
use crate::merkle::{Hash, merkle_root};
use crate::signing::{TreeHead, render_public_key, render_signature, sign_tree_head};

const ROWS: usize = 8;

fn key_claim(id: &str, key: &SigningKey) -> serde_json::Value {
    serde_json::json!({
        "predicate": "AuditSigningKey",
        "args": [
            { "type": "subject", "value": id },
            { "type": "subject", "value": AUDIT_CHECKPOINT_PURPOSE },
            { "type": "subject", "value": render_public_key(&key.verifying_key()) },
        ],
    })
}

/// Row 1 authorises `k1`; row 4 revokes it and authorises `k2`.
fn history(k1: &SigningKey, k2: &SigningKey) -> Vec<AuditRow> {
    (0..ROWS)
        .map(|i| {
            let (asserted, retracted) = match i {
                1 => (vec![key_claim("k1", k1)], vec![]),
                4 => (vec![key_claim("k2", k2)], vec![key_claim("k1", k1)]),
                _ => (vec![], vec![]),
            };
            serde_json::from_value(serde_json::json!({
                "transition_id": format!("00000000-0000-0000-0000-{:012x}", i + 1),
                "transformation_name": "post",
                "arguments": [],
                "actor": { "type": "subject", "value": "alex" },
                "invariant_epoch": 1,
                "invariants_checked": [],
                "asserted_claims": asserted,
                "retracted_claims": retracted,
                "emitted_intents": [],
                "committed_at": format!("2026-06-24T00:00:{i:02}Z"),
            }))
            .unwrap()
        })
        .collect()
}

fn leaves(rows: &[AuditRow]) -> Vec<Hash> {
    rows.iter().map(|r| audit_leaf_hash(r).unwrap()).collect()
}

fn sign(cp: &mut Checkpoint, key: &SigningKey, key_id: &str) {
    let head = TreeHead {
        tree_size: cp.tree_size,
        root_hash: &cp.root_hash,
        prev_checkpoint_hash: cp.prev_checkpoint_hash.as_ref(),
        checkpoint_hash: &cp.checkpoint_hash,
    };
    let signature = sign_tree_head(key, AUDIT_CHECKPOINT_PURPOSE, key_id, &head);
    cp.signatures.push(TreeHeadSignature {
        key_id: key_id.to_string(),
        purpose: AUDIT_CHECKPOINT_PURPOSE.to_string(),
        public_key: render_public_key(&key.verifying_key()),
        signature: render_signature(&signature),
    });
}

/// A genuine chain over `rows` at `sizes`, linked and hashed.
fn chain(rows: &[AuditRow], sizes: &[usize]) -> Vec<Checkpoint> {
    let leaves = leaves(rows);
    let mut chain: Vec<Checkpoint> = Vec::new();
    for &size in sizes {
        let root_hash = Digest::from_bytes(merkle_root(&leaves[..size]));
        let prev = chain.last().map(|c| c.checkpoint_hash);
        chain.push(Checkpoint {
            tree_size: size as i64,
            root_hash,
            prev_checkpoint_hash: prev,
            checkpoint_hash: checkpoint_hash(size as i64, &root_hash, prev.as_ref()),
            signatures: Vec::new(),
            witnesses: Vec::new(),
        });
    }
    chain
}

/// Recompute a checkpoint's identity and every later link after an edit,
/// as a writer with full access would.
fn rehash_from(chain: &mut [Checkpoint], from: usize) {
    for i in from..chain.len() {
        let prev = if i == 0 {
            None
        } else {
            Some(chain[i - 1].checkpoint_hash)
        };
        let cp = &mut chain[i];
        cp.prev_checkpoint_hash = prev;
        cp.checkpoint_hash = checkpoint_hash(cp.tree_size, &cp.root_hash, prev.as_ref());
    }
}

fn old_verdict(
    rows: &[AuditRow],
    cps: &[Checkpoint],
    anchor: Option<&Checkpoint>,
) -> TreeVerification {
    let verdict = verify_tree(&leaves(rows), cps, anchor);
    if matches!(verdict, TreeVerification::Intact { .. }) {
        return authority_violation(cps, anchor, rows).unwrap_or(verdict);
    }
    verdict
}

fn streamed(
    rows: &[AuditRow],
    cps: &[Checkpoint],
    anchor: Option<&Checkpoint>,
) -> TreeVerification {
    let mut verifier = PrefixVerifier::new(cps, anchor);
    for row in rows {
        verifier.push(row).unwrap();
    }
    verifier.finish().0
}

fn variant(v: &TreeVerification) -> &'static str {
    match v {
        TreeVerification::Intact { .. } => "intact",
        TreeVerification::Tampered { .. } => "tampered",
        TreeVerification::ChainBroken { .. } => "chain_broken",
        TreeVerification::AnchorMismatch { .. } => "anchor_mismatch",
        TreeVerification::MalformedPack { .. } => "malformed_pack",
        TreeVerification::SignatureInvalid { .. } => "signature_invalid",
        TreeVerification::UnauthorizedKey { .. } => "unauthorized_key",
        TreeVerification::SignatureRequired { .. } => "signature_required",
        TreeVerification::SigningKeyRequired { .. } => "signing_key_required",
    }
}

struct Case {
    name: &'static str,
    rows: Vec<AuditRow>,
    chain: Vec<Checkpoint>,
    anchor: Option<Checkpoint>,
    expect: &'static str,
}

fn cases() -> Vec<Case> {
    let k1 = crate::signing::generate_signing_key();
    let k2 = crate::signing::generate_signing_key();
    let stranger = crate::signing::generate_signing_key();
    let rows = history(&k1, &k2);
    let sizes = [0, 2, 5, ROWS];
    let base = chain(&rows, &sizes);
    // k1 is authorised from row 1 to row 4, k2 from row 4 on.
    let signed = {
        let mut c = base.clone();
        sign(&mut c[1], &k1, "k1");
        sign(&mut c[2], &k2, "k2");
        sign(&mut c[3], &k2, "k2");
        c
    };
    let mut cases = Vec::new();
    let mut case = |name, rows: Vec<AuditRow>, chain, anchor, expect| {
        cases.push(Case {
            name,
            rows,
            chain,
            anchor,
            expect,
        });
    };

    case("unsigned", rows.clone(), base.clone(), None, "intact");
    case(
        "signed by authorised keys",
        rows.clone(),
        signed.clone(),
        None,
        "intact",
    );
    case("empty chain", rows.clone(), Vec::new(), None, "intact");
    case(
        "rows beyond the last checkpoint",
        rows.clone(),
        chain(&rows, &[0, 2, 5]),
        None,
        "intact",
    );
    case(
        "anchor held",
        rows.clone(),
        signed.clone(),
        Some(signed[2].clone()),
        "intact",
    );
    case(
        "anchor at genesis",
        rows.clone(),
        base.clone(),
        Some(base[0].clone()),
        "intact",
    );

    let mut tampered = rows.clone();
    tampered[3].transformation_name = "forged".into();
    case(
        "an edited row",
        tampered.clone(),
        base.clone(),
        None,
        "tampered",
    );
    case(
        "an edited row outranks a later bad key",
        tampered.clone(),
        {
            let mut c = signed.clone();
            c[3].signatures.clear();
            sign(&mut c[3], &stranger, "k2");
            c
        },
        None,
        "tampered",
    );

    case(
        "a later edited row outranks an earlier bad key",
        {
            let mut r = rows.clone();
            r[6].transformation_name = "forged".into();
            r
        },
        {
            let mut c = base.clone();
            sign(&mut c[1], &stranger, "k9");
            c
        },
        None,
        "tampered",
    );
    case(
        "fewer rows than the last checkpoint",
        rows[..6].to_vec(),
        base.clone(),
        None,
        "tampered",
    );
    case(
        "no rows under a signed chain",
        Vec::new(),
        signed.clone(),
        None,
        "tampered",
    );
    case(
        "a missing row and a broken later link",
        rows[..3].to_vec(),
        {
            let mut c = base.clone();
            c[2].prev_checkpoint_hash = Some(c[0].checkpoint_hash);
            c
        },
        None,
        "chain_broken",
    );

    case(
        "a checkpoint whose contents do not hash to it",
        rows.clone(),
        {
            let mut c = base.clone();
            c[2].root_hash = c[1].root_hash;
            c
        },
        None,
        "chain_broken",
    );
    case(
        "a relinked checkpoint",
        rows.clone(),
        {
            let mut c = base.clone();
            c[2].prev_checkpoint_hash = None;
            rehash_from(&mut c[2..], 0);
            c
        },
        None,
        "chain_broken",
    );
    case(
        "a rewritten root, rehashed and relinked",
        rows.clone(),
        {
            let mut c = base.clone();
            c[2].root_hash = c[1].root_hash;
            rehash_from(&mut c, 2);
            c
        },
        None,
        "tampered",
    );

    case(
        "a signature that does not verify",
        rows.clone(),
        {
            let mut c = signed.clone();
            c[2].signatures[0].signature = c[1].signatures[0].signature.clone();
            c
        },
        None,
        "signature_invalid",
    );
    case(
        "a bad signature outranks a later edited row",
        {
            let mut r = rows.clone();
            r[6].transformation_name = "forged".into();
            r
        },
        {
            let mut c = signed.clone();
            c[2].signatures[0].signature = c[1].signatures[0].signature.clone();
            c
        },
        None,
        "signature_invalid",
    );

    case(
        "a key the ledger never authorised",
        rows.clone(),
        {
            let mut c = base.clone();
            sign(&mut c[2], &stranger, "k9");
            c
        },
        None,
        "unauthorized_key",
    );
    case(
        "a key before it was authorised",
        rows.clone(),
        {
            let mut c = base.clone();
            sign(&mut c[1], &k2, "k2");
            c
        },
        None,
        "unauthorized_key",
    );
    case(
        "a key after it was revoked",
        rows.clone(),
        {
            let mut c = base.clone();
            sign(&mut c[3], &k1, "k1");
            c
        },
        None,
        "unauthorized_key",
    );
    case(
        "a signed genesis checkpoint",
        rows.clone(),
        {
            let mut c = base.clone();
            sign(&mut c[0], &k1, "k1");
            c
        },
        None,
        "unauthorized_key",
    );
    case(
        "the second of two signatures unauthorised",
        rows.clone(),
        {
            let mut c = signed.clone();
            sign(&mut c[3], &k1, "k1");
            c
        },
        None,
        "unauthorized_key",
    );

    case(
        "an anchor that disagrees",
        rows.clone(),
        base.clone(),
        Some({
            let mut a = base.clone();
            a[2].root_hash = a[1].root_hash;
            rehash_from(&mut a, 2);
            a[2].clone()
        }),
        "anchor_mismatch",
    );
    case(
        "an anchor at a size the chain lacks",
        rows.clone(),
        base.clone(),
        Some(chain(&rows, &[3])[0].clone()),
        "anchor_mismatch",
    );
    case(
        "an anchor mismatch outranks an edited row",
        tampered.clone(),
        base.clone(),
        Some({
            let mut a = base.clone();
            a[1].root_hash = a[0].root_hash;
            rehash_from(&mut a, 1);
            a[1].clone()
        }),
        "anchor_mismatch",
    );
    case(
        "an anchor whose signature does not verify",
        rows.clone(),
        base.clone(),
        Some({
            let mut a = signed[2].clone();
            a.signatures[0].signature = signed[1].signatures[0].signature.clone();
            a
        }),
        "signature_invalid",
    );
    case(
        "an anchor signed by an unauthorised key",
        rows.clone(),
        base.clone(),
        Some({
            let mut a = base[2].clone();
            sign(&mut a, &stranger, "k9");
            a
        }),
        "unauthorized_key",
    );
    case(
        "a chain key outranks an anchor key",
        rows.clone(),
        {
            let mut c = base.clone();
            sign(&mut c[3], &k1, "k1");
            c
        },
        Some({
            let mut a = base[2].clone();
            sign(&mut a, &stranger, "k9");
            a
        }),
        "unauthorized_key",
    );

    cases
}

#[test]
fn the_streaming_verifier_gives_the_verdict_it_replaces() {
    for case in cases() {
        let anchor = case.anchor.as_ref();
        let old = old_verdict(&case.rows, &case.chain, anchor);
        let new = streamed(&case.rows, &case.chain, anchor);
        assert_eq!(new, old, "{}: the verdicts differ", case.name);
        assert_eq!(variant(&new), case.expect, "{}: {new:?}", case.name);
    }
}

/// Of two unauthorised keys, the chain's is reported before the anchor's,
/// whatever order they were met in.
#[test]
fn the_chain_key_is_named_before_the_anchor_key() {
    let case = cases()
        .into_iter()
        .find(|c| c.name == "a chain key outranks an anchor key")
        .unwrap();
    match streamed(&case.rows, &case.chain, case.anchor.as_ref()) {
        TreeVerification::UnauthorizedKey { key_id, .. } => assert_eq!(key_id, "k1"),
        other => panic!("expected the chain's key, got {other:?}"),
    }
}

/// The one deliberate difference from the old verifier: a chain whose sizes
/// go back down, every checkpoint otherwise genuine. The old verifier found
/// each root in its own prefix and called the tree intact; the streaming one
/// has already passed the smaller prefix and calls the chain broken. Neither
/// the database (sizes are unique and read in order) nor a pack (validated
/// strictly increasing) can hand either verifier such a chain.
#[test]
fn a_chain_whose_sizes_go_back_down_is_broken_where_it_was_intact() {
    let k1 = crate::signing::generate_signing_key();
    let k2 = crate::signing::generate_signing_key();
    let rows = history(&k1, &k2);
    let down = chain(&rows, &[0, 5, 2]);
    assert!(matches!(
        old_verdict(&rows, &down, None),
        TreeVerification::Intact { .. }
    ));
    match streamed(&rows, &down, None) {
        TreeVerification::ChainBroken { detail } => assert!(
            detail.contains("tree_size 2 follows one at tree_size 5"),
            "{detail}"
        ),
        other => panic!("expected ChainBroken, got {other:?}"),
    }
}
