//! `morpholog audit export` / `verify-pack` - export a portable evidence pack over the audit
//! log (a complete prefix, or a window between two checkpoints), and verify
//! one offline.
//!
//! `export` needs the database; `verify-pack` does not, so a third party
//! can check a pack with no database access at all.

use morpholog_postgres::{
    Checkpoint, EvidencePack, PackVerdict, PackVerificationReport, SelectiveEvidencePack,
    SelectiveVerification, SignaturePolicy, TreeVerification, WindowEvidencePack, WindowStart,
    WindowVerification, WitnessesReport, export_pack, export_selective, export_window,
    pack_role_rebindings, verify_pack, verify_selective, verify_window, with_anchor_signatures,
    witnesses_report,
};

use anyhow::Context;

use crate::commands::verify::{signature_policy, witness_anchors};
use crate::commands::{AlreadyReported, connect, print_json, read_anchor, read_json};
use crate::{EvidenceExportArgs, EvidenceVerifyArgs};

/// `audit export`: a complete-prefix pack by default, a window between two
/// checkpoints with a `--from-*` start, or, with `--transition`, a
/// selective pack of only the named transitions. Printed as JSON.
///
/// Prefix and window packs carry the full audit rows (actors, arguments,
/// claims, intents), which may be confidential. A selective pack proves its
/// rows authentic but not that the selection is complete.
pub(crate) async fn export(args: EvidenceExportArgs) -> anyhow::Result<()> {
    let pool = connect(&args.db.database_url).await?;

    if !args.transition.is_empty() {
        let pack = export_selective(&pool, args.tree_size, &args.transition)
            .await
            .context("export_selective failed")?;
        return print_json(&pack);
    }

    // A window start: an anchor file (export refuses if the stored start
    // has diverged from it), or just a tree size, which is weaker.
    let start = match (&args.from_anchor, args.from_tree_size) {
        (Some(path), _) => Some(WindowStart::Anchor(read_json(
            path,
            "anchor",
            "a checkpoint",
        )?)),
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

/// `audit verify-pack`: check a pack offline. A prefix pack recomputes its
/// root from every row; a window pack checks a consistency proof and
/// per-row inclusion proofs; `pack_format_version` says which. Prints one
/// JSON report and exits 1 on any tamper, divergence or malformed pack,
/// like `audit verify`.
pub(crate) fn verify(args: EvidenceVerifyArgs) -> anyhow::Result<()> {
    let bytes = std::fs::read(&args.pack_file)
        .with_context(|| format!("reading pack file {}", args.pack_file.display()))?;

    let anchor = read_anchor(args.anchor_file.as_deref())?;

    // Each pack kind has its own verifier and verdict shape, chosen by the
    // format version. An unknown future version is named as such, not read
    // as a malformed v1. A file that is not a pack is still a verdict, not
    // an operational error.
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

    // Witnesses are judged apart from the verdict, and only when asked for.
    // Role rebindings are read from the rows only an intact verdict
    // established, and never fail the check.
    let mut witness_invalid = false;
    let witnesses = if args.witnesses || args.trusted_tsa_file.is_some() {
        let anchors = witness_anchors(args.trusted_tsa_file.as_deref())?;
        let witnesses = witnesses_report(&pack_checkpoints(&bytes), anchors.as_ref());
        witness_invalid = witnesses.as_ref().is_some_and(WitnessesReport::any_invalid);
        witnesses
    } else {
        None
    };
    print_json(&PackVerificationReport {
        verdict,
        witnesses,
        role_rebindings: pack_role_rebindings(&bytes, intact),
    })?;

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

/// An offline verifier's answer: the pack's verdict, or, once the pack is
/// proven intact, that the requested policy cannot be judged here. A
/// broken pack reports as broken whatever the policy.
enum Offline<V> {
    Verdict(V),
    PinNeedsFullPrefix,
}

/// A sparse pack can check signatures but not whether the key was
/// authorised at the time. A key pin checked here would quietly mean only
/// "signed by this key", so it is refused with the remedy.
fn pin_needs_full_prefix() -> anyhow::Error {
    anyhow::anyhow!(
        "--require-signing-key needs a complete-prefix pack: a window or selective pack \
         cannot establish signing-key authority, so the pin could only be checked \
         cryptographically. Verify a full-prefix pack, or the live log with `audit verify`."
    )
}

/// The `manifest.pack_format_version`, if present: a cheap way to tell
/// pack kinds apart before a typed deserialization.
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
    // Policy runs over the pack's own checkpoints, and only on an intact
    // tree, so every signature it sees is already proven and authorised.
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
    // Policy checks the one covering checkpoint for a signature only; a
    // sparse pack cannot judge key authority, so a pin is refused.
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
    // Policy checks only the window's end for a signature; the anchor
    // vouches for the start. A sparse pack cannot judge key authority, so
    // a pin is refused.
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
