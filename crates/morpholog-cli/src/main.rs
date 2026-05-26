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
//! - `check` - parse a `.morph` source file and run
//!   `Program::validate()` against the IR. Silent on clean input;
//!   uniform diagnostics on parse or validation failure.
//!
//! [`Program`]: morpholog_core::Program
//!
//! `propose` and `inspect` accept `--database-url <url>` or read
//! `DATABASE_URL` from the environment; if neither is supplied, clap
//! emits a clear error. Output is pretty-printed JSON.
//!
//! `main.rs` carries the `clap`-derived CLI structs and the dispatch
//! loop only; each subcommand's logic lives in `commands::<name>::run`.

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

    /// Parse and validate a `.morph` source file. If parsing succeeds,
    /// runs `Program::validate()` against the IR: declarations and arity
    /// for predicates and intents, kind/type compatibility, binding flow
    /// (unbound variables), expression shape, actor context, and a
    /// nesting-depth bound. Exits zero on a clean programme; exits one
    /// with uniform diagnostics on either a parse or a validation
    /// failure.
    Check(SourceFileArgs),

    /// Propose a transformation defined in a user-supplied `.morph`
    /// source file. The non-built-in counterpart of `propose`: parses
    /// and validates the source file, then proposes the named
    /// transformation with the supplied actor and JSON args. Same JSON
    /// output and exit-code semantics as `propose`.
    Run(RunArgs),

    /// Explain why a transformation from a `.morph` source file would be
    /// admitted or rejected against live state, without proposing it.
    /// Parses and validates the source like `run`, loads the scoped
    /// pre-state, then renders the structured explanation - the gate that
    /// failed and the directly-missing claims, the violated invariant, or
    /// admissibility - as claim-shaped prose, or as JSON with `--json`.
    /// Read-only: the verdict does not affect the exit code (zero on both
    /// admissible and rejected). Only operational failures - parse or
    /// validation errors, bad `--args`, an unknown transformation, a
    /// database failure - exit non-zero.
    Explain(ExplainArgs),

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
    /// Intent type to claim (e.g. `ClaimPaymentRequested`): the
    /// predicate-style name a transformation emits via `emit X(...)`.
    #[arg(long)]
    pub(crate) intent_type: String,

    /// Lease duration in seconds; sets `lock_expires_at` to `now() +
    /// this`. If the caller does not `complete` or `release` within the
    /// window, the row becomes reclaimable by another worker.
    #[arg(long, default_value_t = 30)]
    pub(crate) lease_seconds: u64,

    /// Worker identity. Defaults to a fresh UUIDv7; the generated id
    /// appears in the returned row's `locked_by` so the caller can pass
    /// it back to `complete` / `release`.
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
    /// `failed` marks it failed (compensation, if any, is the Rust
    /// worker's responsibility; the CLI does not invoke it).
    #[arg(long, value_enum)]
    pub(crate) outcome: OutboxCompleteOutcome,

    /// Seconds until the next attempt; sets the row's `next_attempt_at`
    /// to `now() + N seconds`. Required for `transient`; an error for
    /// other outcomes.
    #[arg(long)]
    pub(crate) retry_after_seconds: Option<u64>,

    /// Optional human-readable narrative. Recorded as `failure_reason`
    /// for `--outcome failed`. For `transient` it is accepted but not
    /// persisted (the helper records the schedule, not the per-attempt
    /// reason).
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
    /// List every committed audit row, in commit order. `--as-of` does
    /// not apply: the audit table IS the chronological record. For a
    /// time-bounded view, query `morpholog.audit` directly with the
    /// same `(committed_at, transition_id) <= target` shape
    /// `reconstruct_state_at` uses - not `transition_id <= T` alone,
    /// which selects the wrong rows when commit order and UUID order
    /// diverge under concurrent commits.
    Audit(InspectArgs),
    /// List outbox rows, in enqueue order. Defaults to `--status
    /// pending`; use `--status all` for a full view, or any of
    /// `delivered|failed|in-progress` for a specific slice. `--as-of`
    /// does not apply: outbox is delivery state, not claim state.
    Outbox(InspectOutboxArgs),
    /// Enumerate a derived claim from a built-in program against the
    /// current state, or against the state at a past `transition_id`
    /// via `--as-of`. Read-only: no claims are written, no audit row
    /// is produced.
    Derived(InspectDerivedArgs),
    /// List the declared predicate vocabulary for a built-in program.
    /// Read-only: no database connection, no state. The declarations
    /// are static programme metadata, the same data `Program::validate`
    /// checks references against.
    Predicates(InspectPredicatesArgs),
    /// Show the states a built-in program makes impossible - one entry
    /// per invariant, naming the forbidden state where it is
    /// mechanically obvious. Read-only and static: no database, no
    /// state. Prose by default; `--json` for the structured form.
    Guarantees(InspectGuaranteesArgs),
}

