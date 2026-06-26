//! `morpholog evidence` - export a portable evidence pack over a
//! checkpointed prefix of the audit log, and verify one offline.
//!
//! `export` needs the database; `verify` deliberately does not - a third
//! party checks a pack with zero database access, which is the whole
//! product promise.

use morpholog_postgres::{Checkpoint, EvidencePack, TreeVerification, export_pack, verify_pack};

use anyhow::Context;

use crate::commands::{connect, print_json};
use crate::{EvidenceCmd, EvidenceExportArgs, EvidenceVerifyArgs};

pub(crate) async fn run(cmd: EvidenceCmd) -> anyhow::Result<()> {
    match cmd {
        EvidenceCmd::Export(args) => export(args).await,
        EvidenceCmd::Verify(args) => verify(args),
    }
}

/// `evidence export`: assemble a complete-prefix pack covering a
/// checkpoint (the latest, or the one at `--tree-size N`) and print it as
/// JSON. Redirect it to a file. The pack carries the full audit prefix -
/// actors, arguments, claims, intents - so it is NOT selective disclosure
/// and may contain confidential business data.
async fn export(args: EvidenceExportArgs) -> anyhow::Result<()> {
    let pool = connect(&args.db.database_url).await?;
    let pack = export_pack(&pool, args.tree_size)
        .await
        .context("export_pack failed")?;
    print_json(&pack)?;
    Ok(())
}

/// `evidence verify`: check a pack offline, with no database. Recomputes
/// the Merkle root from the pack's rows and checks it against the pack's
/// checkpoints, and against an external anchor if one is supplied. One
/// JSON verdict on stdout; exit one on any tamper, divergence, or
/// malformed pack - the same data-on-stdout, exit-code-as-verdict shape
/// as `verify`.
fn verify(args: EvidenceVerifyArgs) -> anyhow::Result<()> {
    let bytes = std::fs::read(&args.pack_file)
        .with_context(|| format!("reading pack file {}", args.pack_file.display()))?;
    let pack: EvidencePack = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "parsing pack file {} as an evidence pack",
            args.pack_file.display()
        )
    })?;

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

    let tree =
        verify_pack(&pack, anchor.as_ref()).unwrap_or_else(|e| TreeVerification::MalformedPack {
            detail: e.to_string(),
        });
    print_json(&tree)?;
    if !matches!(tree, TreeVerification::Intact { .. }) {
        std::process::exit(1);
    }
    Ok(())
}
