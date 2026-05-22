//! Morpholog CLI.
//!
//! v0 exposes two surfaces:
//!
//! - `inspect` dumps the durable substrate (current claims, audit
//!   rows, pending outbox intents) as JSON, and enumerates derived
//!   claims declared by a built-in program against the current state
//!   (`inspect derived <program> <name>`). Read-only.
//! - `propose` runs a named transformation from a built-in [`Program`]
//!   against a Morpholog PostgreSQL database, with arguments supplied
//!   as a JSON array of `EvalValue`s. Outcome is JSON on stdout, with
//!   exit codes that let scripts distinguish commit from business
//!   rejection from operational error.
//!
//! [`Program`]: morpholog_core::Program
//!
//! Both surfaces accept `--database-url <url>` or read `DATABASE_URL`
//! from the environment; if neither is supplied, clap emits a clear
//! error. Output is pretty-printed JSON via
//! `serde_json::to_string_pretty`.
//!
//! `inspect claims` and `inspect derived` both accept an optional
//! `--as-of <transition_id>` flag. With it, the inspection runs
//! against the state reconstructed from the audit log at that past
//! transition; without it, the current state is returned. `inspect
//! audit` and `inspect outbox` do not accept `--as-of` (audit IS
//! the chronological record; outbox is delivery state, not claim
//! state).
//!
//! The CLI is still deliberately narrow. Explicit non-goals: no
//! parser, no user-supplied program loading (`propose` and `inspect
//! derived` only accept built-in programs from
//! `morpholog_examples::all_programs()`), no outbox-delivery
//! worker, no filtering or pagination DSL, no materialised
//! derived-claim storage.

use anyhow::{Context, anyhow};
use clap::{Parser, Subcommand};
use morpholog_core::{EvalValue, Transition};
use morpholog_postgres::{
    PgPool, PgProposalOutcome, PgTracedOutcome, list_audit_rows, list_claims, list_claims_at,
    list_derived, list_derived_at, list_pending_outbox, propose_against_pg,
    propose_against_pg_with_trace,
};
use morpholog_surface::parse_program;
use serde::Serialize;
use std::path::PathBuf;
use uuid::Uuid;

/// Top-level Morpholog CLI.
#[derive(Parser, Debug)]
#[command(version, about = "Morpholog runtime CLI", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Inspect the durable state of a Morpholog database.
    Inspect {
        #[command(subcommand)]
        what: Inspect,
    },

    /// Propose a named transformation from a built-in program against a
    /// Morpholog PostgreSQL database. Arguments are supplied as a JSON
    /// array of `EvalValue`s. On commit, prints the outcome as JSON and
    /// exits zero. On business rejection (a `require` failing or an
    /// invariant violated on the candidate state), prints the rejection
    /// reason as JSON and exits one. On any other error (bad arguments,
    /// unknown program, connection failure, JSON encoding error),
    /// prints an error message to stderr and exits one via anyhow's
    /// default.
    Propose(ProposeArgs),

    /// Parse a `.morph` source file. On success, prints the parsed
    /// `Program` as JSON and exits zero. On parse failure, renders
    /// ariadne-formatted diagnostics to stderr and exits one.
    ///
    /// v0 recognises only the `program` header and `predicate`
    /// declarations. A `.morph` file containing invariants,
    /// transformations, or derived claims currently fails with a
    /// parse error - the parser requires the file to end after the
    /// last predicate declaration. Subsequent PRs expand the surface.
    Parse(ParseArgs),
}

#[derive(Subcommand, Debug)]
enum Inspect {
    /// List currently-admitted claims, or claims as they were at a
    /// past `transition_id` via `--as-of`.
    Claims(InspectClaimsArgs),
    /// List every committed audit row, in commit order. `--as-of`
    /// does not apply here: the audit table IS the chronological
    /// record. Callers who want a time-bounded audit view should
    /// query `morpholog.audit` directly with their own predicate -
    /// the same `(committed_at, transition_id) <= target` shape the
    /// adapter's `reconstruct_state_at` uses internally, not
    /// `transition_id <= T` alone (which can include or exclude the
    /// wrong rows when commit order and UUID order diverge under
    /// concurrent commits).
    Audit(InspectArgs),
    /// List every pending outbox intent, in enqueue order. `--as-of`
    /// does not apply: outbox is delivery state, not claim state.
    Outbox(InspectArgs),
    /// Enumerate a derived claim from a built-in program against the
    /// current state, or against the state at a past `transition_id`
    /// via `--as-of`. Read-only: no claims are written, no audit row
    /// is produced.
    Derived(InspectDerivedArgs),
    /// List the declared predicate vocabulary for a built-in
    /// program. Read-only: no database connection, no state. The
    /// declarations are static programme metadata - the same data
    /// `Program::validate` checks references against.
    Predicates(InspectPredicatesArgs),
}