/// Arguments for `inspect predicates`. No `--as-of`; predicate
/// declarations are programme metadata, not state.
#[derive(clap::Args, Debug)]
pub(crate) struct InspectPredicatesArgs {
    /// Built-in program name (e.g. `double_entry_ledger`).
    pub(crate) program: String,
}

/// Arguments for `inspect guarantees`. Like `inspect predicates`, a
/// static read over a built-in program; `--json` switches the prose view
/// for the structured form.
#[derive(clap::Args, Debug)]
pub(crate) struct InspectGuaranteesArgs {
    /// Built-in program name (e.g. `carbon_credit_provenance`).
    pub(crate) program: String,
    /// Emit the structured JSON form instead of prose.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Arguments for `inspect claims`. Like the shared `InspectArgs` plus
/// an optional `--as-of` for historical claim listing.
#[derive(clap::Args, Debug)]
pub(crate) struct InspectClaimsArgs {
    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: String,

    /// Optional: list claims as they were at this past `transition_id`
    /// (UUIDv7). Without it, the current admitted claim set is
    /// returned; with it, the adapter replays the audit log up to the
    /// named transition. Unknown ids return `TransitionNotFound`.
    #[arg(long)]
    pub(crate) as_of: Option<Uuid>,
}

/// Arguments for `inspect derived`.
#[derive(clap::Args, Debug)]
pub(crate) struct InspectDerivedArgs {
    /// Built-in program name (e.g. `double_entry_ledger`).
    pub(crate) program: String,

    /// Derived claim predicate name (e.g. `TrialBalanceRow`). Looked
    /// up against the program's `derived_claims` by `predicate`.
    pub(crate) derived: String,

    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: String,

    /// Optional: enumerate against the state at this past
    /// `transition_id` (UUIDv7) instead of current state. Same
    /// predicate-scoped replay; unknown ids return `TransitionNotFound`.
    #[arg(long)]
    pub(crate) as_of: Option<Uuid>,
}

/// Shared arguments for the `inspect` subcommands that do NOT accept
/// `--as-of` (audit, outbox). `inspect claims` uses its own
/// `InspectClaimsArgs` to expose the optional flag.
///
/// The `env` attribute falls back to `DATABASE_URL`; if neither flag
/// nor env is set, clap errors before any async work happens.
#[derive(clap::Args, Debug)]
pub(crate) struct InspectArgs {
    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: String,
}

/// Arguments for `inspect outbox`: the connection-string flag plus the
/// status and intent-type filters.
#[derive(clap::Args, Debug)]
pub(crate) struct InspectOutboxArgs {
    /// Filter by row status. Default `pending` answers "what is
    /// waiting?"; `all` returns every row regardless of status.
    #[arg(long, value_enum, default_value_t = InspectOutboxStatus::Pending)]
    pub(crate) status: InspectOutboxStatus,

    /// Filter by intent type. Optional; omitting returns rows of every
    /// intent type matching the status filter.
    #[arg(long)]
    pub(crate) intent_type: Option<String>,

    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: String,
}

/// Status filter for `inspect outbox`. The named values map to the
/// database's `status` column; `All` disables the status filter.
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
/// file (`parse` and `check`). No database connection.
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
    /// Its parameters and expected argument shape are in the per-example
    /// README.
    pub(crate) transformation: String,

    /// JSON array of arguments matching the transformation's parameter
    /// list. Each element is an `EvalValue` in the codec's tagged form:
    /// `{"type":"subject","value":"..."}`, `{"type":"decimal",
    /// "value":"100"}`, `{"type":"bool","value":true}`, or
    /// `{"type":"collection","value":[...]}`.
    #[arg(long)]
    pub(crate) args: String,

    /// Subject identifying the actor under whose authority this
    /// transition is proposed. Free-form subject string (e.g. `jordan`,
    /// `desk:fx_spot`), wrapped as an `EvalValue::Subject` and persisted
    /// to `morpholog.audit.actor`. Required: every transition carries an
    /// actor.
    #[arg(long)]
    pub(crate) actor: String,

    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: String,

    /// When set, emit a structured per-statement trace alongside the
    /// outcome: `{"result": <PgProposalOutcome>, "trace":
    /// [<TraceEntry>...]}` on commit or rejection. The trace shows which
    /// require/bind fired, what bindings each statement produced, and
    /// which invariant (if any) failed. Kernel errors at the PG boundary
    /// still surface via the anyhow error chain on stderr.
    #[arg(long)]
    pub(crate) trace: bool,
}

/// Arguments for the `run` subcommand. The `propose`-shaped fields
/// match `propose` exactly; the difference is `file` (a user-supplied
/// `.morph` source) in place of `program` (a built-in registry name).
#[derive(clap::Args, Debug)]
pub(crate) struct RunArgs {
    /// Path to a `.morph` source file containing the programme.
    pub(crate) file: PathBuf,

