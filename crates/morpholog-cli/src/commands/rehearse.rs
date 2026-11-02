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
            "rehearse: processed {} of {} audit rows",
            progress.done, progress.total
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lines a record of `total` rows prints when the fold reports
    /// every `chunk` rows: 1024 on the database path, 1000 on the pack
    /// path.
    fn lines(total: u64, chunk: u64) -> Vec<String> {
        let mut progress = ProgressLine::default();
        (1..=total.div_ceil(chunk))
            .filter_map(|n| {
                progress.line(RehearsalProgress {
                    done: (n * chunk).min(total),
                    total,
                })
            })
            .collect()
    }

    #[test]
    fn a_short_record_prints_nothing() {
        assert!(lines(9_999, 1024).is_empty());
        assert!(lines(9_999, 1000).is_empty());
    }

    #[test]
    fn one_line_per_ten_thousand_rows_crossed_on_either_cadence() {
        assert_eq!(
            lines(25_000, 1024),
            [
                "rehearse: processed 10240 of 25000 audit rows",
                "rehearse: processed 20480 of 25000 audit rows",
            ]
        );
        assert_eq!(
            lines(25_000, 1000),
            [
                "rehearse: processed 10000 of 25000 audit rows",
                "rehearse: processed 20000 of 25000 audit rows",
            ]
        );
    }

    #[test]
    fn a_threshold_prints_once_however_many_reports_cross_it() {
        let mut progress = ProgressLine::default();
        let at = |done| RehearsalProgress {
            done,
            total: 50_000,
        };
        assert!(progress.line(at(10_000)).is_some());
        assert!(progress.line(at(10_001)).is_none());
        assert!(progress.line(at(19_999)).is_none());
        assert!(progress.line(at(20_000)).is_some());
        // A jump over two thresholds prints one line, not two.
        assert_eq!(
            progress.line(at(45_000)).as_deref(),
            Some("rehearse: processed 45000 of 50000 audit rows")
        );
        assert!(progress.line(at(49_999)).is_none());
    }
}
