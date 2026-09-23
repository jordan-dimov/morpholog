//! Subcommand handlers, one module per subcommand.
//!
//! `main.rs` holds only the clap definitions and the dispatch. Adding a
//! subcommand means a file here, a variant on `Command`, and one dispatch
//! arm in `main`. Helpers shared across handlers live in this file.

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
pub(crate) mod provision;
pub(crate) mod refresh;
pub(crate) mod schema;
pub(crate) mod session;
pub(crate) mod transact;
pub(crate) mod verify;
pub(crate) mod witness;

/// One parsed `.morph` file with everything needed to render a
/// later finding against its source: the programme, the source map
/// the parser kept, and the original text and display name.
pub(crate) struct ParsedSource {
    pub(crate) program: Program,
    pub(crate) map: SourceMap,
    pub(crate) source: String,
    pub(crate) source_name: String,
}

/// A failure the command has already reported in full, carried out to
/// `main` only to set the exit code. `main` prints nothing more for it.
///
/// Returning it instead of calling `std::process::exit` keeps the helpers
/// composable and testable in-process.
#[derive(Debug)]
pub(crate) struct AlreadyReported;

/// The exit code for a one-shot proposal whose commit outcome could not be
/// proven. It differs from every other failure (1) and from a usage error
/// (2), so a caller knows to read the record before re-submitting.
pub(crate) const EXIT_COMMIT_OUTCOME_UNKNOWN: u8 = 3;

/// A one-shot proposal's COMMIT failed without a PostgreSQL verdict.
#[derive(Debug)]
pub(crate) struct CommitOutcomeUnknown(pub(crate) String);

impl std::fmt::Display for CommitOutcomeUnknown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the commit outcome is unknown - read the record before re-submitting: {}",
            self.0
        )
    }
}

impl std::error::Error for CommitOutcomeUnknown {}

impl std::fmt::Display for AlreadyReported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Only seen if something re-renders it; the real diagnostics
        // were already printed.
        f.write_str("the command reported its own diagnostics")
    }
}

impl std::error::Error for AlreadyReported {}

/// Read a `.morph` source file and parse it. On parse failure, render the
/// diagnostics to stderr and return [`AlreadyReported`]. Every command that
/// reads a `.morph` file goes through here, so they all render alike.
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

/// Validate a parsed programme. On failure, print each diagnostic to stderr
/// (with a caret where the source map can place it) and return
/// [`AlreadyReported`]. On success, return the [`ValidatedProgram`] handle
/// that analysis ([`morpholog_core::transformation_param_kinds`],
/// [`morpholog_core::transformation_arg_schema`]) takes, so validation runs
/// once.
///
/// Every subcommand that acts on a file's meaning runs this after parsing,
/// so a bad file is refused up front rather than at proposal time.
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
/// [`CompiledProgram`]: transformation lookup, the analysis handle
/// ([`CompiledProgram::validated`]) and the rule slices in one object. For
/// commands that look transformations up by name.
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

/// Resolve a transformation by name. The not-found error lists every
/// transformation the file declares. Shared so the error reads the same
/// from every command.
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

/// Open a PostgreSQL connection pool.
///
/// The error never includes the URL: it may carry a password, and stderr
/// ends up in logs. The `sqlx` error already says what went wrong.
pub(crate) async fn connect(url: &str) -> anyhow::Result<PgPool> {
    // `postgres:///mydb` means "the OS user" to every other Postgres
    // tool; sqlx 0.9 alone reads it as `anonymous`.
    let url = morpholog_postgres::with_default_user(url);
    PgPool::connect(&url)
        .await
        .context("failed to connect to PostgreSQL")
}

/// `connect`, capped at one connection, for sessions. A lockstep session
/// never needs a second one, and the cap bounds load when many workers each
/// hold a session open.
pub(crate) async fn connect_single(url: &str) -> anyhow::Result<PgPool> {
    let url = morpholog_postgres::with_default_user(url);
    morpholog_postgres::single_connection_pool(&url)
        .await
        .context("failed to connect to PostgreSQL")
}

/// The read-side tail every inspect surface shares: the structured form
/// under `--json`, the rendered prose otherwise.
pub(crate) fn emit<T: Serialize>(
    json: bool,
    value: &T,
    prose: impl FnOnce() -> String,
) -> anyhow::Result<()> {
    if json {
        print_json(value)
    } else {
        println!("{}", prose());
        Ok(())
    }
}

/// Read and parse a JSON file, naming the file and what it was meant
/// to be in any failure.
pub(crate) fn read_json<T: serde::de::DeserializeOwned>(
    path: &Path,
    noun: &str,
    shape: &str,
) -> anyhow::Result<T> {
    let bytes =
        std::fs::read(path).with_context(|| format!("reading {noun} file {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {noun} file {} as {shape}", path.display()))
}

/// The external anchor a verifier or scorer holds, when one was given.
pub(crate) fn read_anchor(
    path: Option<&Path>,
) -> anyhow::Result<Option<morpholog_postgres::Checkpoint>> {
    path.map(|p| read_json(p, "anchor", "a checkpoint"))
        .transpose()
}

/// Pretty-print a value as JSON to stdout. The canonical output shape
/// for every read-only subcommand.
pub(crate) fn print_json<T: Serialize>(value: &T) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(value).context("JSON encoding failed")?;
    println!("{json}");
    Ok(())
}
