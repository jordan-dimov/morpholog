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

use anyhow::{Context, anyhow};
use morpholog_core::{
    CompiledProgram, Program, Transformation, TransformationName, ValidatedProgram, ValidationError,
};
use morpholog_postgres::PgPool;
use morpholog_surface::{Diagnostic, SourceMap, parse_program_with_sources};
use serde::Serialize;
use std::path::Path;

pub(crate) mod args;
pub(crate) mod check;
pub(crate) mod checkpoint;
pub(crate) mod evaluate;
pub(crate) mod evidence;
pub(crate) mod explain;
pub(crate) mod filter;
pub(crate) mod generate;
pub(crate) mod generate_views;
pub(crate) mod hash;
pub(crate) mod init;
pub(crate) mod inspect;
pub(crate) mod keygen;
pub(crate) mod migrate;
pub(crate) mod outbox;
pub(crate) mod propose;
pub(crate) mod refresh;
pub(crate) mod schema;
pub(crate) mod session;
pub(crate) mod verify;

/// One parsed `.morph` file with everything needed to render a
/// later finding against its source: the programme, the source map
/// the parser kept, and the original text and display name.
pub(crate) struct ParsedSource {
    pub(crate) program: Program,
    pub(crate) map: SourceMap,
    pub(crate) source: String,
    pub(crate) source_name: String,
}

/// A failure the command has already reported in full - diagnostics
/// rendered, remedy named - carried out to `main` only so it can set
/// the exit code.
///
/// The alternative was calling `std::process::exit` from wherever the
/// diagnostic was printed, which made those helpers unusable from
/// anything that wanted to keep running: a caller could not compose
/// them, and a test could not observe them without spawning a
/// process. `main` prints nothing for this variant - the command
/// already said everything.
#[derive(Debug)]
pub(crate) struct AlreadyReported;

impl std::fmt::Display for AlreadyReported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Reached only if something re-renders it; the diagnostics
        // that matter were printed where they were found.
        f.write_str("the command reported its own diagnostics")
    }
}

impl std::error::Error for AlreadyReported {}

/// Read a `.morph` source file and parse it. On parse failure,
/// render diagnostics via ariadne to stderr and return
/// [`AlreadyReported`] - the caller decides what to do next, and
/// `main` prints nothing further. Shared by `parse` and `check` so
/// the diagnostic rendering stays identical across both subcommands.
pub(crate) fn parse_or_report(file: &Path) -> anyhow::Result<ParsedSource> {
    let source = std::fs::read_to_string(file)
        .with_context(|| format!("read source file {}", file.display()))?;
    let source_name = file.display().to_string();
    match parse_program_with_sources(&source) {
        Ok((program, map)) => Ok(ParsedSource {
            program,
            map,
            source,
            source_name,
        }),
        Err(diagnostics) => {
            for d in &diagnostics {
                eprint!("{}", d.render(&source_name, &source));
            }
            Err(AlreadyReported.into())
        }
    }
}

/// Render one validation error to stderr: an ariadne caret block when
/// the source map places it, the plain `error: ...` line when it has
/// no source anchor (a generated discipline invariant, for one).
pub(crate) fn render_validation_error(err: &ValidationError, parsed: &ParsedSource) {
    match parsed.map.span_for_error(err) {
        Some(span) => eprint!(
            "{}",
            Diagnostic::error(err.to_string(), span).render(&parsed.source_name, &parsed.source)
        ),
        None => eprintln!("error: {err}"),
    }
}

/// Validate a parsed programme; on failure, print each diagnostic to
/// stderr (caret-located where the source map can place it) and
/// return [`AlreadyReported`]; on success, a [`ValidatedProgram`]
/// handle the analysis
/// surface ([`morpholog_core::transformation_param_kinds`],
/// [`morpholog_core::transformation_arg_schema`]) consumes. Threading
/// the handle through means the CLI pays the validation cost once,
/// instead of once here and once again inside the analysis layer's
/// previous defensive re-validation.
///
/// The gate every subcommand that acts on a `.morph` file's
/// *semantics* applies after parsing - `propose` and `explain` before
/// touching the database, `inspect derived`/`guarantees` before
/// reading or rendering, `schema` before computing the JSON Schema -
/// so an arbitrary file is held to the same vocabulary contract the
/// kernel would otherwise enforce only at proposal time.
pub(crate) fn validate_or_report(parsed: &ParsedSource) -> anyhow::Result<ValidatedProgram<'_>> {
    match parsed.program.validated() {
        Ok(validated) => Ok(validated),
        Err(errors) => {
            for err in &errors {
                render_validation_error(err, parsed);
            }
            Err(AlreadyReported.into())
        }
    }
}

/// Like [`validate_or_report`], but returns the owned, indexed
/// [`CompiledProgram`] - one model object the command sources its
/// transformation lookups, analysis handle ([`CompiledProgram::validated`]),
/// and rule slices from. Clones the parsed programme once (negligible,
/// one-time per invocation). Used by the by-name-lookup paths (`propose`,
/// `explain`); the analysis-only commands keep `validate_or_report`.
pub(crate) fn compile_or_report(parsed: &ParsedSource) -> anyhow::Result<CompiledProgram> {
    match CompiledProgram::new(parsed.program.clone()) {
        Ok(compiled) => Ok(compiled),
        Err(errors) => {
            for err in &errors {
                render_validation_error(err, parsed);
            }
            Err(AlreadyReported.into())
        }
    }
}

/// Resolve a transformation by name against a compiled programme, the
/// not-found error naming every transformation the file does declare.
/// Shared by `propose` and `explain` so the lookup error (and any future
/// "did you mean?" refinement) cannot drift between them.
pub(crate) fn lookup_transformation<'a>(
    compiled: &'a CompiledProgram,
    name: &str,
    file: &Path,
) -> anyhow::Result<&'a Transformation> {
    compiled
        .transformation(&TransformationName::from(name))
        .ok_or_else(|| {
            let available = compiled
                .program()
                .transformations
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow!(
                "transformation `{name}` not found in `{}`. Available: {available}",
                file.display(),
            )
        })
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
    // `postgres:///mydb` means "the OS user" to every other Postgres
    // tool; sqlx 0.9 alone reads it as `anonymous`.
    let url = morpholog_postgres::with_default_user(url);
    PgPool::connect(&url)
        .await
        .context("failed to connect to PostgreSQL")
}

/// `connect`, capped at one connection: the session's shape. A
/// lockstep protocol cannot use a second connection, and the cap
/// bounds database connection load when many application workers each
/// hold a session open.
pub(crate) async fn connect_single(url: &str) -> anyhow::Result<PgPool> {
    let url = morpholog_postgres::with_default_user(url);
    morpholog_postgres::single_connection_pool(&url)
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
