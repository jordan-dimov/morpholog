//! Subcommand handlers.
//!
//! Each module in this directory carries one subcommand's logic. `main.rs`
//! holds the `clap`-derived `Cli`/`Command`/`Inspect` definitions and the
//! `main()` dispatch loop; everything else lives here.
//!
//! The split keeps `main.rs` reviewable: clap structs plus dispatch, no
//! handler bodies. Adding a new subcommand is "add a file here, add a
//! variant to `Command`, add one dispatch arm in `main`."
//!
//! Shared helpers used across handlers (`connect`, `print_json`) live at
//! the bottom of this file.

use anyhow::Context;
use morpholog_core::Program;
use morpholog_postgres::PgPool;
use morpholog_surface::parse_program;
use serde::Serialize;
use std::path::Path;

pub(crate) mod check;
pub(crate) mod explain;
pub(crate) mod inspect;
pub(crate) mod outbox;
pub(crate) mod parse;
pub(crate) mod run;
pub(crate) mod schema;

/// Read a `.morph` source file and parse it. On parse failure,
/// render diagnostics via ariadne to stderr and exit 1. Shared by
/// `parse` and `check` so the diagnostic rendering stays identical
/// across both subcommands.
///
/// Returns the parsed `Program` together with the original source
/// and source-name (the latter two are needed by callers that want
/// to render further diagnostics anchored in the same source).
pub(crate) fn parse_or_exit(file: &Path) -> anyhow::Result<(Program, String, String)> {
    let source = std::fs::read_to_string(file)
        .with_context(|| format!("read source file {}", file.display()))?;
    let source_name = file.display().to_string();
    match parse_program(&source) {
        Ok(program) => Ok((program, source, source_name)),
        Err(diagnostics) => {
            for d in &diagnostics {
                eprint!("{}", d.render(&source_name, &source));
            }
            std::process::exit(1);
        }
    }
}

/// Validate a parsed programme; on failure, print each diagnostic to
/// stderr and exit 1. The gate every subcommand that acts on a `.morph`
/// file's *semantics* applies after parsing - `run` and `explain` before
/// touching the database, `inspect derived`/`guarantees` before reading
/// or rendering - so an arbitrary file is held to the same vocabulary
/// contract the kernel would otherwise enforce only at proposal time.
pub(crate) fn validate_or_exit(program: &Program) {
    if let Err(errors) = program.validate() {
        for err in &errors {
            eprintln!("error: {err}");
        }
        std::process::exit(1);
    }
}

/// Open a PostgreSQL connection pool. Shared by every subcommand that
/// touches the database.
///
/// The URL is deliberately NOT included in error context: a typical
/// PostgreSQL connection string is `postgres://user:password@host/db`,
/// and echoing it into stderr (where it may be captured by shells, CI
/// logs, or terminal scrollback) would leak credentials. The underlying
/// `sqlx` error already describes what went wrong (DNS failure, refused,
/// authentication, etc.).
pub(crate) async fn connect(url: &str) -> anyhow::Result<PgPool> {
    PgPool::connect(url)
        .await
        .context("failed to connect to PostgreSQL")
}

/// Pretty-print a value as JSON to stdout. The canonical output shape
/// for every read-only subcommand.
pub(crate) fn print_json<T: Serialize>(value: &T) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(value).context("JSON encoding failed")?;
    println!("{json}");
    Ok(())
}
