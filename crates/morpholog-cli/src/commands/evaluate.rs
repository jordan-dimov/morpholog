//! `morpholog evaluate` - score a candidate programme against history.
//! Replays the audit log under the candidate's invariants (never deployed)
//! and reports which past commits each would have refused. History is a
//! live database or, with `--pack`, an evidence pack read offline. Output
//! is JSON, for a discovery loop to consume.

use std::path::Path;

use anyhow::Context;
use morpholog_core::{BatchScore, CandidateScore, Program, invariants_using_pre};
use morpholog_postgres::{
    EvidencePack, SplitBoundary, read_prefix_stream, score_candidate, score_candidate_against_pack,
    score_candidate_against_packs_lazily,
};

use crate::EvaluateArgs;
use crate::commands::evidence::{PackInput, open_pack};
use crate::commands::{
    AlreadyReported, connect, parse_or_report, print_json, read_anchor, validate_or_report,
};

pub(crate) async fn run(args: EvaluateArgs) -> anyhow::Result<()> {
    let parsed = parse_or_report(&args.file)?;
    validate_or_report(&parsed)?;

    // Only state invariants can be scored, so refuse `pre(...)` before
    // any database or pack work.
    let pre = invariants_using_pre(&parsed.program);
    if !pre.is_empty() {
        eprintln!(
            "error: `evaluate` v1 scores state invariants only; \
             these use pre(...) (transition-relational, deferred): {}",
            pre.join(", ")
        );
        return Err(AlreadyReported.into());
    }

    let split = args
        .train_until
        .as_deref()
        .map(parse_boundary)
        .transpose()?;

    // Batch over a directory of packs: a single JSON report, offline.
    if let Some(dir) = &args.packs {
        let report = score_against_packs(&parsed.program, dir)?;
        return print_json(&report);
    }

    let report = match &args.pack {
        Some(pack_path) => score_against_pack(
            &parsed.program,
            pack_path,
            args.anchor_file.as_deref(),
            split,
        )?,
        None => {
            let url = args.database_url.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "provide --database-url (or set DATABASE_URL), --pack <file>, or \
                     --packs <dir> to score offline against evidence packs"
                )
            })?;
            let pool = connect(url).await?;
            score_candidate(&pool, &parsed.program, split)
                .await
                .context("score_candidate failed")?
        }
    };
    print_json(&report)
}

/// Parse a `--train-until` boundary: a transition id first, else an
/// RFC 3339 timestamp.
fn parse_boundary(raw: &str) -> anyhow::Result<SplitBoundary> {
    if let Ok(id) = raw.parse::<uuid::Uuid>() {
        return Ok(SplitBoundary::Transition(id));
    }
    let at = morpholog_postgres::wire_time::parse(raw).map_err(|e| {
        anyhow::anyhow!(
            "--train-until takes a transition id or an RFC 3339 timestamp \
             (e.g. 2026-07-01T00:00:00Z); `{raw}` parses as neither: {e}"
        )
    })?;
    Ok(SplitBoundary::AtOrBefore(at))
}

/// Score the candidate against every evidence pack in `dir` (`.json` or
/// `.ndjson`, either gzip-compressed or not), offline, in file-name order,
/// reading one pack at a time. An unreadable or unparseable file aborts
/// the batch, since the directory is controlled input. A pack that parses
/// but does not verify is a per-case failure in the report.
fn score_against_packs(program: &Program, dir: &Path) -> anyhow::Result<BatchScore> {
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("reading packs directory {}", dir.display()))?
    {
        // A directory entry error is a setup problem: abort.
        let path = entry
            .with_context(|| format!("reading an entry in {}", dir.display()))?
            .path();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        if name.is_some_and(|n| {
            [".json", ".ndjson", ".json.gz", ".ndjson.gz"]
                .iter()
                .any(|ext| n.ends_with(ext))
        }) {
            paths.push(path);
        }
    }
    paths.sort();
    if paths.is_empty() {
        anyhow::bail!("no evidence packs found in {}", dir.display());
    }

    let named = paths.iter().map(|path| {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok::<_, anyhow::Error>((name, read_complete_prefix(path)?))
    });
    score_candidate_against_packs_lazily(program, named).context("scoring against the packs failed")
}

/// A complete-prefix pack in either form, whole in memory, since scoring
/// replays every row.
fn read_complete_prefix(path: &Path) -> anyhow::Result<EvidencePack> {
    let not_a_pack = || format!("{} is not a complete-prefix evidence pack", path.display());
    match open_pack(path)? {
        PackInput::Stream(input) => read_prefix_stream(input).with_context(not_a_pack),
        PackInput::Document(bytes) => serde_json::from_slice(&bytes).with_context(not_a_pack),
        PackInput::Newer(n) => Err(anyhow::anyhow!(
            "pack_format_version {n} is newer than this binary understands"
        ))
        .with_context(not_a_pack),
        PackInput::Unreadable(detail) => Err(anyhow::anyhow!(detail)).with_context(not_a_pack),
    }
}

/// Read an evidence pack (and optional external anchor) and score the
/// candidate against it, offline. File handling mirrors `audit verify-pack`.
fn score_against_pack(
    program: &Program,
    pack_path: &Path,
    anchor_path: Option<&Path>,
    split: Option<SplitBoundary>,
) -> anyhow::Result<CandidateScore> {
    let pack = read_complete_prefix(pack_path)?;
    let anchor = read_anchor(anchor_path)?;

    score_candidate_against_pack(program, &pack, anchor.as_ref(), split)
        .context("scoring against the evidence pack failed")
}