    /// Transformation name within the parsed programme.
    pub(crate) transformation: String,

    /// JSON array of arguments matching the transformation's parameter
    /// list. Each element is an `EvalValue` in the codec's tagged form:
    /// `{"type":"subject","value":"..."}`, `{"type":"decimal",
    /// "value":"100"}`, `{"type":"bool","value":true}`, or
    /// `{"type":"collection","value":[...]}`.
    #[arg(long)]
    pub(crate) args: String,

    /// Subject identifying the actor under whose authority this
    /// transition is proposed. Wrapped as an `EvalValue::Subject` and
    /// persisted to `morpholog.audit.actor`.
    #[arg(long)]
    pub(crate) actor: String,

    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: String,

    /// When set, emit a structured per-statement trace alongside the
    /// outcome. Same shape as `propose --trace`.
    #[arg(long)]
    pub(crate) trace: bool,
}

/// Arguments for `explain`. The same source/transformation/args/actor
/// shape as [`RunArgs`] - it builds the identical `Transition` - but with
/// `--json` in place of `--trace`: explain's whole output already is the
/// interpreted trace, so prose-or-JSON is the only output choice.
#[derive(clap::Args, Debug)]
pub(crate) struct ExplainArgs {
    /// Path to a `.morph` source file containing the programme.
    pub(crate) file: PathBuf,

    /// Transformation name within the parsed programme.
    pub(crate) transformation: String,

    /// JSON array of arguments matching the transformation's parameter
    /// list, in the same tagged codec as `run --args` - e.g.
    /// `[{"type":"subject","value":"c1"},{"type":"decimal","value":"100"}]`.
    #[arg(long)]
    pub(crate) args: String,

    /// Subject identifying the actor under whose authority the explained
    /// transition is proposed. Wrapped as an `EvalValue::Subject`.
    #[arg(long)]
    pub(crate) actor: String,

    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: String,

    /// Emit the structured JSON `Explanation` instead of prose.
    #[arg(long)]
    pub(crate) json: bool,
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
        Command::Explain(args) => commands::explain::run(args).await,
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
// End-to-end CLI-against-PostgreSQL coverage lives in morpholog-postgres'
// read-helper integration tests; duplicating it here adds no signal. These
// tests verify that clap parses the expected command shapes, catches missing
// required arguments, and threads the database URL through from flag or env.
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
            Inspect::Guarantees(_) => {
                panic!("inspect guarantees does not take a database URL")
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
        // clap classifies FromStr failures as ValueValidation in recent
        // versions, InvalidValue in older ones; accept either.
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
        use morpholog_core::{ClaimInstance, Subject};
        use morpholog_postgres::PgProposalOutcome;
        use uuid::Uuid;

        let committed = PgProposalOutcome::Committed {
            transition_id: Uuid::nil(),
            actor: Subject::from("jordan"),
            asserted_claims: vec![ClaimInstance {
                predicate: "Foo".into(),
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
        // clap 4 surfaces this as DisplayHelpOnMissingArgumentOrSubcommand;
        // older versions used MissingSubcommand. Accept either so the
        // test does not break under minor clap updates.
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

    #[test]
    fn explain_with_all_args_parses() {
        let cli = Cli::parse_from([
            "morpholog",
            "explain",
            "model.morph",
            "issue_credit",
            "--args",
            "[]",
            "--actor",
            "jordan",
            "--database-url",
            "postgres:///morpholog_dev",
        ]);
        let Command::Explain(args) = cli.command else {
            panic!("expected Explain, got {:?}", cli.command);
        };
        assert_eq!(args.file.as_os_str(), "model.morph");
        assert_eq!(args.transformation, "issue_credit");
        assert_eq!(args.args, "[]");
        assert_eq!(args.actor, "jordan");
        assert_eq!(args.database_url, "postgres:///morpholog_dev");
        assert!(!args.json, "expected --json to default to false");
    }

    #[test]
    fn explain_with_json_flag_parses() {
        let cli = Cli::parse_from([
            "morpholog",
            "explain",
            "model.morph",
            "issue_credit",
            "--args",
            "[]",
            "--actor",
            "jordan",
            "--database-url",
            "postgres:///morpholog_dev",
            "--json",
        ]);
        let Command::Explain(args) = cli.command else {
            panic!("expected Explain, got {:?}", cli.command);
        };
        assert!(args.json, "expected --json flag to be set");
    }

    #[test]
    fn explain_missing_actor_flag_errors() {
        let err = Cli::try_parse_from([
            "morpholog",
            "explain",
            "model.morph",
            "issue_credit",
            "--args",
            "[]",
            "--database-url",
            "postgres:///morpholog_dev",
        ])
        .expect_err("missing --actor should error");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }
}
