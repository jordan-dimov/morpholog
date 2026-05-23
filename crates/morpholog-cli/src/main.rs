//! Morpholog CLI.
//!
//! Subcommands:
//!
//! - `inspect` - read-only inspection of the durable substrate (claims,
//!   audit rows, pending outbox intents, derived-claim enumerations,
//!   declared predicate vocabulary). `inspect claims` and `inspect
//!   derived` accept an optional `--as-of <transition_id>` for
//!   historical reconstruction.
//! - `propose` - runs a named transformation from a built-in
//!   [`Program`] against a Morpholog PostgreSQL database, with
//!   arguments supplied as a JSON array of `EvalValue`s. JSON
//!   outcomes on stdout; exit codes distinguish commit, business
//!   rejection, and operational error.
//! - `parse` - read a `.morph` source file and print the parsed
//!   `Program` as JSON. No database connection.
//! - `check` - read a `.morph` source file, parse it, and run
//!   `Program::validate()` against the IR. Silent on clean input;
//!   uniform diagnostics on parse or validation failure.
//!
//! [`Program`]: morpholog_core::Program
//!
//! `propose` and `inspect` accept `--database-url <url>` or read
//! `DATABASE_URL` from the environment; if neither is supplied, clap
//! emits a clear error. Output is pretty-printed JSON via
//! `serde_json::to_string_pretty`.
//!
//! Explicit non-goals (today): no user-supplied program loading
//! (`propose` and `inspect derived` only accept built-in programs
//! from `morpholog_examples::all_programs()`), no outbox-delivery
//! worker, no filtering or pagination DSL, no materialised
//! derived-claim storage.
//!
//! Module layout: `main.rs` carries the `clap`-derived CLI structs
//! and the dispatch loop only. Each subcommand's logic lives in
//! `commands/<name>.rs` and is invoked via `commands::<name>::run`.

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use uuid::Uuid;

