//! `morpholog audit export` / `verify-pack` - export a portable evidence pack over the audit
//! log (a complete prefix, or a window between two checkpoints), and verify
//! one offline.
//!
//! `export` needs the database; `verify-pack` does not, so a third party
//! can check a pack with no database access at all.

use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::path::Path;

use morpholog_postgres::{
    Checkpoint, EvidencePack, PackError, PackVerdict, PackVerificationReport, RoleRebindings,
    SelectiveEvidencePack, SelectiveVerification, SignaturePolicy, TreeVerification,
    WindowEvidencePack, WindowStart, WindowVerification, WitnessesReport, begin_prefix_export,
    export_selective, export_window, pack_format_version, pack_role_rebindings,
    streamed_pack_version, verify_pack, verify_prefix_stream, verify_selective, verify_window,
    with_anchor_signatures, witnesses_report,
};

use anyhow::Context;

use crate::commands::verify::{signature_policy, witness_anchors};
use crate::commands::{AlreadyReported, connect, print_json, read_anchor, read_json};
use crate::{EvidenceExportArgs, EvidenceVerifyArgs};

/// `audit export`: a complete-prefix pack by default, a window between two
/// checkpoints with a `--from-*` start, or, with `--transition`, a
/// selective pack of only the named transitions. A complete prefix prints
/// one line at a time; a window or selective pack as one JSON document.
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
        None => export_prefix(&pool, args.tree_size).await?,
    }
    Ok(())
}

/// The complete prefix, one line at a time: the manifest, each
/// checkpoint, then every covered row in log order.
async fn export_prefix(
    pool: &morpholog_postgres::PgPool,
    tree_size: Option<i64>,
) -> anyhow::Result<()> {
    let mut export = begin_prefix_export(pool, tree_size)
        .await
        .context("export failed")?;
    let mut out = std::io::BufWriter::new(std::io::stdout());
    write_line(&mut out, &export.manifest)?;
    for checkpoint in &export.checkpoints {
        write_line(&mut out, checkpoint)?;
    }
    loop {
        let page = export.next_page().await.context("reading an audit page")?;
        if page.is_empty() {
            break;
        }
        for row in &page {
            write_line(&mut out, row)?;
        }
    }
    out.flush()?;
    Ok(())
}

fn write_line(out: &mut impl Write, value: &impl serde::Serialize) -> anyhow::Result<()> {
    serde_json::to_writer(&mut *out, value)?;
    out.write_all(b"\n")?;
    Ok(())
}

/// `audit verify-pack`: check a pack offline. A prefix pack recomputes its
/// root from every row; a window pack checks a consistency proof and
/// per-row inclusion proofs; `pack_format_version` says which. Prints one
/// JSON report and exits 1 on any tamper, divergence or malformed pack,
/// like `audit verify`.
pub(crate) fn verify(args: EvidenceVerifyArgs) -> anyhow::Result<()> {
    let anchor = read_anchor(args.anchor_file.as_deref())?;
    let policy = signature_policy(
        args.require_signatures,
        args.require_signatures_from,
        args.require_signing_key.as_deref(),
    )?;

    // Each pack kind has its own verifier and verdict shape, chosen by the
    // format version. An unknown future version is named as such, not read
    // as a malformed v1. A file that is not a pack is still a verdict, not
    // an operational error.
    let (verdict, chain, role_rebindings) = match open_pack(&args.pack_file)? {
        PackInput::Stream(input) => {
            let (verdict, checkpoints, role_rebindings) =
                verify_streamed(input, anchor.as_ref(), policy.as_ref())?;
            (verdict, Chain::Held(checkpoints), role_rebindings)
        }
        PackInput::Newer(n) => (
            newer_than_this_binary(n),
            Chain::Held(Vec::new()),
            RoleRebindings::NotEvaluated,
        ),
        PackInput::Unreadable(detail) => (
            PackVerdict::Prefix(TreeVerification::MalformedPack { detail }),
            Chain::Held(Vec::new()),
            RoleRebindings::NotEvaluated,
        ),
        PackInput::Document(bytes) => {
            let verdict = verify_document(&bytes, anchor.as_ref(), policy.as_ref())?;
            let role_rebindings = pack_role_rebindings(&bytes, &verdict);
            (verdict, Chain::InDocument(bytes), role_rebindings)
        }
    };
    let intact = verdict.is_intact();

    // Witnesses are judged apart from the verdict, and only when asked for.
    // Role rebindings are read from the rows only an intact verdict
    // established, and never fail the check.
    let mut witness_invalid = false;
    let witnesses = if args.witnesses || args.trusted_tsa_file.is_some() {
        let anchors = witness_anchors(args.trusted_tsa_file.as_deref())?;
        let witnesses = witnesses_report(&chain.checkpoints(), anchors.as_ref());
        witness_invalid = witnesses.as_ref().is_some_and(WitnessesReport::any_invalid);
        witnesses
    } else {
        None
    };
    print_json(&PackVerificationReport {
        verdict,
        witnesses,
        role_rebindings,
    })?;

    if !intact || witness_invalid {
        return Err(AlreadyReported.into());
    }
    Ok(())
}

