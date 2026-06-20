//! `morpholog checkpoint` - record a tamper-evident checkpoint over the
//! audit log and print it as an external anchor.

use anyhow::Context;
use morpholog_postgres::create_checkpoint;

use crate::DatabaseArgs;
use crate::commands::{connect, print_json};

/// Run `checkpoint`: compute the audit Merkle root over the committed
/// prefix, chain it onto the previous checkpoint, and print the
/// checkpoint as JSON. Save that output outside the database - a later
/// `verify --anchor-file` against it is the check a coordinated rewrite
/// of the audit log and the checkpoint table cannot pass.
pub(crate) async fn run(args: DatabaseArgs) -> anyhow::Result<()> {
    let pool = connect(&args.database_url).await?;
    let outcome = create_checkpoint(&pool)
        .await
        .context("create_checkpoint failed")?;
    print_json(&outcome)?;
    Ok(())
}