/// Arguments for `inspect predicates`. No `--as-of`; predicate
/// declarations are programme metadata, not state.
#[derive(clap::Args, Debug)]
struct InspectPredicatesArgs {
    /// Built-in program name (e.g. `double_entry_ledger`). The same
    /// registry that `propose` uses.
    program: String,
}

/// Arguments for `inspect claims`. Same shape as the shared
/// `InspectArgs` but with an optional `--as-of` for historical
/// claim listing.
#[derive(clap::Args, Debug)]
struct InspectClaimsArgs {
    /// PostgreSQL connection string. Falls back to the `DATABASE_URL`
    /// environment variable.
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// Optional: list claims as they were at this past
    /// `transition_id` (UUIDv7). Without this flag, the current
    /// admitted claim set is returned. With it, the adapter replays
    /// the audit log up to the named transition and returns the
    /// historical claim set. Unknown ids return an error
    /// (`TransitionNotFound`).
    #[arg(long)]
    as_of: Option<Uuid>,
}

/// Arguments for `inspect derived`.
#[derive(clap::Args, Debug)]
struct InspectDerivedArgs {
    /// Built-in program name (e.g. `double_entry_ledger`). The same
    /// registry that `propose` uses.
    program: String,

    /// Derived claim predicate name (e.g. `TrialBalanceRow`). Looked
    /// up against the program's `derived_claims` by `predicate`.
    derived: String,

    /// PostgreSQL connection string. Falls back to the `DATABASE_URL`
    /// environment variable.
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// Optional: enumerate the derived claim against the state at
    /// this past `transition_id` (UUIDv7) instead of current state.
    /// Same predicate-scoped replay as the current-state version;
    /// unknown ids return `TransitionNotFound`.
    #[arg(long)]
    as_of: Option<Uuid>,
}

/// Shared arguments for the `inspect` subcommands that do NOT
/// accept `--as-of` (audit, outbox). `inspect claims` uses its own
/// `InspectClaimsArgs` to expose the optional flag.
///
/// Clap's `env` attribute falls back to the `DATABASE_URL` environment
/// variable when `--database-url` is not supplied. If neither is set,
/// clap emits a "required argument was not provided" error before any
/// async work happens.
#[derive(clap::Args, Debug)]
struct InspectArgs {
    /// PostgreSQL connection string. Falls back to the `DATABASE_URL`
    /// environment variable.
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,
}

/// Arguments for the `parse` subcommand. No database connection;
/// `parse` is a pure source-to-IR transformation.
#[derive(clap::Args, Debug)]
struct ParseArgs {
    /// Path to a `.morph` source file. The file is read in full,
    /// lexed, and parsed; success prints the resulting `Program` as
    /// JSON, failure renders ariadne-formatted diagnostics on stderr.
    file: PathBuf,
}

/// Arguments for the `propose` subcommand.
#[derive(clap::Args, Debug)]
struct ProposeArgs {
    /// Built-in program name (e.g. `double_entry_ledger`). The full
    /// list is in the per-example READMEs under `examples/`.
    program: String,

    /// Transformation name within the program (e.g. `post_simple_entry`).
    /// The per-example README documents each transformation's parameters
    /// and the expected argument shape.
    transformation: String,

    /// JSON array of arguments matching the transformation's parameter
    /// list. Each element must be an `EvalValue` in the codec's tagged
    /// form: `{"type":"subject","value":"..."}`, `{"type":"decimal",
    /// "value":"100"}`, `{"type":"bool","value":true}`, or
    /// `{"type":"collection","value":[...]}`. See `examples/<n>/README.md`
    /// for the expected shape of each transformation's argument list.
    #[arg(long)]
    args: String,

    /// Subject value identifying the actor under whose authority this
    /// transition is being proposed. Free-form subject string (e.g.
    /// `jordan`, `user:jordan`, `desk:fx_spot`); the CLI wraps it as
    /// an `EvalValue::Subject`. Persisted to `morpholog.audit.actor`
    /// on commit. Required: every transition carries an actor.
    #[arg(long)]
    actor: String,

