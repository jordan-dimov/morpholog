//! `morpholog generate views` - emit a typed, read-only SQL view surface
//! over `morpholog.claims` for a `.morph` programme's base predicates.
//!
//! Thin wrapper over [`morpholog_postgres::render_views`]: parse the
//! `.morph`, validate it (the same vocabulary gate `schema` and `hash`
//! apply), compute the canonical model hash, render, and either write the
//! script to `--out` or print it raw to stdout so it can be piped to
//! `psql`. The renderer is pure and DB-free; all the SQL knowledge lives
//! beside the claims<->JSONB wire mapping in `morpholog-postgres`.
//!
//! Refusal is whole-run: the renderer returns every un-emittable
//! identifier at once, and any finding fails the run with the full work
//! list printed to stderr and nothing written - the same discipline as
//! `generate python-client`.

use std::io::Write as _;

use morpholog_postgres::render_views;

use crate::GenerateViewsArgs;
use crate::commands::{parse_or_exit, validate_or_exit};

pub(crate) fn run(args: &GenerateViewsArgs) -> anyhow::Result<()> {
    let parsed = parse_or_exit(&args.file)?;
    let validated = validate_or_exit(&parsed);
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
            std::process::exit(1);
        }
    };

    match &args.out {
        Some(path) => {
            std::fs::write(path, &rendered.sql)?;
            eprintln!(
                "generated {} view(s) -> {}",
                rendered.view_count,
                path.display()
            );
        }
        None => {
            // Raw SQL to stdout, byte-identical to the rendered script, so
            // the pipe-to-psql contract holds. `print!` (not `println!`)
            // adds no trailing newline beyond the script's own.
            let stdout = std::io::stdout();
            let mut handle = stdout.lock();
            handle.write_all(rendered.sql.as_bytes())?;
            handle.flush()?;
            eprintln!("generated {} view(s)", rendered.view_count);
        }
    }
    Ok(())
}
