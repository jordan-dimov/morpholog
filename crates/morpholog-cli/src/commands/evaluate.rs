//! `morpholog evaluate` - score a candidate programme against history. The
//! evaluator pointed backward: replay the audit log under a candidate's
//! invariants (which are NOT deployed) and report which already-admitted
//! commits each would have refused. The history is either a live database
//! or, with `--pack`, a portable evidence pack scored entirely offline.
//! Output is JSON - the fitness contract a discovery loop consumes.

use std::path::Path;

use anyhow::Context;
use morpholog_core::{BatchScore, CandidateScore, Program, invariants_using_pre};
use morpholog_postgres::{
    Checkpoint, EvidencePack, score_candidate, score_candidate_against_pack,
    score_candidate_against_packs,
};

use crate::EvaluateArgs;
use crate::commands::{connect, parse_or_exit, print_json, validate_or_exit};

pub(crate) async fn run(args: EvaluateArgs) -> anyhow::Result<()> {
    let parsed = parse_or_exit(&args.file)?;
    validate_or_exit(&parsed);

    // Fail fast in every mode, before any database or pack work: v1 scores
    // state invariants only, so a transition-relational candidate is
    // rejected here rather than after the work.
    let pre = invariants_using_pre(&parsed.program);
    if !pre.is_empty() {
        eprintln!(
            "error: `evaluate` v1 scores state invariants only; \
             these use pre(...) (transition-relational, deferred): {}",
            pre.join(", ")
        );
        std::process::exit(1);
    }

    // Batch over a directory of packs: a single JSON report, offline.
    if let Some(dir) = &args.packs {
        let report = score_against_packs(&parsed.program, dir)?;
        return print_json(&report);
    }

    let report = match &args.pack {
        Some(pack_path) => {
            score_against_pack(&parsed.program, pack_path, args.anchor_file.as_deref())?
        }
        None => {
            let url = args.database_url.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "provide --database-url (or set DATABASE_URL), --pack <file>, or \
                     --packs <dir> to score offline against evidence packs"
                )
            })?;
            let pool = connect(url).await?;
            score_candidate(&pool, &parsed.program)
                .await
                .context("score_candidate failed")?
        }
    };
    print_json(&report)
}

/// Score the candidate against every `*.json` evidence pack in `dir`, in one
/// process, offline. Packs are taken in file-name order (deterministic). A
/// file that cannot be read or parsed aborts the batch - the packs directory
/// is controlled input - whereas a genuine pack that does not verify is a
/// per-case failure inside the report.
fn score_against_packs(program: &Program, dir: &Path) -> anyhow::Result<BatchScore> {
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading packs directory {}", dir.display()))?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();

    let named: Vec<(String, EvidencePack)> = paths
        .iter()
        .map(|path| {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let bytes = std::fs::read(path)
                .with_context(|| format!("reading pack file {}", path.display()))?;
            let pack: EvidencePack = serde_json::from_slice(&bytes).with_context(|| {
                format!("parsing pack file {} as an evidence pack", path.display())
            })?;
            Ok((name, pack))
        })
        .collect::<anyhow::Result<_>>()?;

    if named.is_empty() {
        anyhow::bail!("no `*.json` evidence packs found in {}", dir.display());
    }

    score_candidate_against_packs(program, &named).context("scoring against the packs failed")
}

/// Read an evidence pack (and optional external anchor) and score the
/// candidate against it, offline. File handling mirrors `evidence verify`.
fn score_against_pack(
    program: &Program,
    pack_path: &Path,
    anchor_path: Option<&Path>,
) -> anyhow::Result<CandidateScore> {
    let bytes = std::fs::read(pack_path)
        .with_context(|| format!("reading pack file {}", pack_path.display()))?;
    let pack: EvidencePack = serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "parsing pack file {} as an evidence pack",
            pack_path.display()
        )
    })?;

    let anchor: Option<Checkpoint> = match anchor_path {
        Some(path) => {
            let bytes = std::fs::read(path)
                .with_context(|| format!("reading anchor file {}", path.display()))?;
            Some(serde_json::from_slice(&bytes).with_context(|| {
                format!("parsing anchor file {} as a checkpoint", path.display())
            })?)
        }
        None => None,
    };

    score_candidate_against_pack(program, &pack, anchor.as_ref())
        .context("scoring against the evidence pack failed")
}