    /// PostgreSQL connection string. Falls back to the `DATABASE_URL`
    /// environment variable.
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// When set, emit a structured per-statement trace alongside the
    /// outcome. Output shape becomes `{"result": <PgProposalOutcome>,
    /// "trace": [<TraceEntry>...]}` on commit or rejection. Useful
    /// for diagnosing why a transformation rejected: the trace shows
    /// which require/bind_one fired, what bindings each statement
    /// produced, and which invariant (if any) failed. Kernel errors
    /// at the PG boundary still surface via the normal anyhow error
    /// chain on stderr - PR D2's known limitation.
    #[arg(long)]
    trace: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Inspect { what } => match what {
            Inspect::Claims(args) => {
                let pool = connect(&args.database_url).await?;
                let claims = match args.as_of {
                    Some(tid) => list_claims_at(&pool, tid)
                        .await
                        .context("list_claims_at failed")?,
                    None => list_claims(&pool).await.context("list_claims failed")?,
                };
                print_json(&claims)?;
            }
            Inspect::Audit(args) => {
                let pool = connect(&args.database_url).await?;
                let rows = list_audit_rows(&pool)
                    .await
                    .context("list_audit_rows failed")?;
                print_json(&rows)?;
            }
            Inspect::Outbox(args) => {
                let pool = connect(&args.database_url).await?;
                let rows = list_pending_outbox(&pool)
                    .await
                    .context("list_pending_outbox failed")?;
                print_json(&rows)?;
            }
            Inspect::Derived(args) => {
                inspect_derived(args).await?;
            }
            Inspect::Predicates(args) => {
                inspect_predicates(args)?;
            }
        },
        Command::Propose(args) => {
            propose(args).await?;
        }
        Command::Parse(args) => {
            parse_subcommand(args)?;
        }
    }
    Ok(())
}

/// Run the `parse` subcommand. Reads the `.morph` file at the given
/// path, parses it, and either prints the resulting `Program` as
/// pretty JSON (on success) or renders each diagnostic to stderr via
/// ariadne (on failure, exiting non-zero).
///
/// `Program` does not derive `Serialize` directly today, so the CLI
/// emits a small projection (`{"name": ..., "predicates": [...]}`)
/// rather than the full IR. When the rest of the surface lands and
/// the IR types pick up `Serialize`, this can collapse to a direct
/// `print_json(&program)` call.
fn parse_subcommand(args: ParseArgs) -> anyhow::Result<()> {
    let source = std::fs::read_to_string(&args.file)
        .with_context(|| format!("read source file {}", args.file.display()))?;
    let source_name = args.file.display().to_string();

    match parse_program(&source) {
        Ok(program) => {
            // Invariant bodies are projected as rendered strings
            // because `Expr` doesn't derive `Serialize`. Predicate
            // declarations carry `Serialize` and roundtrip into
            // structured JSON as before. When the kernel IR picks
            // up `Serialize` (probably alongside the formatter
            // doctrine work in P3+), this collapses to a direct
            // `print_json(&program)`.
            let invariants_payload: Vec<serde_json::Value> = program
                .invariants
                .iter()
                .map(|inv| {
                    // Explicit `&` refs to silence Copilot's
                    // conservative borrow analysis; the `json!`
                    // macro already borrows internally, but
                    // surfacing the borrow keeps reviews quiet.
                    serde_json::json!({
                        "name": &inv.name,
                        "version": inv.version,
                        "body": morpholog_core::format::format_expr_inline(&inv.body),
                    })
                })
                .collect();
            // Transformation bodies are projected as rendered
            // strings for the same reason as invariant bodies:
            // `Stmt` and `Expr` don't yet derive `Serialize`.
            // Each statement is rendered via `format_stmt(s, 0)`;
            // P3b1 statements (require / bind / let) produce a
            // single line each. `Stmt::For` (P3b2) produces
            // multi-line output via embedded newlines, which
            // will appear as a single JSON string with `\n`s
            // when it lands - revisit if a stricter "one-line-per-
            // entry" projection is needed.
            let transformations_payload: Vec<serde_json::Value> = program
                .transformations
                .iter()
                .map(|t| {
                    let body_lines: Vec<String> = t
                        .body
                        .iter()
                        .map(|s| morpholog_core::format::format_stmt(s, 0))
                        .collect();
                    serde_json::json!({
                        "name": &t.name,
                        "parameters": &t.parameters,
                        "body": body_lines,
                    })
                })
                .collect();
            let payload = serde_json::json!({
                "name": program.name,
                "predicates": program.predicates,
                "invariants": invariants_payload,
                "transformations": transformations_payload,
            });
            print_json(&payload)?;
            Ok(())
        }
        Err(diagnostics) => {
            for d in &diagnostics {
                eprint!("{}", d.render(&source_name, &source));
            }
            std::process::exit(1);
        }
    }
}

