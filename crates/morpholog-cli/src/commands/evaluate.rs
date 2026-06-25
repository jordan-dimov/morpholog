//! `morpholog evaluate` - score a candidate programme against history. The
//! evaluator pointed backward: replay the audit log under a candidate's
//! invariants (which are NOT deployed) and report which already-admitted
//! commits each would have refused. The history is either a live database
//! or, with `--pack`, a portable evidence pack scored entirely offline.
//! Output is JSON - the fitness contract a discovery loop consumes.

use std::path::Path;

use anyhow::Context;
use morpholog_core::{CandidateScore, Program, invariants_using_pre};
use morpholog_postgres::{Checkpoint, EvidencePack, score_candidate, score_candidate_against_pack};

use crate::EvaluateArgs;
use crate::commands::{connect, parse_or_exit, print_json, validate_or_exit};

pub(crate) async fn run(args: EvaluateArgs) -> anyhow::Result<()> {
    let parsed = parse_or_exit(&args.file)?;
    validate_or_exit(&parsed);

    // Fail fast in either mode, before any database or pack work: v1 scores
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

    let report = match &args.pack {
        Some(pack_path) => {
            score_against_pack(&parsed.program, pack_path, args.anchor_file.as_deref())?
        }
        None => {
            let url = args.database_url.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "provide --database-url (or set DATABASE_URL), or --pack <file> \
                     to score offline against an evidence pack"
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
