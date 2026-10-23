//! `rehearse`: every committed proposal re-decided under a candidate
//! programme, against the state the audit rows before it build.

use std::path::Path;

use anyhow::Context;
use morpholog_core::{PreparedProgram, RehearsalReport};
use morpholog_postgres::{rehearse_candidate, rehearse_candidate_against_pack};

use crate::RehearseArgs;
use crate::commands::evidence::read_complete_prefix;
use crate::commands::{connect, parse_or_report, prepare_or_report, print_json, read_anchor};

pub(crate) async fn run(args: RehearseArgs) -> anyhow::Result<()> {
    let parsed = parse_or_report(&args.file)?;
    let prepared = prepare_or_report(&parsed)?;
    let report = match &args.pack {
        Some(pack_path) => against_pack(&prepared, pack_path, args.anchor_file.as_deref())?,
        None => {
            let url = args.database_url.as_deref().ok_or_else(|| {
                anyhow::anyhow!(
                    "provide --database-url (or set DATABASE_URL), or --pack <file> to \
                     rehearse offline against an evidence pack"
                )
            })?;
            let pool = connect(url).await?;
            rehearse_candidate(&pool, &prepared)
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
) -> anyhow::Result<RehearsalReport> {
    let pack = read_complete_prefix(pack_path)?;
    let anchor = read_anchor(anchor_path)?;
    rehearse_candidate_against_pack(program, &pack, anchor.as_ref())
        .context("rehearsing against the evidence pack failed")
}
