//! `morpholog audit verify` - replay the audit log against the claims table,
//! and verify the audit Merkle tree against its checkpoints.

use anyhow::Context;
use morpholog_postgres::{
    Checkpoint, SignaturePolicy, TreeVerification, VerifyOutcome, VerifyReport, ViewsVerification,
    WitnessAnchors, WitnessesReport, parse_public_key, render_public_key,
    verify_audit_tree_with_chain, verify_replay, verify_views, witnesses_report,
};

use crate::VerifyArgs;
use crate::commands::{AlreadyReported, connect, print_json};

/// Run `audit verify`: replay (claims vs audit), then the tamper-evidence
/// check (recompute the audit Merkle root against each checkpoint, and
/// against an external anchor if given). One JSON object on stdout
/// carrying both verdicts; exit one if either fails - the same
/// data-on-stdout, exit-code-as-verdict shape as `propose`.
pub(crate) async fn run(args: VerifyArgs) -> anyhow::Result<()> {
    let pool = connect(&args.db.database_url).await?;

    let replay = verify_replay(&pool).await.context("verify_replay failed")?;

    let anchor: Option<Checkpoint> = match &args.anchor_file {
        Some(path) => {
            let bytes = std::fs::read(path)
                .with_context(|| format!("reading anchor file {}", path.display()))?;
            Some(serde_json::from_slice(&bytes).with_context(|| {
                format!("parsing anchor file {} as a checkpoint", path.display())
            })?)
        }
        None => None,
    };
    // The verifier's signature policy rides inside the same snapshot the
    // intrinsic verdict is computed from, over the chain it just proved.
    let policy = signature_policy(
        args.require_signatures,
        args.require_signatures_from,
        args.require_signing_key.as_deref(),
    )?;
    let (tree, chain) = verify_audit_tree_with_chain(&pool, anchor, policy.as_ref())
        .await
        .context("verify_audit_tree failed")?;
    // The witness axis reads the same chain the tree verdict saw, and is
    // independent of it: a witness vouches for a head's existence at a
    // time whether or not the log still recomputes to it.
    let anchors = witness_anchors(args.trusted_tsa_file.as_deref())?;
    let witnesses = witnesses_report(&chain, anchors.as_ref());

    // The views leg is opt-in: only a deployment that generated a view
    // surface has one to verify.
    let views = match &args.views_schema {
        Some(schema) => Some(
            verify_views(&pool, schema)
                .await
                .context("verify_views failed")?,
        ),
        None => None,
    };

    let report = VerifyReport {
        replay,
        tree,
        views,
        witnesses,
    };
    print_json(&report)?;

    let diverged = matches!(report.replay, VerifyOutcome::Divergent { .. });
    let tampered = !matches!(report.tree, TreeVerification::Intact { .. });
    // NotSealed is visible in the JSON but not a failure: an unsealed
    // surface has nothing to contradict.
    let surface_tampered = matches!(report.views, Some(ViewsVerification::Tampered { .. }));
    // Of the witness standings only `invalid` is a judgement; the rest
    // describe what the verifier could and could not establish.
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

/// The signature policy the flags spell, or none. Each flag alone
/// requires signatures: `--require-signatures` from zero,
/// `--require-signatures-from N` from N, and a pinned key from zero
/// unless a threshold is given too. The key file is parsed and
/// re-rendered so the pin is the canonical `ed25519-pub:<hex>` form,
/// whatever whitespace the file carries.
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
