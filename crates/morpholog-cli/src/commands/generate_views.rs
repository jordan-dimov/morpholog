//! `morpholog generate views` - emit a typed, read-only SQL view surface
//! over `morpholog.claims` for a `.morph` programme's base predicates.
//!
//! A thin wrapper over [`morpholog_postgres::render_views`]: parse,
//! validate, hash, render, then write to `--out` or print raw to stdout for
//! piping to `psql`. The SQL knowledge lives in `morpholog-postgres`.
//!
//! Refusal is whole-run, as in `generate python-client`: every identifier
//! that cannot be emitted is printed to stderr, and nothing is written.

use std::io::Write as _;

use morpholog_postgres::render_views;

use crate::GenerateViewsArgs;
use crate::commands::{AlreadyReported, parse_or_report, validate_or_report};

pub(crate) fn run(args: &GenerateViewsArgs) -> anyhow::Result<()> {
    let parsed = parse_or_report(&args.file)?;
    let validated = validate_or_report(&parsed)?;
    let hash = crate::commands::hash::canonical_hash(&parsed.program);

    let rendered = match render_views(validated, &args.schema, &hash) {
        Ok(rendered) => rendered,
        Err(refusals) => {
            for refusal in &refusals {
                eprintln!("error: {refusal}");
            }
            eprintln!(
                "generate views refused: {} finding(s); nothing was written",
                refusals.len()
            );
            return Err(AlreadyReported.into());
        }
    };

    let total = rendered.base_view_count + rendered.derived_view_count;
    let summary = format!(
        "generated {total} view(s): {} base, {} derived",
        rendered.base_view_count, rendered.derived_view_count
    );
    match &args.out {
        Some(path) => {
            std::fs::write(path, &rendered.sql)?;
            eprintln!("{summary} -> {}", path.display());
        }
        None => {
            // Exactly the rendered script, for piping to psql. It has its
            // own trailing newline.
            let stdout = std::io::stdout();
            let mut handle = stdout.lock();
            handle.write_all(rendered.sql.as_bytes())?;
            handle.flush()?;
            eprintln!("{summary}");
        }
    }
    Ok(())
}