/// Run the `propose` subcommand end-to-end:
/// look up the named program and transformation, parse the JSON `--args`
/// as a `Vec<EvalValue>`, open a SERIALIZABLE PostgreSQL transaction
/// via `propose_against_pg`, and print the outcome as JSON.
///
/// Exit-code semantics:
/// - The outcome is *always* printed to stdout as JSON, both on commit
///   and on business rejection. Callers can distinguish by reading the
///   `"status"` field.
/// - On `Committed`, the function returns `Ok(())` and the process
///   exits 0 via `main`'s default.
/// - On `Rejected`, the function calls `std::process::exit(1)` after
///   printing, so scripts can detect business rejection without parsing
///   JSON.
/// - On any earlier error (unknown program/transformation, malformed
///   `--args` JSON, connection failure), the function returns `Err`
///   and anyhow's default exit path prints the error chain to stderr
///   and exits 1. Stdout vs stderr distinguishes the two failure
///   modes; in both cases the exit code is 1.
async fn propose(args: ProposeArgs) -> anyhow::Result<()> {
    // 1. Resolve the program from the built-in registry.
    let programs = morpholog_examples::all_programs();
    let program = programs
        .iter()
        .find(|p| p.name == args.program)
        .ok_or_else(|| {
            let available = programs
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow!(
                "program `{}` not found. Available built-in programs: {}",
                args.program,
                available
            )
        })?;

    // 2. Resolve the transformation within the program.
    let transformation = program
        .transformation(&args.transformation)
        .ok_or_else(|| {
            let available = program
                .transformations
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow!(
                "transformation `{}` not found in program `{}`. Available: {}",
                args.transformation,
                args.program,
                available
            )
        })?;

    // 3. Parse --args as a JSON array of EvalValues using the same
    //    tagged codec that `morpholog inspect` emits, so output of one
    //    invocation can plausibly be piped into the input of another.
    let eval_args: Vec<morpholog_core::EvalValue> = serde_json::from_str(&args.args).context(
        "failed to parse --args as a JSON array of EvalValues \
         (each element must be a tagged object such as \
         `{\"type\":\"subject\",\"value\":\"...\"}` or \
         `{\"type\":\"decimal\",\"value\":\"100\"}`)",
    )?;

    // 4. Connect and propose.
    //
    // Known v0 limitation: `PgError::SerializationFailure` (PostgreSQL
    // SSI conflict; SQLSTATE 40001) is documented as caller-retryable,
    // but the CLI surfaces it as a single failed invocation instead of
    // retrying internally. Acceptable for the developer/operator use
    // case the CLI is built for; concurrent `morpholog propose`
    // pipelines should add their own retry wrapper, or this code
    // should grow a small bounded-retry loop. Deferred until concurrent
    // CLI use is actually pressured.
    let pool = connect(&args.database_url).await?;
    let transition = Transition {
        transformation_name: transformation.name.clone(),
        args: eval_args,
        actor: EvalValue::Subject(args.actor.clone()),
    };
    // 5. Propose, with or without trace, and emit JSON accordingly.
    //    Trace branch emits `{"result": ..., "trace": [...]}` for
    //    every kernel-side outcome (Committed, Rejected, and
    //    Errored). Non-trace branch emits the bare outcome (its
    //    existing wire shape) so scripts that parse stdout don't
    //    break. PG-layer errors surface via anyhow on both branches.
    if args.trace {
        let traced =
            propose_against_pg_with_trace(&pool, transformation, &transition, &program.invariants)
                .await
                .context("propose_against_pg_with_trace failed")?;
        match traced {
            PgTracedOutcome::Outcome { outcome, trace } => {
                print_json(&serde_json::json!({
                    "result": &outcome,
                    "trace": &trace,
                }))?;
                if matches!(outcome, PgProposalOutcome::Rejected { .. }) {
                    std::process::exit(1);
                }
            }
            PgTracedOutcome::KernelErrored { error, trace } => {
                // Kernel error with structured trace preserved. Emit
                // a tagged "errored" result alongside the trace so
                // downstream JSON consumers can distinguish from a
                // lawful rejection, then exit non-zero.
                print_json(&serde_json::json!({
                    "result": {
                        "status": "errored",
                        "error": format!("{error}"),
                    },
                    "trace": &trace,
                }))?;
                std::process::exit(1);
            }
        }
    } else {
        let outcome = propose_against_pg(&pool, transformation, &transition, &program.invariants)
            .await
            .context("propose_against_pg failed")?;
        print_json(&outcome)?;
        if matches!(outcome, PgProposalOutcome::Rejected { .. }) {
            std::process::exit(1);
        }
    }
    Ok(())
}

