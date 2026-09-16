//! `morpholog audit checkpoint` - record a tamper-evident checkpoint over the
//! audit log and print it as an external anchor.

use anyhow::Context;
use morpholog_postgres::{
    CheckpointOutcome, CheckpointSigner, attach_witness, create_checkpoint, signing_key_from_pem,
};

use crate::CheckpointArgs;
use crate::commands::witness::obtain;
use crate::commands::{AlreadyReported, connect, print_json};

/// Run `audit checkpoint`: compute the audit Merkle root over the committed
/// prefix, chain it onto the previous checkpoint, optionally sign the new
/// tree head, and print the checkpoint as JSON. Save that output outside
/// the database - a later `verify --anchor-file` against it is the check a
/// coordinated rewrite of the audit log and the checkpoint table cannot
/// pass; a signature makes the anchor attributable as well, and an
/// outside witness (`--witness`) dates it. Witnessing happens after the
/// commit, outside any transaction: the checkpoint is recorded and
/// printed whatever the authority does, and a failed submission exits
/// one naming the retry.
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

    let mut failed = None;
    match &mut outcome {
        CheckpointOutcome::Created(checkpoint) => {
            for target in &args.witness {
                match obtain(target, checkpoint).await {
                    Ok(witness) => {
                        *checkpoint = attach_witness(
                            &pool,
                            checkpoint.tree_size,
                            &checkpoint.checkpoint_hash,
                            witness,
                        )
                        .await
                        .context("attach_witness failed")?;
                    }
                    Err(e) => {
                        failed = Some((checkpoint.tree_size, e));
                        break;
                    }
                }
            }
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
    if let Some((tree_size, e)) = failed {
        eprintln!(
            "error: {e:#}\nThe checkpoint is recorded and printed above; retry with \
             `audit witness --tree-size {tree_size} --witness ...`."
        );
        return Err(AlreadyReported.into());
    }
    Ok(())
}
