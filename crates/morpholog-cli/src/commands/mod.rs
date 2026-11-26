//! Subcommand handlers, one module per subcommand.
//!
//! `main.rs` holds only the clap definitions and the dispatch. Adding a
//! subcommand means a file here, a variant on `Command`, and one dispatch
//! arm in `main`. Helpers shared across handlers live in this file.

use anyhow::{Context, anyhow};
use morpholog_cli::envelopes::{PreflightCode, PreflightError};
use morpholog_core::{
    PreparedProgram, Program, Transformation, TransformationName, ValidatedProgram, ValidationError,
};
use morpholog_postgres::PgPool;
use morpholog_surface::{Diagnostic, SourceMap, SourceText};
use serde::Serialize;
use std::io::Write;
use std::path::Path;

pub(crate) mod args;
pub(crate) mod check;
pub(crate) mod checkpoint;
pub(crate) mod evaluate;
pub(crate) mod evidence;
pub(crate) mod explain;
pub(crate) mod filter;
pub(crate) mod generate;
pub(crate) mod generate_days;
pub(crate) mod generate_views;
pub(crate) mod hash;
pub(crate) mod init;
pub(crate) mod inspect;
pub(crate) mod keygen;
pub(crate) mod migrate;
pub(crate) mod out;
pub(crate) mod outbox;
pub(crate) mod parts;
pub(crate) mod propose;
pub(crate) mod provision;
pub(crate) mod receipt;
pub(crate) mod refresh;
pub(crate) mod rehearse;
pub(crate) mod resident;
pub(crate) mod roles;
pub(crate) mod schema;
pub(crate) mod serve;
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
    pub(crate) source: SourceText,
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

/// Read a programme's text: a `.morph` file, or the parts a `.parts`
/// manifest lists.
pub(crate) fn read_source(file: &Path) -> anyhow::Result<SourceText> {
    if parts::is_manifest(file) {
        return Ok(parts::read(file)?);
    }
    let text = std::fs::read_to_string(file)
        .with_context(|| format!("read source file {}", file.display()))?;
    Ok(SourceText::file(file.display().to_string(), text))
}

/// Read a programme, from a `.morph` file or a `.parts` manifest, and
/// parse it. On parse failure, render the diagnostics to stderr and return
/// [`AlreadyReported`]. Every command that reads a programme goes through
/// here, so they all render alike.
pub(crate) fn parse_or_report(file: &Path) -> anyhow::Result<ParsedSource> {
    let source = read_source(file)?;
    match source.parse() {
        Ok((program, map)) => Ok(ParsedSource {
            program,
            map,
            source,
        }),
        Err(diagnostics) => {
            for d in &diagnostics {
                eprint!("{}", source.render(d, colour()));
            }
            Err(AlreadyReported.into())
        }
    }
}

/// Whether diagnostics on stderr are coloured: only on a terminal, and
/// never when `NO_COLOR` is set to anything but the empty string.
pub(crate) fn colour() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

