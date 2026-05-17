//! Morpholog CLI.
//!
//! v0 exposes two surfaces:
//!
//! - `inspect` dumps the durable substrate (current claims, audit
//!   rows, pending outbox intents) as JSON. Read-only.
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
//! The CLI is still deliberately narrow. Explicit non-goals: no
//! parser, no user-supplied program loading (`propose` only accepts
//! built-in programs from `morpholog_core::examples::all_programs()`),
//! no outbox-delivery worker, no filtering or pagination DSL, no
//! as-of evaluation, no derived-claim machinery.

use anyhow::{Context, anyhow};
use clap::{Parser, Subcommand};
use morpholog_postgres::{
    PgPool, PgProposalOutcome, list_audit_rows, list_claims, list_pending_outbox,
    propose_against_pg,
};
use serde::Serialize;

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
}

#[derive(Subcommand, Debug)]
enum Inspect {
    /// List every currently-admitted claim.
    Claims(InspectArgs),
    /// List every committed audit row, in commit order.
    Audit(InspectArgs),
    /// List every pending outbox intent, in enqueue order.
    Outbox(InspectArgs),
}

/// Shared arguments for every `inspect` subcommand.
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

    /// PostgreSQL connection string. Falls back to the `DATABASE_URL`
    /// environment variable.
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Inspect { what } => match what {
            Inspect::Claims(args) => {
                let pool = connect(&args.database_url).await?;
                let claims = list_claims(&pool).await.context("list_claims failed")?;
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
        },
        Command::Propose(args) => {
            propose(args).await?;
        }
    }
    Ok(())
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
    let programs = morpholog_core::examples::all_programs();
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
    let outcome = propose_against_pg(&pool, transformation, eval_args, &program.invariants)
        .await
        .context("propose_against_pg failed")?;

    // 5. Print the outcome and translate Rejected into exit 1.
    print_json(&outcome)?;
    if matches!(outcome, PgProposalOutcome::Rejected { .. }) {
        std::process::exit(1);
    }
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

// ===========================================================================
// Tests — CLI argument parsing only.
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
    /// that landed on the resulting `InspectArgs`.
    fn parsed_url(argv: &[&str]) -> String {
        let cli = Cli::parse_from(argv);
        let Command::Inspect { what } = cli.command else {
            panic!("expected Command::Inspect, got {:?}", cli.command);
        };
        match what {
            Inspect::Claims(args) | Inspect::Audit(args) | Inspect::Outbox(args) => {
                args.database_url
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

    /// Sanity-check that `Cli::try_parse_from` *can* surface a
    /// `MissingRequiredArgument` error — without actually mutating the
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
            "--database-url",
            "postgres:///morpholog_dev",
        ]);
        let Command::Propose(args) = cli.command else {
            panic!("expected Propose, got {:?}", cli.command);
        };
        assert_eq!(args.program, "double_entry_ledger");
        assert_eq!(args.transformation, "post_simple_entry");
        assert_eq!(args.args, "[]");
        assert_eq!(args.database_url, "postgres:///morpholog_dev");
    }

    #[test]
    fn propose_missing_args_flag_errors() {
        let err = Cli::try_parse_from([
            "morpholog",
            "propose",
            "double_entry_ledger",
            "post_simple_entry",
            "--database-url",
            "postgres:///morpholog_dev",
        ])
        .expect_err("missing --args should error");
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
            "--database-url",
            "postgres:///morpholog_dev",
        ])
        .expect_err("missing positional should error");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
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
}
