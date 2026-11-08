//! `morpholog generate views` - emit a typed, read-only SQL view surface
//! over `morpholog.claims` for a `.morph` programme's base predicates.
//!
//! A thin wrapper over [`morpholog_postgres::render_views`]: parse,
//! validate, hash, render, then write to `--out` or print raw to stdout for
//! piping to `psql`, and with `--describe` write the surface's description
//! beside it. Both are rendered before either is written, and each file
//! lands whole or not at all. The SQL knowledge lives in
//! `morpholog-postgres`.
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

    let rendered = match render_views(&validated, &args.schema, &hash) {
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
    if let Some(path) = &args.describe {
        let mut json = serde_json::to_string_pretty(&rendered.surface)?;
        json.push('\n');
        write_whole(path, json.as_bytes())?;
        eprintln!("described {total} view(s) -> {}", path.display());
    }
    match &args.out {
        Some(path) => {
            write_whole(path, rendered.sql.as_bytes())?;
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

/// Write to a sibling and rename over `path`, so a reader never sees a
/// partial file.
fn write_whole(path: &std::path::Path, bytes: &[u8]) -> anyhow::Result<()> {
    let mut partial = path.as_os_str().to_owned();
    partial.push(".partial");
    let partial = std::path::PathBuf::from(partial);
    std::fs::write(&partial, bytes)?;
    std::fs::rename(&partial, path)?;
    Ok(())
}
