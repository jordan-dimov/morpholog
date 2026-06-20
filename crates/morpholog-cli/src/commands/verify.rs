//! `morpholog verify` - replay the audit log against the claims table,
//! and verify the audit Merkle tree against its checkpoints.

use anyhow::Context;
use morpholog_postgres::{
    Checkpoint, TreeVerification, VerifyOutcome, verify_audit_tree, verify_replay,
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
    let tree = verify_audit_tree(&pool, anchor)
        .await
        .context("verify_audit_tree failed")?;

    print_json(&serde_json::json!({ "replay": &replay, "tree": &tree }))?;

    let diverged = matches!(replay, VerifyOutcome::Divergent { .. });
    let tampered = !matches!(tree, TreeVerification::Intact { .. });
    if diverged || tampered {
        std::process::exit(1);
    }
    Ok(())
}
