//! `morpholog checkpoint` - record a tamper-evident checkpoint over the
//! audit log and print it as an external anchor.

use anyhow::Context;
use morpholog_postgres::{CheckpointSigner, create_checkpoint, signing_key_from_pem};

use crate::CheckpointArgs;
use crate::commands::{connect, print_json};

/// Run `checkpoint`: compute the audit Merkle root over the committed
/// prefix, chain it onto the previous checkpoint, optionally sign the new
/// tree head, and print the checkpoint as JSON. Save that output outside
/// the database - a later `verify --anchor-file` against it is the check a
/// coordinated rewrite of the audit log and the checkpoint table cannot
/// pass; a signature makes the anchor attributable as well.
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
    let writers = (!args.writer_role.is_empty()).then_some(args.writer_role.as_slice());
    let outcome = create_checkpoint(&pool, signer.as_ref(), writers)
        .await
        .context("create_checkpoint failed")?;
    print_json(&outcome)?;
    Ok(())
}
