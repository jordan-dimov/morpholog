//! `rehearse`: every committed proposal re-decided under a candidate
//! programme, against the state the audit rows before it build.

use std::path::Path;

use anyhow::Context;
use morpholog_core::{PreparedProgram, RehearsalReport};
use morpholog_postgres::{RehearsalProgress, rehearse_candidate, rehearse_candidate_against_pack};

use crate::RehearseArgs;
use crate::commands::evidence::read_complete_prefix;
use crate::commands::{connect, parse_or_report, prepare_or_report, print_json, read_anchor};

pub(crate) async fn run(args: RehearseArgs) -> anyhow::Result<()> {
    let parsed = parse_or_report(&args.file)?;
    let prepared = prepare_or_report(&parsed)?;
    let mut progress = ProgressLine::default();
    let mut on_progress = |p: RehearsalProgress| {
        if let Some(line) = progress.line(p) {
            eprintln!("{line}");
        }
    };
    let report = match &args.pack {
        Some(pack_path) => against_pack(
            &prepared,
            pack_path,
            args.anchor_file.as_deref(),
            &mut on_progress,
        )?,
        None => {
            let url = args.database_url.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "provide --database-url (or set DATABASE_URL), or --pack <file> to \
                     rehearse offline against an evidence pack"
                )
            })?;
            let pool = connect(url).await?;
            rehearse_candidate(&pool, &prepared, &mut on_progress)
                .await
                .context("rehearsal failed")?
        }
    };
    print_json(&report)
}

fn against_pack(
    program: &PreparedProgram,
    pack_path: &Path,
    anchor_path: Option<&Path>,
    on_progress: &mut dyn FnMut(RehearsalProgress),
) -> anyhow::Result<RehearsalReport> {
    let pack = read_complete_prefix(pack_path)?;
    let anchor = read_anchor(anchor_path)?;
    rehearse_candidate_against_pack(program, &pack, anchor.as_ref(), on_progress)
        .context("rehearsing against the evidence pack failed")
}

/// Rows between progress lines.
const PROGRESS_EVERY: u64 = 10_000;

/// A line for stderr each time the rehearsal crosses another
/// `PROGRESS_EVERY` rows, so a long one says it is working. A short one
/// says nothing; stdout is only ever the report.
#[derive(Default)]
struct ProgressLine {
    shown: u64,
}

impl ProgressLine {
    fn line(&mut self, progress: RehearsalProgress) -> Option<String> {
        let reached = progress.done / PROGRESS_EVERY;
        if reached <= self.shown {
            return None;
        }
        self.shown = reached;
        Some(format!(
            "rehearse: re-decided {} of {} rows",
            progress.done, progress.total
        ))
    }
}