mod commands;

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
    Parse(SourceFileArgs),

    /// Parse and validate a `.morph` source file. Runs the parser
    /// first; if parsing succeeds, runs `Program::validate()` against
    /// the resulting IR (strict-arity check, duplicate-predicate
    /// check, predicate-reference resolution). Exits zero on a clean
    /// programme; exits one with diagnostics on either a parse
    /// failure or a validation failure. One command to answer "is
    /// this program well-formed?", with uniform output regardless of
    /// which layer raised the issue.
    Check(SourceFileArgs),

    /// Propose a transformation defined in a user-supplied `.morph`
    /// source file against a Morpholog PostgreSQL database. The
    /// non-built-in counterpart of `propose`: parses and validates
    /// the source file, then proposes the named transformation with
    /// the supplied actor and JSON args. Same JSON output and same
    /// exit-code semantics as `propose`.
    Run(RunArgs),

    /// Drive the outbox state machine from outside Rust. Lets a
    /// shell or Python deliverer participate in the lease protocol
    /// (`claim` to acquire a row, `complete` to resolve it,
    /// `release` to abandon it back to pending) without writing a
    /// `Deliverer` trait impl.
    Outbox {
        #[command(subcommand)]
        what: OutboxCmd,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum OutboxCmd {
    /// Claim the next pending row of the given intent type, leasing
    /// it for `--lease-seconds`. Output is `{"row": <OutboxRow>}` if
    /// claimed, `{"row": null}` if none are available. Exit 0 in
    /// both cases - empty outbox is normal, not an error.
    Claim(OutboxClaimArgs),

    /// Resolve a leased row: `delivered` marks it done, `transient`
    /// schedules another attempt after `--retry-after-seconds`,
    /// `failed` marks it failed (with optional `--reason`). Output is
    /// the `OutboxUpdate` JSON. Exit 1 on `LeaseLost`.
    Complete(OutboxCompleteArgs),

    /// Abandon a leased row, returning it to `pending` for another
    /// worker to claim. For graceful shutdown of an external
    /// deliverer that holds claims it can no longer service.
    Release(OutboxReleaseArgs),
}

#[derive(clap::Args, Debug)]
pub(crate) struct OutboxClaimArgs {
    /// Intent type to claim (e.g. `ClaimPaymentRequested`). Matches
    /// the predicate-style name a transformation emits via `emit X(...)`.
    #[arg(long)]
    pub(crate) intent_type: String,

    /// Lease duration in seconds. The claimed row's `lock_expires_at`
    /// is set to `now() + this`. If the caller does not call
    /// `complete` or `release` within the window, the row becomes
    /// reclaimable by another worker.
    #[arg(long, default_value_t = 30)]
    pub(crate) lease_seconds: u64,

    /// Worker identity. Defaults to a fresh UUIDv7 if not supplied;
    /// the generated id appears in the returned row's `locked_by`
    /// field so the caller can pass it back to `complete` / `release`.
    #[arg(long)]
    pub(crate) worker_id: Option<String>,

    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: String,
}

#[derive(clap::Args, Debug)]
pub(crate) struct OutboxCompleteArgs {
    /// Intent id of the leased row to resolve.
    pub(crate) intent_id: uuid::Uuid,

    /// Worker identity that holds the lease (returned by `claim` in
    /// the row's `locked_by` field).
    #[arg(long)]
    pub(crate) worker_id: String,

    /// Outcome to record. `delivered` marks the row done; `transient`
    /// schedules another attempt (requires `--retry-after-seconds`);
    /// `failed` marks the row failed (compensation, if configured,
    /// is the Rust worker's responsibility; the CLI does not invoke it).
    #[arg(long, value_enum)]
    pub(crate) outcome: OutboxCompleteOutcome,

    /// Seconds until the next attempt for `--outcome transient`.
    /// Internally converted to `now() + N seconds` for the row's
    /// `next_attempt_at`. Required for `transient`; an error for
    /// other outcomes.
    #[arg(long)]
    pub(crate) retry_after_seconds: Option<u64>,

    /// Optional human-readable narrative. Recorded as `failure_reason`
    /// for `--outcome failed`. For `--outcome transient` it is
    /// silently accepted but not yet persisted (the helper records
    /// the schedule, not the per-attempt reason - a future enhancement
    /// could carry it).
    #[arg(long)]
    pub(crate) reason: Option<String>,

    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: String,
}

#[derive(clap::ValueEnum, Debug, Clone, Copy)]
pub(crate) enum OutboxCompleteOutcome {
    Delivered,
    Transient,
    Failed,
}

#[derive(clap::Args, Debug)]
pub(crate) struct OutboxReleaseArgs {
    /// Intent id of the leased row to release.
    pub(crate) intent_id: uuid::Uuid,

    /// Worker identity that holds the lease.
    #[arg(long)]
    pub(crate) worker_id: String,

    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: String,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Inspect {
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
    /// List outbox rows, in enqueue order. Defaults to `--status pending`
    /// (the in-flight queue, matching the historical behaviour); use
    /// `--status all` for a full historical view, or any of
    /// `delivered|failed|in-progress` for a specific slice. `--as-of`
    /// does not apply: outbox is delivery state, not claim state.
    Outbox(InspectOutboxArgs),
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
pub(crate) struct InspectPredicatesArgs {
    /// Built-in program name (e.g. `double_entry_ledger`). The same
    /// registry that `propose` uses.
    pub(crate) program: String,
}

/// Arguments for `inspect claims`. Same shape as the shared
/// `InspectArgs` but with an optional `--as-of` for historical
/// claim listing.
#[derive(clap::Args, Debug)]
pub(crate) struct InspectClaimsArgs {
    /// PostgreSQL connection string. Falls back to the `DATABASE_URL`
    /// environment variable.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: String,

    /// Optional: list claims as they were at this past
    /// `transition_id` (UUIDv7). Without this flag, the current
    /// admitted claim set is returned. With it, the adapter replays
    /// the audit log up to the named transition and returns the
    /// historical claim set. Unknown ids return an error
    /// (`TransitionNotFound`).
    #[arg(long)]
    pub(crate) as_of: Option<Uuid>,
}

/// Arguments for `inspect derived`.
#[derive(clap::Args, Debug)]
pub(crate) struct InspectDerivedArgs {
    /// Built-in program name (e.g. `double_entry_ledger`). The same
    /// registry that `propose` uses.
    pub(crate) program: String,

    /// Derived claim predicate name (e.g. `TrialBalanceRow`). Looked
    /// up against the program's `derived_claims` by `predicate`.
    pub(crate) derived: String,

    /// PostgreSQL connection string. Falls back to the `DATABASE_URL`
    /// environment variable.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: String,

    /// Optional: enumerate the derived claim against the state at
    /// this past `transition_id` (UUIDv7) instead of current state.
    /// Same predicate-scoped replay as the current-state version;
    /// unknown ids return `TransitionNotFound`.
    #[arg(long)]
    pub(crate) as_of: Option<Uuid>,
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
pub(crate) struct InspectArgs {
    /// PostgreSQL connection string. Falls back to the `DATABASE_URL`
    /// environment variable.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: String,
}

/// Arguments for `inspect outbox`. Carries the same connection-string
/// flag as [`InspectArgs`] plus the status and intent-type filters.
#[derive(clap::Args, Debug)]
pub(crate) struct InspectOutboxArgs {
    /// Filter by row status. Default `pending` matches the
    /// operationally common question "what is waiting?". `all` returns
    /// every row regardless of status.
    #[arg(long, value_enum, default_value_t = InspectOutboxStatus::Pending)]
    pub(crate) status: InspectOutboxStatus,

    /// Filter by intent type. Optional; omitting returns rows of every
    /// intent type matching the status filter.
    #[arg(long)]
    pub(crate) intent_type: Option<String>,

    /// PostgreSQL connection string. Falls back to the `DATABASE_URL`
    /// environment variable.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: String,
}

/// Status filter for `inspect outbox`. The first four map directly to
/// the database's `status` column; `All` disables the status filter.
#[derive(clap::ValueEnum, Debug, Clone, Copy)]
pub(crate) enum InspectOutboxStatus {
    Pending,
    InProgress,
    Delivered,
    Failed,
    All,
}

impl InspectOutboxStatus {
    /// Database status string, or `None` for the `All` filter (which
    /// drops the `WHERE status = ?` clause).
    pub(crate) fn db_filter(self) -> Option<&'static str> {
        match self {
            InspectOutboxStatus::Pending => Some("pending"),
            InspectOutboxStatus::InProgress => Some("in_progress"),
            InspectOutboxStatus::Delivered => Some("delivered"),
            InspectOutboxStatus::Failed => Some("failed"),
            InspectOutboxStatus::All => None,
        }
    }
}

/// Arguments for any subcommand whose only input is a `.morph` source
/// file (today: `parse` and `check`). No database connection; these
/// subcommands are pure source-to-IR pipelines.
#[derive(clap::Args, Debug)]
pub(crate) struct SourceFileArgs {
    /// Path to a `.morph` source file.
    pub(crate) file: PathBuf,
}

/// Arguments for the `propose` subcommand.
#[derive(clap::Args, Debug)]
pub(crate) struct ProposeArgs {
    /// Built-in program name (e.g. `double_entry_ledger`). The full
    /// list is in the per-example READMEs under `examples/`.
    pub(crate) program: String,

    /// Transformation name within the program (e.g. `post_simple_entry`).
    /// The per-example README documents each transformation's parameters
    /// and the expected argument shape.
    pub(crate) transformation: String,

    /// JSON array of arguments matching the transformation's parameter
    /// list. Each element must be an `EvalValue` in the codec's tagged
    /// form: `{"type":"subject","value":"..."}`, `{"type":"decimal",
    /// "value":"100"}`, `{"type":"bool","value":true}`, or
    /// `{"type":"collection","value":[...]}`. See `examples/<n>/README.md`
    /// for the expected shape of each transformation's argument list.
    #[arg(long)]
    pub(crate) args: String,

    /// Subject value identifying the actor under whose authority this
    /// transition is being proposed. Free-form subject string (e.g.
    /// `jordan`, `user:jordan`, `desk:fx_spot`); the CLI wraps it as
    /// an `EvalValue::Subject`. Persisted to `morpholog.audit.actor`
    /// on commit. Required: every transition carries an actor.
    #[arg(long)]
    pub(crate) actor: String,

    /// PostgreSQL connection string. Falls back to the `DATABASE_URL`
    /// environment variable.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: String,

    /// When set, emit a structured per-statement trace alongside the
    /// outcome. Output shape becomes `{"result": <PgProposalOutcome>,
    /// "trace": [<TraceEntry>...]}` on commit or rejection. Useful
    /// for diagnosing why a transformation rejected: the trace shows
    /// which require/bind fired, what bindings each statement produced,
    /// and which invariant (if any) failed. Kernel errors at the PG
    /// boundary still surface via the normal anyhow error chain on
    /// stderr.
    #[arg(long)]
    pub(crate) trace: bool,
}

/// Arguments for the `run` subcommand. The `propose`-shaped fields
/// (`transformation`, `args`, `actor`, `database_url`, `trace`)
/// match `propose` exactly; the difference is `file` (a path to a
/// user-supplied `.morph` source) in place of `propose`'s `program`
/// (a built-in registry name).
#[derive(clap::Args, Debug)]
pub(crate) struct RunArgs {
    /// Path to a `.morph` source file containing the programme.
    pub(crate) file: PathBuf,

    /// Transformation name within the parsed programme.
    pub(crate) transformation: String,

    /// JSON array of arguments matching the transformation's parameter
    /// list. Each element must be an `EvalValue` in the codec's tagged
    /// form: `{"type":"subject","value":"..."}`, `{"type":"decimal",
    /// "value":"100"}`, `{"type":"bool","value":true}`, or
    /// `{"type":"collection","value":[...]}`.
    #[arg(long)]
    pub(crate) args: String,

    /// Subject value identifying the actor under whose authority this
    /// transition is being proposed. Free-form subject string; the
    /// CLI wraps it as an `EvalValue::Subject` and persists it to
    /// `morpholog.audit.actor`.
    #[arg(long)]
    pub(crate) actor: String,

    /// PostgreSQL connection string. Falls back to the `DATABASE_URL`
    /// environment variable.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: String,

    /// When set, emit a structured per-statement trace alongside the
    /// outcome. Same shape as `propose --trace`.
    #[arg(long)]
    pub(crate) trace: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Inspect { what } => commands::inspect::run(what).await,
        Command::Propose(args) => commands::propose::run(args).await,
        Command::Parse(args) => commands::parse::run(args),
        Command::Check(args) => commands::check::run(args),
        Command::Run(args) => commands::run::run(args).await,
        Command::Outbox { what } => match what {
            OutboxCmd::Claim(args) => commands::outbox::claim(args).await,
            OutboxCmd::Complete(args) => commands::outbox::complete(args).await,
            OutboxCmd::Release(args) => commands::outbox::release(args).await,
        },
    }
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
            Inspect::Audit(args) => args.database_url,
            Inspect::Outbox(args) => args.database_url,
            Inspect::Derived(_) => {
                panic!("use the dedicated inspect-derived parse tests, not parsed_url")
            }
            Inspect::Predicates(_) => {
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
        use morpholog_core::{ClaimInstance, EvalValue};
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