/// Run the `inspect derived` subcommand end-to-end: look up the named
/// program and derived claim, connect, and enumerate the derived
/// extension against the current durable state via [`list_derived`].
///
/// Errors:
/// - Unknown program: surfaces the list of available built-in programs
///   in the error message.
/// - Unknown derived claim: surfaces the list of derived predicates
///   declared on the matched program.
/// - Connection failure or kernel error from `list_derived`: propagated
///   via anyhow context.
///
/// Output: pretty-printed JSON array of `ClaimInstance`s. The ordering
/// matches the kernel contract (sorted by `(keys ++ values)` under
/// structural `EvalValue` ordering), so the output is deterministic
/// for a given state.
async fn inspect_derived(args: InspectDerivedArgs) -> anyhow::Result<()> {
    let programs = morpholog_examples::all_programs();
    let program = programs
        .iter()
        .find(|p| p.name == args.program)
        .ok_or_else(|| {
            let available = programs
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow!(
                "program `{}` not found. Available built-in programs: {}",
                args.program,
                available
            )
        })?;

    let derived = program.derived_claim(&args.derived).ok_or_else(|| {
        let available = program
            .derived_claims
            .iter()
            .map(|d| d.predicate.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if available.is_empty() {
            anyhow!("program `{}` declares no derived claims", args.program)
        } else {
            anyhow!(
                "derived claim `{}` not found in program `{}`. Available: {}",
                args.derived,
                args.program,
                available
            )
        }
    })?;

    let pool = connect(&args.database_url).await?;
    let rows = match args.as_of {
        Some(tid) => list_derived_at(&pool, derived, tid)
            .await
            .context("list_derived_at failed")?,
        None => list_derived(&pool, derived)
            .await
            .context("list_derived failed")?,
    };
    print_json(&rows)?;
    Ok(())
}

async fn connect(url: &str) -> anyhow::Result<PgPool> {
    // The URL is deliberately NOT included in error context: a typical
    // PostgreSQL connection string is `postgres://user:password@host/db`,
    // and echoing it into stderr (where it may be captured by shells,
    // CI logs, or terminal scrollback) would leak credentials. The
    // underlying sqlx error already describes what went wrong (DNS
    // failure, connection refused, authentication failed, etc.); the
    // user knows which URL they supplied via `--database-url` or
    // `DATABASE_URL`.
    PgPool::connect(url)
        .await
        .context("failed to connect to PostgreSQL")
}

fn print_json<T: Serialize>(value: &T) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(value).context("JSON encoding failed")?;
    println!("{json}");
    Ok(())
}

/// Implement `inspect predicates <program>` end-to-end. Looks up the
/// program by name in the built-in registry, then prints its declared
/// predicates as JSON. Read-only and synchronous; no database
/// connection.
///
/// JSON shape (array of objects):
///
/// ```text
/// [
///   {
///     "name": "Policy",
///     "args": [
///       {"name": "policy_id", "kind": "Subject"},
///       {"name": "aggregate_limit", "kind": "Decimal"}
///     ]
///   }
/// ]
/// ```
///
/// `PredicateDecl` and `PredicateArgDecl` derive `Serialize` via the
/// kernel's existing serde derives; the order in the array matches
/// the order in `Program::predicates`.
fn inspect_predicates(args: InspectPredicatesArgs) -> anyhow::Result<()> {
    let programs = morpholog_examples::all_programs();
    let program = programs
        .iter()
        .find(|p| p.name == args.program)
        .ok_or_else(|| {
            let available = programs
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow!(
                "program `{}` not found. Available built-in programs: {}",
                args.program,
                available
            )
        })?;
    print_json(&program.predicates)
}

