//! `morpholog audit checkpoint` - record a tamper-evident checkpoint over the
//! audit log and print it as an external anchor.

use anyhow::Context;
use morpholog_postgres::{
    CheckpointOutcome, CheckpointSigner, create_checkpoint, signing_key_from_pem,
};

use crate::CheckpointArgs;
use crate::commands::witness::{report_failures, witness_all};
use crate::commands::{AlreadyReported, connect, print_json};

/// Run `audit checkpoint`: compute the audit Merkle root, chain it onto the
/// previous checkpoint, optionally sign it, and print it as JSON.
///
/// Keep the output outside the database. A later `verify --anchor-file`
/// against it catches a rewrite of both the audit log and the checkpoint
/// table. A signature says who made the anchor; a `--witness` dates it.
/// Witnessing runs after the commit, outside any transaction: every named
/// authority is tried, the checkpoint is kept and printed regardless, and
/// any failed submission exits 1 naming the retry.
pub(crate) async fn run(args: CheckpointArgs) -> anyhow::Result<()> {
    let signer = match (&args.signing_key, &args.key_id) {
        (Some(path), Some(key_id)) => {
            let pem = std::fs::read_to_string(path)
                .with_context(|| format!("reading signing key {}", path.display()))?;
            let key = signing_key_from_pem(&pem)
                .with_context(|| format!("parsing signing key {}", path.display()))?;
            Some(CheckpointSigner {
                key_id: key_id.clone(),
                key,
            })
        }
        // clap's `requires` keeps the two flags together, so the mixed
        // cases never reach here.
        _ => None,
    };

    let pool = connect(&args.db.database_url).await?;
    let mut outcome = create_checkpoint(&pool, signer.as_ref(), args.writers.as_writers())
        .await
        .context("create_checkpoint failed")?;

    let mut failed = Vec::new();
    match &mut outcome {
        CheckpointOutcome::Created(checkpoint) => {
            let (witnessed, failures) =
                witness_all(&pool, checkpoint.clone(), &args.witness).await?;
            *checkpoint = witnessed;
            failed = failures;
        }
        CheckpointOutcome::NoNewRows(checkpoint) if !args.witness.is_empty() => {
            eprintln!(
                "note: no new rows, so nothing was submitted; to witness the current head \
                 run `audit witness --tree-size {}`",
                checkpoint.tree_size
            );
        }
        CheckpointOutcome::NoNewRows(_) => {}
    }
    print_json(&outcome)?;
    let tree_size = match &outcome {
        CheckpointOutcome::Created(c) | CheckpointOutcome::NoNewRows(c) => c.tree_size,
    };
    if report_failures(tree_size, &failed) {
        return Err(AlreadyReported.into());
    }
    Ok(())
}
