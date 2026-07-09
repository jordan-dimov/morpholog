//! `morpholog verify` - replay the audit log against the claims table,
//! and verify the audit Merkle tree against its checkpoints.

use anyhow::Context;
use morpholog_postgres::{
    Checkpoint, TreeVerification, VerifyOutcome, VerifyReport, ViewsVerification,
    first_unsigned_checkpoint_size, verify_audit_tree, verify_replay, verify_views,
};

use crate::VerifyArgs;
use crate::commands::{connect, print_json};

/// Run `verify`: replay (claims vs audit), then the tamper-evidence
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
    let mut tree = verify_audit_tree(&pool, anchor)
        .await
        .context("verify_audit_tree failed")?;

    // Compliance policy: with --require-signatures, an otherwise-intact
    // tree that has an unsigned checkpoint fails. Signing is opt-in, so
    // this is the verifier's choice, applied over the intrinsic verdict.
    if args.require_signatures
        && matches!(tree, TreeVerification::Intact { .. })
        && let Some(tree_size) = first_unsigned_checkpoint_size(&pool)
            .await
            .context("checking for unsigned checkpoints")?
    {
        tree = TreeVerification::SignatureRequired { tree_size };
    }

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
    };
    print_json(&report)?;

    let diverged = matches!(report.replay, VerifyOutcome::Divergent { .. });
    let tampered = !matches!(report.tree, TreeVerification::Intact { .. });
    // NotSealed is visible in the JSON but not a failure: an unsealed
    // surface has nothing to contradict.
    let surface_tampered = matches!(report.views, Some(ViewsVerification::Tampered { .. }));
    if diverged || tampered || surface_tampered {
        std::process::exit(1);
    }
    Ok(())
}