// ===========================================================================
// Tests - CLI argument parsing only.
//
// End-to-end CLI-against-PostgreSQL tests would duplicate the read-helper
// integration tests in morpholog-postgres without adding signal. These tests
// only verify that clap parses the expected command shapes correctly,
// catches missing required arguments, and threads the database URL through
// from either the flag or the environment fallback.
// ===========================================================================

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use clap::error::ErrorKind;

    /// Helper: parse the argv into our `Cli` and return the `database_url`
    /// that landed on the resulting `InspectArgs` (or
    /// `InspectClaimsArgs` for claims).
    fn parsed_url(argv: &[&str]) -> String {
        let cli = Cli::parse_from(argv);
        let Command::Inspect { what } = cli.command else {
            panic!("expected Command::Inspect, got {:?}", cli.command);
        };
        match what {
            Inspect::Claims(args) => args.database_url,
            Inspect::Audit(args) | Inspect::Outbox(args) => args.database_url,
            Inspect::Derived(_) => {
                // `Inspect::Derived` has its own arg struct; the other
                // helper tests cover it directly. Reaching this arm
                // here means a test passed `inspect derived` argv into
                // `parsed_url`, which is a test bug.
                panic!("use the dedicated inspect-derived parse tests, not parsed_url")
            }
            Inspect::Predicates(_) => {
                // `inspect predicates` takes no database URL. Same
                // reasoning as the Derived arm: this helper exists for
                // the URL-bearing subcommands only.
                panic!("inspect predicates does not take a database URL")
            }
        }
    }

    #[test]
    fn inspect_claims_with_flag_url_parses() {
        let url = parsed_url(&[
            "morpholog",
            "inspect",
            "claims",
            "--database-url",
            "postgres:///morpholog_dev",
        ]);
        assert_eq!(url, "postgres:///morpholog_dev");
    }

    #[test]
    fn inspect_audit_with_flag_url_parses() {
        let url = parsed_url(&[
            "morpholog",
            "inspect",
            "audit",
            "--database-url",
            "postgres://u:p@h/db",
        ]);
        assert_eq!(url, "postgres://u:p@h/db");
    }

    #[test]
    fn inspect_outbox_with_flag_url_parses() {
        let url = parsed_url(&[
            "morpholog",
            "inspect",
            "outbox",
            "--database-url",
            "postgres:///morpholog_dev",
        ]);
        assert_eq!(url, "postgres:///morpholog_dev");
    }

    /// `inspect claims` without `--as-of` parses to `as_of = None`.
    /// Pins that the optional flag is genuinely optional.
    #[test]
    fn inspect_claims_without_as_of_parses_to_none() {
        let cli = Cli::parse_from([
            "morpholog",
            "inspect",
            "claims",
            "--database-url",
            "postgres:///morpholog_dev",
        ]);
        let Command::Inspect {
            what: Inspect::Claims(args),
        } = cli.command
        else {
            panic!("expected Inspect::Claims, got {:?}", cli.command);
        };
        assert!(args.as_of.is_none(), "as_of must be None without the flag");
    }

    /// `inspect claims --as-of <uuid>` parses the UUID into the
    /// optional field.
    #[test]
    fn inspect_claims_with_as_of_parses_uuid() {
        let tid = "0192e000-0000-7000-8000-000000000001";
        let cli = Cli::parse_from([
            "morpholog",
            "inspect",
            "claims",
            "--database-url",
            "postgres:///morpholog_dev",
            "--as-of",
            tid,
        ]);
        let Command::Inspect {
            what: Inspect::Claims(args),
        } = cli.command
        else {
            panic!("expected Inspect::Claims, got {:?}", cli.command);
        };
        assert_eq!(
            args.as_of,
            Some(Uuid::parse_str(tid).unwrap()),
            "--as-of must parse into Some(Uuid)"
        );
    }

    /// `inspect claims --as-of <garbage>` is rejected by clap's
    /// `FromStr` parser before any async work happens.
    #[test]
    fn inspect_claims_with_bad_as_of_errors_at_parse_time() {
        let err = Cli::try_parse_from([
            "morpholog",
            "inspect",
            "claims",
            "--database-url",
            "postgres:///morpholog_dev",
            "--as-of",
            "not-a-uuid",
        ])
        .expect_err("bad UUID must surface a clap parse error");
        // clap classifies FromStr failures as ValueValidation in
        // recent versions; older versions used a different kind.
        // Accept either as a signal that parsing rejected the input.
        assert!(
            matches!(
                err.kind(),
                ErrorKind::ValueValidation | ErrorKind::InvalidValue
            ),
            "expected a value-validation/invalid-value error, got {:?}",
            err.kind()
        );
    }

    /// `inspect derived --as-of <uuid>` parses the optional flag.
    #[test]
    fn inspect_derived_with_as_of_parses_uuid() {
        let tid = "0192e000-0000-7000-8000-000000000002";
        let cli = Cli::parse_from([
            "morpholog",
            "inspect",
            "derived",
            "double_entry_ledger",
            "TrialBalanceRow",
            "--database-url",
            "postgres:///morpholog_dev",
            "--as-of",
            tid,
        ]);
        let Command::Inspect {
            what: Inspect::Derived(args),
        } = cli.command
        else {
            panic!("expected Inspect::Derived, got {:?}", cli.command);
        };
        assert_eq!(args.as_of, Some(Uuid::parse_str(tid).unwrap()));
    }

    /// `inspect audit --as-of <uuid>` is rejected by clap because
    /// `Inspect::Audit` uses `InspectArgs`, which does not declare
    /// the `--as-of` flag. Pins the design decision that as-of does
    /// not apply to the audit subcommand.
    #[test]
    fn inspect_audit_rejects_as_of_flag() {
        let err = Cli::try_parse_from([
            "morpholog",
            "inspect",
            "audit",
            "--database-url",
            "postgres:///morpholog_dev",
            "--as-of",
            "0192e000-0000-7000-8000-000000000001",
        ])
        .expect_err("inspect audit must not accept --as-of");
        assert_eq!(err.kind(), ErrorKind::UnknownArgument);
    }

    /// Same for `inspect outbox`.
    #[test]
    fn inspect_outbox_rejects_as_of_flag() {
        let err = Cli::try_parse_from([
            "morpholog",
            "inspect",
            "outbox",
            "--database-url",
            "postgres:///morpholog_dev",
            "--as-of",
            "0192e000-0000-7000-8000-000000000001",
        ])
        .expect_err("inspect outbox must not accept --as-of");
        assert_eq!(err.kind(), ErrorKind::UnknownArgument);
    }

    /// Sanity-check that `Cli::try_parse_from` *can* surface a
    /// `MissingRequiredArgument` error - without actually mutating the
    /// process environment (which would require `unsafe` in edition
    /// 2024 and the workspace forbids it). We trigger the error by
    /// omitting the subcommand entirely, which is unambiguously
    /// missing regardless of any `DATABASE_URL` value in the test
    /// process. The "no `--database-url` and no env" failure mode is a
    /// clap library guarantee (any `#[arg(env = "X")]` field with no
    /// default falls back to env, and errors if neither is supplied)
    /// and is not re-proven here.
    #[test]
    fn propose_with_all_args_parses() {
        let cli = Cli::parse_from([
            "morpholog",
            "propose",
            "double_entry_ledger",
            "post_simple_entry",
            "--args",
            "[]",
            "--actor",
            "jordan",
            "--database-url",
            "postgres:///morpholog_dev",
        ]);
        let Command::Propose(args) = cli.command else {
            panic!("expected Propose, got {:?}", cli.command);
        };
        assert_eq!(args.program, "double_entry_ledger");
        assert_eq!(args.transformation, "post_simple_entry");
        assert_eq!(args.args, "[]");
        assert_eq!(args.actor, "jordan");
        assert_eq!(args.database_url, "postgres:///morpholog_dev");
    }

    #[test]
    fn propose_missing_args_flag_errors() {
        let err = Cli::try_parse_from([
            "morpholog",
            "propose",
            "double_entry_ledger",
            "post_simple_entry",
            "--actor",
            "jordan",
            "--database-url",
            "postgres:///morpholog_dev",
        ])
        .expect_err("missing --args should error");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn propose_missing_actor_flag_errors() {
        let err = Cli::try_parse_from([
            "morpholog",
            "propose",
            "double_entry_ledger",
            "post_simple_entry",
            "--args",
            "[]",
            "--database-url",
            "postgres:///morpholog_dev",
        ])
        .expect_err("missing --actor should error");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn propose_missing_positional_errors() {
        let err = Cli::try_parse_from([
            "morpholog",
            "propose",
            "double_entry_ledger",
            // missing transformation positional
            "--args",
            "[]",
            "--actor",
            "jordan",
            "--database-url",
            "postgres:///morpholog_dev",
        ])
        .expect_err("missing positional should error");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn inspect_derived_with_all_args_parses() {
        let cli = Cli::parse_from([
            "morpholog",
            "inspect",
            "derived",
            "double_entry_ledger",
            "TrialBalanceRow",
            "--database-url",
            "postgres:///morpholog_dev",
        ]);
        let Command::Inspect { what } = cli.command else {
            panic!("expected Inspect, got {:?}", cli.command);
        };
        let Inspect::Derived(args) = what else {
            panic!("expected Inspect::Derived, got {what:?}");
        };
        assert_eq!(args.program, "double_entry_ledger");
        assert_eq!(args.derived, "TrialBalanceRow");
        assert_eq!(args.database_url, "postgres:///morpholog_dev");
    }

    #[test]
    fn inspect_derived_missing_derived_name_errors() {
        // Two positionals are required (program + derived name). Omit
        // the derived name; clap must surface MissingRequiredArgument
        // rather than silently taking the flag as the missing arg.
        let err = Cli::try_parse_from([
            "morpholog",
            "inspect",
            "derived",
            "double_entry_ledger",
            "--database-url",
            "postgres:///morpholog_dev",
        ])
        .expect_err("missing derived positional should error");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    /// `inspect predicates <program>` parses to `Inspect::Predicates`
    /// with the program name on the args struct. No `--database-url`
    /// flag: predicate declarations are programme metadata, not state.
    #[test]
    fn inspect_predicates_parses_with_program_argument() {
        let cli = Cli::parse_from([
            "morpholog",
            "inspect",
            "predicates",
            "clinical_trial_enrolment",
        ]);
        let Command::Inspect { what } = cli.command else {
            panic!("expected Inspect, got {:?}", cli.command);
        };
        let Inspect::Predicates(args) = what else {
            panic!("expected Inspect::Predicates, got {what:?}");
        };
        assert_eq!(args.program, "clinical_trial_enrolment");
    }

    /// Omitting the program positional must produce a clap
    /// MissingRequiredArgument error - the program name is required
    /// for the subcommand to identify which programme's vocabulary
    /// to render.
    #[test]
    fn inspect_predicates_missing_program_errors() {
        let err = Cli::try_parse_from(["morpholog", "inspect", "predicates"])
            .expect_err("missing program positional should error");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    /// `propose --trace` parses to a `ProposeArgs` with `trace: true`.
    /// All other propose-subcommand fields keep their existing
    /// behaviour.
    #[test]
    fn propose_with_trace_flag_parses() {
        let cli = Cli::parse_from([
            "morpholog",
            "propose",
            "settlement_netting",
            "create_net_settlement",
            "--actor",
            "jordan",
            "--args",
            "[]",
            "--database-url",
            "postgres:///morpholog_dev",
            "--trace",
        ]);
        let Command::Propose(args) = cli.command else {
            panic!("expected Propose, got {:?}", cli.command);
        };
        assert!(args.trace, "expected trace flag to be set");
        assert_eq!(args.program, "settlement_netting");
        assert_eq!(args.transformation, "create_net_settlement");
        assert_eq!(args.actor, "jordan");
    }

    /// Without `--trace`, `ProposeArgs.trace` defaults to false. The
    /// non-trace propose path must not be affected by the new flag.
    #[test]
    fn propose_without_trace_flag_defaults_to_false() {
        let cli = Cli::parse_from([
            "morpholog",
            "propose",
            "settlement_netting",
            "create_net_settlement",
            "--actor",
            "jordan",
            "--args",
            "[]",
            "--database-url",
            "postgres:///morpholog_dev",
        ]);
        let Command::Propose(args) = cli.command else {
            panic!("expected Propose, got {:?}", cli.command);
        };
        assert!(!args.trace, "expected trace flag to default to false");
    }

    #[test]
    fn propose_outcome_serialises_with_status_tag() {
        // Pin the JSON wire shape that the CLI emits for outcomes.
        // The codec uses a `status` discriminant via serde's tagged-enum
        // representation; the CLI relies on this so that scripts can
        // parse stdout and branch on `.status`.
        use morpholog_core::ClaimInstance;
        use morpholog_postgres::PgProposalOutcome;
        use uuid::Uuid;

        let committed = PgProposalOutcome::Committed {
            transition_id: Uuid::nil(),
            actor: EvalValue::Subject("jordan".to_string()),
            asserted_claims: vec![ClaimInstance {
                predicate: "Foo".to_string(),
                args: vec![],
            }],
            retracted_claims: vec![],
            emitted_intents: vec![],
        };
        let json = serde_json::to_string(&committed).unwrap();
        assert!(
            json.contains(r#""status":"committed""#),
            "committed outcome must carry status=committed, got: {json}"
        );
        assert!(json.contains(r#""transition_id":"00000000-0000-0000-0000-000000000000""#));
        assert!(
            json.contains(r#""actor":{"type":"subject","value":"jordan"}"#),
            "committed outcome must carry actor on the wire, got: {json}"
        );

        let rejected = PgProposalOutcome::Rejected {
            reason: "require failed".to_string(),
        };
        let json = serde_json::to_string(&rejected).unwrap();
        assert!(
            json.contains(r#""status":"rejected""#),
            "rejected outcome must carry status=rejected, got: {json}"
        );
        assert!(json.contains(r#""reason":"require failed""#));
    }

    #[test]
    fn missing_required_argument_surfaces_as_clap_error() {
        let err = Cli::try_parse_from(["morpholog"]).expect_err("no subcommand should error");
        // Either MissingRequiredArgument or MissingSubcommand depending
        // on clap version; both are acceptable signals of "you didn't
        // give me enough to act on."
        // clap 4 surfaces this as DisplayHelpOnMissingArgumentOrSubcommand
        // (which auto-prints help) rather than the explicit
        // MissingSubcommand kind. Older clap versions used the latter; we
        // accept either so the test does not break under minor clap
        // updates.
        assert!(
            matches!(
                err.kind(),
                ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                    | ErrorKind::MissingRequiredArgument
                    | ErrorKind::MissingSubcommand
            ),
            "expected a missing-argument/subcommand error, got {:?}",
            err.kind()
        );
    }

    #[test]
    fn parse_with_file_argument_parses() {
        let cli = Cli::try_parse_from(["morpholog", "parse", "demo.morph"]).unwrap();
        let Command::Parse(args) = cli.command else {
            panic!("expected Command::Parse, got {:?}", cli.command);
        };
        assert_eq!(args.file.as_os_str(), "demo.morph");
    }

    #[test]
    fn parse_missing_file_argument_errors() {
        let err =
            Cli::try_parse_from(["morpholog", "parse"]).expect_err("expected clap parse error");
        assert!(
            matches!(err.kind(), ErrorKind::MissingRequiredArgument),
            "expected missing-argument error, got {:?}",
            err.kind()
        );
    }
}
