//! `morpholog evidence` - export a portable evidence pack over the audit
//! log (a complete prefix, or a window between two checkpoints), and verify
//! one offline.
//!
//! `export` needs the database; `verify` deliberately does not - a third
//! party checks a pack with zero database access, which is the whole
//! product promise.

use morpholog_postgres::{
    Checkpoint, EvidencePack, TreeVerification, WindowEvidencePack, WindowStart,
    WindowVerification, export_pack, export_window, verify_pack, verify_window,
};

use anyhow::Context;

use crate::commands::{connect, print_json};
use crate::{EvidenceCmd, EvidenceExportArgs, EvidenceVerifyArgs};

pub(crate) async fn run(cmd: EvidenceCmd) -> anyhow::Result<()> {
    match cmd {
        EvidenceCmd::Export(args) => export(args).await,
        EvidenceCmd::Verify(args) => verify(args),
    }
}

/// `evidence export`: a complete-prefix pack by default, or - with a
/// `--from-*` start - the window between that earlier checkpoint and the
/// covering one. Printed as JSON; redirect it to a file. A pack carries the
/// full audit rows it covers - actors, arguments, claims, intents - so it is
/// NOT selective disclosure and may contain confidential business data.
async fn export(args: EvidenceExportArgs) -> anyhow::Result<()> {
    let pool = connect(&args.db.database_url).await?;

    // The window start: a whole anchor file (the trust object - export
    // refuses if the stored start has diverged from it), or the weaker
    // tree-size convenience. Either turns this into a window export.
    let start = match (&args.from_anchor, args.from_tree_size) {
        (Some(path), _) => {
            let bytes = std::fs::read(path)
                .with_context(|| format!("reading anchor file {}", path.display()))?;
            let anchor: Checkpoint = serde_json::from_slice(&bytes).with_context(|| {
                format!("parsing anchor file {} as a checkpoint", path.display())
            })?;
            Some(WindowStart::Anchor(anchor))
        }
        (None, Some(n)) => Some(WindowStart::TreeSize(n)),
        (None, None) => None,
    };

    match start {
        Some(start) => {
            let pack = export_window(&pool, start, args.tree_size)
                .await
                .context("export_window failed")?;
            print_json(&pack)?;
        }
        None => {
            let pack = export_pack(&pool, args.tree_size)
                .await
                .context("export_pack failed")?;
            print_json(&pack)?;
        }
    }
    Ok(())
}

/// `evidence verify`: check a pack offline, with no database. A prefix pack
/// recomputes its root from every row; a window pack checks a consistency
/// proof plus per-row inclusion proofs. The pack's `pack_format_version`
/// selects which. One JSON verdict on stdout; exit one on any tamper,
/// divergence, or malformed pack - the same data-on-stdout,
/// exit-code-as-verdict shape as `verify`.
fn verify(args: EvidenceVerifyArgs) -> anyhow::Result<()> {
    let bytes = std::fs::read(&args.pack_file)
        .with_context(|| format!("reading pack file {}", args.pack_file.display()))?;

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

    // The pack kind is part of the contract: peek the format version so a
    // window pack is not mistaken for a malformed prefix pack. A file that is
    // not a pack at all is still a decided verdict, not an operational error.
    let intact = match pack_format_version(&bytes) {
        Some(2) => {
            let verdict = verify_window_pack(&bytes, anchor.as_ref(), args.require_signatures);
            let intact = matches!(verdict, WindowVerification::Intact { .. });
            print_json(&verdict)?;
            intact
        }
        _ => {
            let verdict = verify_prefix_pack(&bytes, anchor.as_ref(), args.require_signatures);
            let intact = matches!(verdict, TreeVerification::Intact { .. });
            print_json(&verdict)?;
            intact
        }
    };

    if !intact {
        std::process::exit(1);
    }
    Ok(())
}

/// The `manifest.pack_format_version`, if the bytes parse as JSON with that
/// field - the cheap discriminator between a v1 prefix pack and a v2 window
/// pack, before committing to a typed deserialization.
fn pack_format_version(bytes: &[u8]) -> Option<u64> {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()?
        .get("manifest")?
        .get("pack_format_version")?
        .as_u64()
}

fn verify_prefix_pack(
    bytes: &[u8],
    anchor: Option<&Checkpoint>,
    require_signatures: bool,
) -> TreeVerification {
    let pack: EvidencePack = match serde_json::from_slice(bytes) {
        Ok(pack) => pack,
        Err(e) => {
            return TreeVerification::MalformedPack {
                detail: e.to_string(),
            };
        }
    };
    let verdict = verify_pack(&pack, anchor).unwrap_or_else(|e| TreeVerification::MalformedPack {
        detail: e.to_string(),
    });
    // Compliance policy, offline from the pack's own checkpoints: with
    // --require-signatures an unsigned checkpoint fails.
    if require_signatures
        && matches!(verdict, TreeVerification::Intact { .. })
        && let Some(tree_size) = pack
            .checkpoints
            .iter()
            .filter(|c| c.signatures.is_empty())
            .map(|c| c.tree_size)
            .min()
    {
        return TreeVerification::SignatureRequired { tree_size };
    }
    verdict
}

fn verify_window_pack(
    bytes: &[u8],
    anchor: Option<&Checkpoint>,
    require_signatures: bool,
) -> WindowVerification {
    let pack: WindowEvidencePack = match serde_json::from_slice(bytes) {
        Ok(pack) => pack,
        Err(e) => {
            return WindowVerification::Malformed {
                detail: e.to_string(),
            };
        }
    };
    let verdict = verify_window(&pack, anchor).unwrap_or_else(|e| WindowVerification::Malformed {
        detail: e.to_string(),
    });
    // Compliance policy: REMIT attribution wants a signed window end, so
    // --require-signatures fails an unsigned to-checkpoint.
    if require_signatures
        && matches!(verdict, WindowVerification::Intact { .. })
        && pack.to_checkpoint.signatures.is_empty()
    {
        return WindowVerification::SignatureRequired {
            tree_size: pack.to_checkpoint.tree_size,
        };
    }
    verdict
}