/// The largest first line read to tell a line-oriented pack from a single
/// JSON document. A line-oriented manifest is a few hundred bytes; a
/// document's first line can be the whole file.
const MANIFEST_LINE_LIMIT: u64 = 4096;

pub(crate) enum PackInput {
    Stream(Box<dyn BufRead>),
    Newer(u32),
    Unreadable(String),
    Document(Vec<u8>),
}

/// Open a pack, decompressing it if it is gzip, and tell its kind from at
/// most its first line, so a streamed pack is never read whole.
pub(crate) fn open_pack(path: &Path) -> anyhow::Result<PackInput> {
    let reading = || format!("reading pack file {}", path.display());
    let mut raw = BufReader::new(std::fs::File::open(path).with_context(reading)?);
    let gzip = raw
        .fill_buf()
        .with_context(reading)?
        .starts_with(&[0x1f, 0x8b]);
    let mut input: Box<dyn BufRead> = if gzip {
        Box::new(BufReader::new(flate2::bufread::MultiGzDecoder::new(raw)))
    } else {
        Box::new(raw)
    };
    let mut first = Vec::new();
    if let Err(e) = (&mut input)
        .take(MANIFEST_LINE_LIMIT)
        .read_until(b'\n', &mut first)
    {
        return undecodable(e).with_context(reading);
    }
    if first.ends_with(b"\n") {
        match streamed_pack_version(&first) {
            Some(4) => return Ok(PackInput::Stream(Box::new(Cursor::new(first).chain(input)))),
            Some(n) if n > 4 => return Ok(PackInput::Newer(n)),
            _ => {}
        }
    }
    let mut bytes = first;
    if let Err(e) = input.read_to_end(&mut bytes) {
        return undecodable(e).with_context(reading);
    }
    Ok(PackInput::Document(bytes))
}

/// Compressed bytes that do not decode are a malformed pack, a verdict;
/// any other read failure is operational.
fn undecodable(e: std::io::Error) -> anyhow::Result<PackInput> {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::InvalidData | ErrorKind::InvalidInput | ErrorKind::UnexpectedEof => Ok(
            PackInput::Unreadable(format!("the pack could not be read: {e}")),
        ),
        _ => Err(e.into()),
    }
}

/// Where the checkpoints are, for the witness report: already held, or
/// still inside a single-document pack, read only if asked for.
enum Chain {
    Held(Vec<Checkpoint>),
    InDocument(Vec<u8>),
}

impl Chain {
    fn checkpoints(self) -> Vec<Checkpoint> {
        match self {
            Chain::Held(checkpoints) => checkpoints,
            Chain::InDocument(bytes) => pack_checkpoints(&bytes),
        }
    }
}

fn newer_than_this_binary(n: impl std::fmt::Display) -> PackVerdict {
    PackVerdict::Prefix(TreeVerification::MalformedPack {
        detail: format!(
            "pack_format_version {n} is newer than this binary understands; \
             upgrade morpholog to verify it"
        ),
    })
}

/// A complete-prefix pack read line by line. Policy runs only on an intact
/// tree, as for a single-document pack, and a verdict it changes leaves
/// the rows unestablished.
fn verify_streamed(
    input: impl BufRead,
    anchor: Option<&Checkpoint>,
    policy: Option<&SignaturePolicy>,
) -> anyhow::Result<(PackVerdict, Vec<Checkpoint>, RoleRebindings)> {
    let report = match verify_prefix_stream(input, anchor) {
        Ok(report) => report,
        Err(PackError::Read(e)) => return Err(e).context("reading the pack"),
        Err(e) => {
            return Ok((
                PackVerdict::Prefix(TreeVerification::MalformedPack {
                    detail: e.to_string(),
                }),
                Vec::new(),
                RoleRebindings::NotEvaluated,
            ));
        }
    };
    if let Some(policy) = policy
        && matches!(report.verdict, TreeVerification::Intact { .. })
        && let Some(violation) =
            policy.violation(&with_anchor_signatures(&report.checkpoints, anchor))
    {
        return Ok((
            PackVerdict::Prefix(violation.into()),
            report.checkpoints,
            RoleRebindings::NotEvaluated,
        ));
    }
    Ok((
        PackVerdict::Prefix(report.verdict),
        report.checkpoints,
        report.role_rebindings,
    ))
}

/// A single-document pack: v1 complete prefix, v2 window or v3 selective.
fn verify_document(
    bytes: &[u8],
    anchor: Option<&Checkpoint>,
    policy: Option<&SignaturePolicy>,
) -> anyhow::Result<PackVerdict> {
    Ok(match pack_format_version(bytes) {
        Some(2) => match verify_window_pack(bytes, anchor, policy) {
            Offline::Verdict(verdict) => PackVerdict::Window(verdict),
            Offline::PinNeedsFullPrefix => return Err(pin_needs_full_prefix()),
        },
        Some(3) => match verify_selective_pack(bytes, anchor, policy) {
            Offline::Verdict(verdict) => PackVerdict::Selective(verdict),
            Offline::PinNeedsFullPrefix => return Err(pin_needs_full_prefix()),
        },
        Some(n) if n > 4 => newer_than_this_binary(n),
        _ => PackVerdict::Prefix(verify_prefix_pack(bytes, anchor, policy)),
    })
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