/// Render one validation error to stderr: an ariadne caret block when
/// the source map places it, the plain `error: ...` line when it has
/// no source anchor (a generated discipline invariant, for one).
pub(crate) fn render_validation_error(err: &ValidationError, parsed: &ParsedSource) {
    match parsed.map.span_for_error(err) {
        Some(span) => eprint!(
            "{}",
            parsed
                .source
                .render(&Diagnostic::error(err.to_string(), span), colour())
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
/// [`PreparedProgram`]: transformation lookup, the analysis handle
/// ([`PreparedProgram::validated`]) and the rule slices in one object. For
/// commands that look transformations up by name.
pub(crate) fn prepare_or_report(parsed: &ParsedSource) -> anyhow::Result<PreparedProgram> {
    match PreparedProgram::new(parsed.program.clone()) {
        Ok(prepared) => Ok(prepared),
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
    prepared: &'a PreparedProgram,
    name: &str,
) -> anyhow::Result<&'a Transformation> {
    prepared
        .transformation(&TransformationName::from(name))
        .ok_or_else(|| {
            let available = prepared
                .program()
                .transformations
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow!("transformation `{name}` is not declared. Available: {available}")
        })
}

/// Open a PostgreSQL connection pool.
///
/// The error never includes the URL: it may carry a password, and stderr
/// ends up in logs. The `sqlx` error already says what went wrong.
/// Connect, and refuse a database this binary cannot serve before the
/// command's first query: none provisioned, ahead of the binary, or
/// behind it, each by name. Every database-backed command connects here except `init`
/// and `migrate`, which are what make a database current.
pub(crate) async fn connect(url: &str) -> anyhow::Result<PgPool> {
    let pool = connect_unchecked(url).await?;
    morpholog_postgres::require_current_schema(&pool).await?;
    Ok(pool)
}

pub(crate) async fn connect_unchecked(url: &str) -> anyhow::Result<PgPool> {
    // `postgres:///mydb` means "the OS user" to every other Postgres
    // tool; sqlx 0.9 alone reads it as `anonymous`.
    let defaulted = morpholog_postgres::with_connection_defaults(url);
    PgPool::connect(&defaulted)
        .await
        .map_err(|err| connect_failed(url, err))
}

/// `connect`, capped at one connection, for sessions. A lockstep session
/// never needs a second one, and the cap bounds load when many workers each
/// hold a session open. Checked the same way, so a session on a database
/// this binary cannot serve refuses before its ready line.
pub(crate) async fn connect_single(url: &str) -> anyhow::Result<PgPool> {
    let defaulted = morpholog_postgres::with_connection_defaults(url);
    let pool = morpholog_postgres::single_connection_pool(&defaulted)
        .await
        .map_err(|err| connect_failed(url, err))?;
    morpholog_postgres::require_current_schema(&pool).await?;
    Ok(pool)
}

/// The connection itself failed, before any query. Its own type, so a
/// command can tell it from every later failure without reading prose.
#[derive(Debug)]
pub(crate) struct ConnectFailed {
    source: Box<dyn std::error::Error + Send + Sync>,
    hint: Option<String>,
}

impl std::fmt::Display for ConnectFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("failed to connect to PostgreSQL")?;
        match &self.hint {
            Some(hint) => write!(f, " ({hint})"),
            None => Ok(()),
        }
    }
}

impl std::error::Error for ConnectFailed {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

/// `url` is the value as given. It is echoed only when it is a bare
/// database name, which cannot carry a password.
pub(crate) fn connect_failed(
    url: &str,
    err: impl Into<Box<dyn std::error::Error + Send + Sync>>,
) -> anyhow::Error {
    let bare_name = !url.is_empty()
        && url
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '$'));
    let hint = if url.contains("://") {
        None
    } else if bare_name {
        Some(format!(
            "`{url}` is a database name, not a URL: did you mean `postgres:///{url}`?"
        ))
    } else {
        Some("the value is not a URL: give one such as `postgres:///DBNAME`".to_string())
    };
    anyhow::Error::new(ConnectFailed {
        source: err.into(),
        hint,
    })
}

/// The published code for a failure met while connecting and asking
/// whether this binary serves the database. Call it only on an error from
/// that step: a later failure has no code, whatever its type.
pub(crate) fn preflight_code(err: &anyhow::Error) -> Option<PreflightCode> {
    use morpholog_postgres::PgError;
    if err.is::<ConnectFailed>() {
        return Some(PreflightCode::ConnectFailed);
    }
    match err.downcast_ref::<PgError>()? {
        PgError::SchemaMissing => Some(PreflightCode::NotInitialised),
        PgError::SchemaBelowBaseline { .. } => Some(PreflightCode::SchemaBelowBaseline),
        PgError::SchemaAhead { .. } => Some(PreflightCode::SchemaAhead),
        PgError::SchemaBehind { .. } => Some(PreflightCode::SchemaBehind),
        PgError::MigrationRecordAltered { .. } => Some(PreflightCode::SchemaRecordAltered),
        _ => None,
    }
}

/// Print the coded `preflight_error` for a failure of the preflight step,
/// when the command speaks JSON and the failure has a code, and hand the
/// error back for `main` to print as prose and exit 1.
pub(crate) fn preflight_failed(json: bool, err: anyhow::Error) -> anyhow::Error {
    if json
        && let Some(code) = preflight_code(&err)
        && let Err(print) = print_json(&PreflightError::new(code, format!("{err:#}")))
    {
        return print;
    }
    err
}

