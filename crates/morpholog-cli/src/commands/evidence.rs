//! `morpholog audit export` / `verify-pack` - export a portable evidence pack over the audit
//! log (a complete prefix, or a window between two checkpoints), and verify
//! one offline.
//!
//! `export` needs the database; `verify` deliberately does not - a third
//! party checks a pack with zero database access, which is the whole
//! product promise.

use morpholog_postgres::{
    Checkpoint, EvidencePack, PackVerdict, PackVerificationReport, SelectiveEvidencePack,
    SelectiveVerification, SignaturePolicy, TreeVerification, WindowEvidencePack, WindowStart,
    WindowVerification, WitnessesReport, export_pack, export_selective, export_window, verify_pack,
    verify_selective, verify_window, with_anchor_signatures, witnesses_report,
};

use anyhow::Context;

use crate::commands::verify::{signature_policy, witness_anchors};
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
    let verdict = match pack_format_version(&bytes) {
        Some(2) => match verify_window_pack(&bytes, anchor.as_ref(), policy.as_ref()) {
            Offline::Verdict(verdict) => PackVerdict::Window(verdict),
            Offline::PinNeedsFullPrefix => return Err(pin_needs_full_prefix()),
        },
        Some(3) => match verify_selective_pack(&bytes, anchor.as_ref(), policy.as_ref()) {
            Offline::Verdict(verdict) => PackVerdict::Selective(verdict),
            Offline::PinNeedsFullPrefix => return Err(pin_needs_full_prefix()),
        },
        Some(n) if n > 3 => PackVerdict::Prefix(TreeVerification::MalformedPack {
            detail: format!(
                "pack_format_version {n} is newer than this binary understands; \
                 upgrade morpholog to verify it"
            ),
        }),
        _ => PackVerdict::Prefix(verify_prefix_pack(&bytes, anchor.as_ref(), policy.as_ref())),
    };
    let intact = matches!(
        verdict,
        PackVerdict::Prefix(TreeVerification::Intact { .. })
            | PackVerdict::Window(WindowVerification::Intact { .. })
            | PackVerdict::Selective(SelectiveVerification::Intact { .. })
    );

    // The witness axis is judged over the pack's own checkpoints and is
    // independent of the verdict; it changes the output shape, so it is
    // emitted only when asked for.
    let mut witness_invalid = false;
    if args.witnesses || args.trusted_tsa_file.is_some() {
        let anchors = witness_anchors(args.trusted_tsa_file.as_deref())?;
        let witnesses = witnesses_report(&pack_checkpoints(&bytes), anchors.as_ref());
        witness_invalid = witnesses.as_ref().is_some_and(WitnessesReport::any_invalid);
        print_json(&PackVerificationReport { verdict, witnesses })?;
    } else {
        print_json(&verdict)?;
    }

    if !intact || witness_invalid {
        return Err(AlreadyReported.into());
    }
    Ok(())
}

/// Every checkpoint a pack carries, whatever its kind; none for bytes
/// that are not a pack this binary understands (the verdict already says
/// so).
fn pack_checkpoints(bytes: &[u8]) -> Vec<Checkpoint> {
    match pack_format_version(bytes) {
        Some(2) => serde_json::from_slice::<WindowEvidencePack>(bytes)
            .map(|p| vec![p.from_checkpoint, p.to_checkpoint])
            .unwrap_or_default(),
        Some(3) => serde_json::from_slice::<SelectiveEvidencePack>(bytes)
            .map(|p| vec![p.checkpoint])
            .unwrap_or_default(),
        Some(n) if n > 3 => Vec::new(),
        _ => serde_json::from_slice::<EvidencePack>(bytes)
            .map(|p| p.checkpoints)
            .unwrap_or_default(),
    }
}

/// What an offline verifier answers: the pack's verdict, or - only once
/// the pack has proven intact - that the policy asked of it cannot be
/// judged here. A broken pack reports as broken whatever the policy.
enum Offline<V> {
    Verdict(V),
    PinNeedsFullPrefix,
}

/// A sparse pack checks signatures cryptographically only; it cannot say
/// whether a key was authorised as of its prefix. A pin is an
/// intersection with that authority, so here it would silently become
/// "signed by this key" - a different claim. Refused, remedy named.
fn pin_needs_full_prefix() -> anyhow::Error {
    anyhow::anyhow!(
        "--require-signing-key needs a complete-prefix pack: a window or selective pack \
         cannot establish signing-key authority, so the pin could only be checked \
         cryptographically. Verify a full-prefix pack, or the live log with `audit verify`."
    )
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
        && let Some(violation) =
            policy.violation(&with_anchor_signatures(&pack.checkpoints, anchor))
    {
        return violation.into();
    }
    verdict
}

fn verify_selective_pack(
    bytes: &[u8],
    anchor: Option<&Checkpoint>,
    policy: Option<&SignaturePolicy>,
) -> Offline<SelectiveVerification> {
    let pack: SelectiveEvidencePack = match serde_json::from_slice(bytes) {
        Ok(pack) => pack,
        Err(e) => {
            return Offline::Verdict(SelectiveVerification::Malformed {
                detail: e.to_string(),
            });
        }
    };
    let verdict =
        verify_selective(&pack, anchor).unwrap_or_else(|e| SelectiveVerification::Malformed {
            detail: e.to_string(),
        });
    if !matches!(verdict, SelectiveVerification::Intact { .. }) {
        return Offline::Verdict(verdict);
    }
    // Policy over the one covering checkpoint, and presence only: a
    // sparse pack cannot judge authority, so a pin cannot be an
    // intersection with it here.
    if let Some(policy) = policy {
        if policy.required_public_key.is_some() {
            return Offline::PinNeedsFullPrefix;
        }
        let held = with_anchor_signatures(std::slice::from_ref(&pack.checkpoint), anchor);
        if let Some(tree_size) = policy.unsigned_at_or_after(&held) {
            return Offline::Verdict(SelectiveVerification::SignatureRequired { tree_size });
        }
    }
    Offline::Verdict(verdict)
}

fn verify_window_pack(
    bytes: &[u8],
    anchor: Option<&Checkpoint>,
    policy: Option<&SignaturePolicy>,
) -> Offline<WindowVerification> {
    let pack: WindowEvidencePack = match serde_json::from_slice(bytes) {
        Ok(pack) => pack,
        Err(e) => {
            return Offline::Verdict(WindowVerification::Malformed {
                detail: e.to_string(),
            });
        }
    };
    let verdict = verify_window(&pack, anchor).unwrap_or_else(|e| WindowVerification::Malformed {
        detail: e.to_string(),
    });
    if !matches!(verdict, WindowVerification::Intact { .. }) {
        return Offline::Verdict(verdict);
    }
    // Policy over the window's end only (REMIT attribution wants a signed
    // window end; the trusted start is the anchor's business), and
    // presence only: a sparse pack cannot judge authority, so a pin
    // cannot be an intersection with it here.
    if let Some(policy) = policy {
        if policy.required_public_key.is_some() {
            return Offline::PinNeedsFullPrefix;
        }
        if let Some(tree_size) = policy.unsigned_at_or_after([&pack.to_checkpoint]) {
            return Offline::Verdict(WindowVerification::SignatureRequired { tree_size });
        }
    }
    Offline::Verdict(verdict)
}
