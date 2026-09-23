//! `morpholog audit verify` - replay the audit log against the claims table,
//! and verify the audit Merkle tree against its checkpoints.

use anyhow::Context;
use morpholog_postgres::{
    SignaturePolicy, TreeVerification, VerifyOutcome, VerifyReport, ViewsVerification,
    WitnessAnchors, WitnessesReport, parse_public_key, render_public_key,
    verify_audit_tree_with_chain, verify_replay, verify_views, witnesses_report,
};

use crate::VerifyArgs;
use crate::commands::{AlreadyReported, connect, print_json, read_anchor};

/// Run `audit verify`: replay the audit log against the claims table, then
/// recompute the audit Merkle root against each checkpoint and any given
/// anchor. Prints both verdicts as one JSON object; exits 1 if either fails.
pub(crate) async fn run(args: VerifyArgs) -> anyhow::Result<()> {
    let pool = connect(&args.db.database_url).await?;

    let replay = verify_replay(&pool).await.context("verify_replay failed")?;

    let anchor = read_anchor(args.anchor_file.as_deref())?;
    // The signature policy is checked in the same snapshot, over the chain
    // just proved.
    let policy = signature_policy(
        args.require_signatures,
        args.require_signatures_from,
        args.require_signing_key.as_deref(),
    )?;
    let (tree, chain, role_rebindings) =
        verify_audit_tree_with_chain(&pool, anchor, policy.as_ref())
            .await
            .context("verify_audit_tree failed")?;
    // Witnesses are judged on the same chain but apart from the tree
    // verdict: a witness dates a head whether or not the log still matches
    // it.
    let anchors = witness_anchors(args.trusted_tsa_file.as_deref())?;
    let witnesses = witnesses_report(&chain, anchors.as_ref());

    // Opt-in: only a deployment that generated views has any to verify.
    let views = match &args.views_schema {
        Some(schema) => Some(
            verify_views(&pool, schema)
                .await
                .context("verify_views failed")?,
        ),
        None => None,
    };

    // A rebinding is a finding, never a failure: it does not affect the
    // exit code.
    let report = VerifyReport {
        replay,
        tree,
        views,
        witnesses,
        role_rebindings,
    };
    print_json(&report)?;

    let diverged = matches!(report.replay, VerifyOutcome::Divergent { .. });
    let tampered = !matches!(report.tree, TreeVerification::Intact { .. });
    // NotSealed is visible in the JSON but not a failure: an unsealed
    // surface has nothing to contradict.
    let surface_tampered = matches!(report.views, Some(ViewsVerification::Tampered { .. }));
    // Only an `invalid` witness fails; the other standings say what could
    // and could not be established.
    let witness_invalid = report
        .witnesses
        .as_ref()
        .is_some_and(WitnessesReport::any_invalid);
    if diverged || tampered || surface_tampered || witness_invalid {
        return Err(AlreadyReported.into());
    }
    Ok(())
}

/// The timestamp-authority trust anchors the verifier named, or none.
pub(crate) fn witness_anchors(
    pem_file: Option<&std::path::Path>,
) -> anyhow::Result<Option<WitnessAnchors>> {
    match pem_file {
        Some(path) => {
            let pem = std::fs::read(path)
                .with_context(|| format!("reading the trusted TSA file {}", path.display()))?;
            let anchors = WitnessAnchors::from_pem(&pem)
                .with_context(|| format!("{} is not a PEM file of certificates", path.display()))?;
            Ok(Some(anchors))
        }
        None => Ok(None),
    }
}

/// The signature policy the flags spell, or none. Each flag alone requires
/// signatures: `--require-signatures` from zero, `--require-signatures-from
/// N` from N, and a pinned key from zero unless a threshold is also given.
/// The key file is re-rendered to canonical `ed25519-pub:<hex>`, so stray
/// whitespace does not matter.
pub(crate) fn signature_policy(
    require: bool,
    from: Option<i64>,
    key_file: Option<&std::path::Path>,
) -> anyhow::Result<Option<SignaturePolicy>> {
    let required_public_key = match key_file {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading the pinned signing key {}", path.display()))?;
            let key = parse_public_key(text.trim())
                .with_context(|| format!("{} is not an ed25519-pub:<hex> key", path.display()))?;
            Some(render_public_key(&key))
        }
        None => None,
    };
    if !require && from.is_none() && required_public_key.is_none() {
        return Ok(None);
    }
    Ok(Some(SignaturePolicy {
        from_tree_size: from.unwrap_or(0),
        required_public_key,
    }))
}