/// Connect, and ask whether this binary serves the database, reporting a
/// failure of either as [`preflight_failed`] does.
pub(crate) async fn connect_or_report(url: &str, json: bool) -> anyhow::Result<PgPool> {
    connect(url)
        .await
        .map_err(|err| preflight_failed(json, err))
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
        writeln!(out::stdout(), "{}", prose())?;
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

/// Say on stderr that the deployment is open, on every run that reads it,
/// so a deployment nobody closed is never taken for a closed one; and
/// that a closed one's root login is no longer the role it was closed
/// under, before the root's first act is refused for it.
pub(crate) async fn warn_on_identity(
    pool: &PgPool,
) -> anyhow::Result<morpholog_postgres::DeploymentIdentity> {
    use morpholog_postgres::DeploymentIdentity;
    let identity = morpholog_postgres::deployment_identity(pool).await?;
    match &identity {
        DeploymentIdentity::Open => eprintln!(
            "note: this deployment is open: any login may assert an actor nobody \
             restricted. Close it, with its writers stopped, by \
             `morpholog init --skip-if-exists --root-actor ACTOR --root-login LOGIN`."
        ),
        DeploymentIdentity::Closed {
            root_actor,
            root_login,
            root_login_matches: false,
        } => eprintln!(
            "warning: no role named `{root_login}` holds the OID this deployment was \
             closed under, so acts as the root actor `{}` are refused. A restore onto \
             another cluster gives roles new OIDs, and so does a role dropped and made \
             again. Once you have checked that `{root_login}` is the intended principal, \
             rebind it as the owner: UPDATE morpholog.deployment_identity SET \
             root_login_oid = (SELECT oid FROM pg_roles WHERE rolname = '{}'); see \
             docs/install.md, \"Restoring onto another cluster\"",
            root_actor.as_str(),
            root_login.replace('\'', "''"),
        ),
        DeploymentIdentity::Closed { .. } => {}
    }
    Ok(identity)
}

/// Warn on stderr when this deployment's roles also reach another
/// database: a login granted either role holds the other deployment too.
/// A warning, not a refusal, so an affected deployment still upgrades.
pub(crate) async fn warn_if_roles_shared(
    pool: &PgPool,
    roles: &morpholog_postgres::DeploymentRoles,
) -> anyhow::Result<()> {
    let others = morpholog_postgres::databases_also_reached(pool, roles).await?;
    if !others.is_empty() {
        let members = morpholog_postgres::direct_members(pool, roles).await?;
        let mut held = vec![
            members_of(roles.writer(), &members.writer),
            members_of(roles.reader(), &members.reader),
        ];
        held.extend(
            roles
                .executor()
                .map(|executor| members_of(executor, &members.executor)),
        );
        eprintln!(
            "warning: this deployment's roles {} also hold privileges in {}; a login \
             granted any of them can act there too. Direct members: {}. A copy made with \
             `createdb -T` holds its source's grants, so this is expected on a rehearsal \
             copy; drop the copy before migrating the live database. Otherwise, review each \
             member's effective access, and give each deployment its own roles with \
             `morpholog roles move --role-prefix <prefix> --login <login>`: see \
             docs/install.md, \"Moving a deployment to its own roles\"",
            morpholog_postgres::listed_roles(roles),
            others
                .iter()
                .map(|d| format!("`{d}`"))
                .collect::<Vec<_>>()
                .join(", "),
            held.join("; "),
        );
    }
    Ok(())
}

/// "`gm_human` (login) holds `morpholog_writer`", or that none does.
/// Direct membership only; what a member can do with it is not read.
fn members_of(role: &str, members: &[morpholog_postgres::RoleMember]) -> String {
    if members.is_empty() {
        return format!("none hold `{role}`");
    }
    let listed = members
        .iter()
        .map(|m| {
            if m.can_login {
                format!("`{}` (login)", m.member)
            } else {
                format!("`{}`", m.member)
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let verb = if members.len() == 1 { "holds" } else { "hold" };
    format!("{listed} {verb} `{role}`")
}

/// Pretty-print a value as JSON to stdout. The canonical output shape
/// for every read-only subcommand.
pub(crate) fn print_json<T: Serialize>(value: &T) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(value).context("JSON encoding failed")?;
    writeln!(out::stdout(), "{json}")?;
    Ok(())
}
