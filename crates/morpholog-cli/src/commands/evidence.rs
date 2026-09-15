//! `morpholog audit export` / `verify-pack` - export a portable evidence pack over the audit
//! log (a complete prefix, or a window between two checkpoints), and verify
//! one offline.
//!
//! `export` needs the database; `verify` deliberately does not - a third
//! party checks a pack with zero database access, which is the whole
//! product promise.

use morpholog_postgres::{
    Checkpoint, EvidencePack, SelectiveEvidencePack, SelectiveVerification, SignaturePolicy,
    TreeVerification, WindowEvidencePack, WindowStart, WindowVerification, export_pack,
    export_selective, export_window, verify_pack, verify_selective, verify_window,
};

use anyhow::Context;

use crate::commands::verify::signature_policy;
use crate::commands::{AlreadyReported, connect, print_json};
use crate::{EvidenceExportArgs, EvidenceVerifyArgs};

/// `audit export`: a complete-prefix pack by default, a window between
/// two checkpoints with a `--from-*` start, or - with `--transition` - a
/// selective pack disclosing only the named transitions, each proven
/// included. Printed as JSON; redirect it to a file. Prefix and window
/// packs carry the FULL audit rows they cover - actors, arguments, claims,
/// intents - and may contain confidential business data; a selective pack
/// carries only the chosen rows, and proves them authentic without
/// proving the selection complete.
pub(crate) async fn export(args: EvidenceExportArgs) -> anyhow::Result<()> {
    let pool = connect(&args.db.database_url).await?;

    if !args.transition.is_empty() {
        let pack = export_selective(&pool, args.tree_size, &args.transition)
            .await
            .context("export_selective failed")?;
        return print_json(&pack);
    }

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

/// `audit verify-pack`: check a pack offline, with no database. A prefix pack
/// recomputes its root from every row; a window pack checks a consistency
/// proof plus per-row inclusion proofs. The pack's `pack_format_version`
/// selects which. One JSON verdict on stdout; exit one on any tamper,
/// divergence, or malformed pack - the same data-on-stdout,
/// exit-code-as-verdict shape as `verify`.
pub(crate) fn verify(args: EvidenceVerifyArgs) -> anyhow::Result<()> {
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

    // The pack kind is part of the contract: peek the format version so
    // each pack kind gets its own verifier and verdict shape. An unknown
    // FUTURE version is named as such rather than falling through to the
    // prefix path and reading as a malformed v1; a file that is not a pack
    // at all is still a decided verdict, not an operational error.
    let policy = signature_policy(
        args.require_signatures,
        args.require_signatures_from,
        args.require_signing_key.as_deref(),
    )?;
    let version = pack_format_version(&bytes);
    // A sparse pack checks signatures cryptographically only; it cannot
    // say whether a key was authorised as of its prefix. A pin is an
    // intersection with that authority, so here it would silently become
    // "signed by this key" - a different claim. Refused, remedy named.
    if matches!(version, Some(2 | 3))
        && policy
            .as_ref()
            .is_some_and(|p| p.required_public_key.is_some())
    {
        anyhow::bail!(
            "--require-signing-key needs a complete-prefix pack: a window or selective pack \
             cannot establish signing-key authority, so the pin could only be checked \
             cryptographically. Verify a full-prefix pack, or the live log with `audit verify`."
        );
    }
    let intact = match version {
        Some(2) => {
            let verdict = verify_window_pack(&bytes, anchor.as_ref(), policy.as_ref());
            let intact = matches!(verdict, WindowVerification::Intact { .. });
            print_json(&verdict)?;
            intact
        }
        Some(3) => {
            let verdict = verify_selective_pack(&bytes, anchor.as_ref(), policy.as_ref());
            let intact = matches!(verdict, SelectiveVerification::Intact { .. });
            print_json(&verdict)?;
            intact
        }
        Some(n) if n > 3 => {
            print_json(&TreeVerification::MalformedPack {
                detail: format!(
                    "pack_format_version {n} is newer than this binary understands; \
                     upgrade morpholog to verify it"
                ),
            })?;
            false
        }
        _ => {
            let verdict = verify_prefix_pack(&bytes, anchor.as_ref(), policy.as_ref());
            let intact = matches!(verdict, TreeVerification::Intact { .. });
            print_json(&verdict)?;
            intact
        }
    };

    if !intact {
        return Err(AlreadyReported.into());
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
    policy: Option<&SignaturePolicy>,
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
    // Verifier policy, offline from the pack's own checkpoints, over an
    // intact tree only - every signature it inspects is already proven
    // genuine and authorised by the pack's full prefix.
    if let Some(policy) = policy
        && matches!(verdict, TreeVerification::Intact { .. })
        && let Some(violation) = policy.violation(&pack.checkpoints)
    {
        return violation.into();
    }
    verdict
}

fn verify_selective_pack(
    bytes: &[u8],
    anchor: Option<&Checkpoint>,
    policy: Option<&SignaturePolicy>,
) -> SelectiveVerification {
    let pack: SelectiveEvidencePack = match serde_json::from_slice(bytes) {
        Ok(pack) => pack,
        Err(e) => {
            return SelectiveVerification::Malformed {
                detail: e.to_string(),
            };
        }
    };
    let verdict =
        verify_selective(&pack, anchor).unwrap_or_else(|e| SelectiveVerification::Malformed {
            detail: e.to_string(),
        });
    // Policy over the one covering checkpoint: presence only, since a
    // sparse pack cannot judge authority and so never carries a pin.
    if let Some(policy) = policy
        && matches!(verdict, SelectiveVerification::Intact { .. })
        && let Some(tree_size) = policy.unsigned_at_or_after([&pack.checkpoint])
    {
        return SelectiveVerification::SignatureRequired { tree_size };
    }
    verdict
}

fn verify_window_pack(
    bytes: &[u8],
    anchor: Option<&Checkpoint>,
    policy: Option<&SignaturePolicy>,
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
    // Policy over the window's end only (REMIT attribution wants a signed
    // window end; the trusted start is the anchor's business): presence
    // only, since a sparse pack cannot judge authority and never carries
    // a pin.
    if let Some(policy) = policy
        && matches!(verdict, WindowVerification::Intact { .. })
        && let Some(tree_size) = policy.unsigned_at_or_after([&pack.to_checkpoint])
    {
        return WindowVerification::SignatureRequired { tree_size };
    }
    verdict
}
