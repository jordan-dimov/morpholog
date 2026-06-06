//! Morpholog CLI - the `morpholog` binary.
//!
//! `main.rs` carries the `clap`-derived CLI structs and the dispatch
//! loop only; each subcommand's logic lives in `commands::<name>::run`,
//! and each subcommand's contract is its doc comment on [`Command`]
//! (rendered by `--help`) - no parallel list here to drift from it.
//! The shared conventions:
//!
//! - Database-backed subcommands accept `--database-url <url>` or fall
//!   back to the `DATABASE_URL` environment variable; if neither is
//!   supplied, clap errors before any work happens.
//! - Results go to stdout (pretty-printed JSON, or prose where a
//!   subcommand documents it); diagnostics and operational errors go
//!   to stderr.
//! - Exit codes distinguish success, business rejection, and
//!   operational failure; each subcommand's doc comment states its
//!   own mapping.

use chrono::{DateTime, Utc};
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

    /// Parse a `.morph` source file. On success, prints the parsed
    /// `Program` as JSON and exits zero. On parse failure, renders
    /// ariadne-formatted diagnostics to stderr and exits one.
    Parse(SourceFileArgs),

    /// Parse and validate a `.morph` source file. If parsing succeeds,
    /// runs `Program::validate()` against the IR: declarations and arity
    /// for predicates and intents, kind/type compatibility, binding flow
    /// (unbound variables), expression shape, actor context, and a
    /// nesting-depth bound. Exits zero on a clean programme - silently by
    /// default, or with a one-screen summary of what validated under
    /// `--verbose`; exits one with uniform diagnostics on either a parse
    /// or a validation failure.
    Check(CheckArgs),

    /// Propose a transformation defined in a `.morph` source file against
    /// a Morpholog PostgreSQL database. Parses and validates the source,
    /// then proposes the named transformation with the supplied actor and
    /// a JSON array of `EvalValue` args. On commit, prints the outcome as
    /// JSON and exits zero; on business rejection, prints the reason and
    /// exits one; on any other error (bad args, unknown transformation,
    /// connection failure), prints to stderr and exits one.
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

    /// Emit a JSON Schema describing a named transformation's argument
    /// object, or (with `--intent <Type>`) an emitted intent's payload
    /// object. Thin wrapper over the library's `transformation_arg_schema`
    /// / `intent_arg_schema`: parse, validate, render. The schema is the
    /// public contract a non-Rust embedder uses to validate request
    /// bodies, generate input forms, decode an outbox payload by name, or
    /// derive typed client models without touching Rust. Output is a JSON
    /// Schema (Draft 2020-12); exits zero on success, non-zero on parse /
    /// validation failure or an unknown transformation / intent. No
    /// `--json` flag because the output IS JSON.
    Schema(SchemaArgs),

    /// Replay the audit log to its latest transition and compare the
    /// reconstructed state against the claims table. The two tables are
    /// independent records of the same history, so a difference is
    /// evidence that one was modified outside the runtime. Prints the
    /// outcome as JSON; consistent exits zero, divergent prints the
    /// claims each record holds that the other does not and exits one.
    /// Read-only; an empty database is trivially consistent.
    Verify(VerifyArgs),
}

/// Arguments for `verify`: just the connection string.
#[derive(clap::Args, Debug)]
pub(crate) struct VerifyArgs {
    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: String,
}

/// An `--as-of` coordinate: an exact `transition_id` (UUIDv7), or an
/// RFC 3339 timestamp resolved to the last transition committed at or
/// before that instant. Parsed at the clap layer so a malformed value
/// errors before any database work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AsOf {
    /// State immediately after this committed transition.
    Transition(Uuid),
    /// State at the last transition committed at or before this instant.
    AtOrBefore(DateTime<Utc>),
}

impl std::str::FromStr for AsOf {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(tid) = Uuid::parse_str(s) {
            return Ok(AsOf::Transition(tid));
        }
        if let Ok(at) = DateTime::parse_from_rfc3339(s) {
            return Ok(AsOf::AtOrBefore(at.with_timezone(&Utc)));
        }
        Err(format!(
            "expected a transition_id (UUID) or an RFC 3339 timestamp \
             (e.g. 2026-06-30T00:00:00Z), got `{s}`"
        ))
    }
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
    /// List currently-admitted claims, or claims as they were at a past
    /// `transition_id` via `--as-of`. A repeatable `--predicate <Name>`
    /// narrows either read to the named predicates - the targeted query
    /// an embedder uses to read governed state back.
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
    /// Enumerate a derived claim from a `.morph` source file against the
    /// current state, or against the state at a past `transition_id`
    /// via `--as-of`. Read-only: no claims are written, no audit row
    /// is produced.
    Derived(InspectDerivedArgs),
    /// List the declared predicate vocabulary of a `.morph` source file.
    /// Read-only: no database connection, no state. The declarations
    /// are static programme metadata, the same data `Program::validate`
    /// checks references against.
    Predicates(InspectPredicatesArgs),
    /// Show the states a `.morph` programme makes impossible - one entry
    /// per invariant, naming the forbidden state where it is
    /// mechanically obvious. Read-only and static: no database, no
    /// state. Prose by default; `--json` for the structured form.
    Guarantees(InspectGuaranteesArgs),
}

/// Arguments for `inspect predicates`. No `--as-of`; predicate
/// declarations are programme metadata, not state.
#[derive(clap::Args, Debug)]
pub(crate) struct InspectPredicatesArgs {
    /// Path to a `.morph` source file.
    pub(crate) file: PathBuf,
}

/// Arguments for `inspect guarantees`. Like `inspect predicates`, a
/// static read over a parsed `.morph` programme; `--json` switches the
/// prose view for the structured form.
#[derive(clap::Args, Debug)]
pub(crate) struct InspectGuaranteesArgs {
    /// Path to a `.morph` source file.
    pub(crate) file: PathBuf,
    /// Emit the structured JSON form instead of prose.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Arguments for `inspect claims`. Like the shared `InspectArgs` plus
/// an optional `--as-of` for historical claim listing and a repeatable
/// `--predicate` filter for targeted reads.
#[derive(clap::Args, Debug)]
pub(crate) struct InspectClaimsArgs {
    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: String,

    /// Optional: list claims as they were at a past moment - either a
    /// `transition_id` (UUIDv7) or an RFC 3339 timestamp resolved to
    /// the last transition committed at or before it. Without it, the
    /// current admitted claim set is returned; with it, the adapter
    /// replays the audit log up to the resolved transition. Unknown
    /// ids return `TransitionNotFound`; a timestamp earlier than every
    /// commit returns `NoTransitionAtOrBefore`.
    #[arg(long)]
    pub(crate) as_of: Option<AsOf>,

    /// Optional, repeatable: return only claims of these predicates -
    /// the targeted read an embedder uses to fetch governed state back
    /// (e.g. the in-force pointer claim) instead of the whole claim
    /// set. Composes with `--as-of`, where it also scopes the replay
    /// itself. An unknown predicate name matches nothing and yields an
    /// empty result, not an error: the claims table is the authority,
    /// not any one programme's vocabulary.
    #[arg(long = "predicate")]
    pub(crate) predicate: Vec<String>,
}

/// Arguments for `inspect derived`.
#[derive(clap::Args, Debug)]
pub(crate) struct InspectDerivedArgs {
    /// Path to a `.morph` source file.
    pub(crate) file: PathBuf,

    /// Derived claim predicate name (e.g. `TrialBalanceRow`). Looked
    /// up against the program's `derived_claims` by `predicate`.
    pub(crate) derived: String,

    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: String,

    /// Optional: enumerate against the state at a past moment - either
    /// a `transition_id` (UUIDv7) or an RFC 3339 timestamp resolved to
    /// the last transition committed at or before it - instead of
    /// current state. Same predicate-scoped replay; unknown ids return
    /// `TransitionNotFound`, and a timestamp earlier than every commit
    /// returns `NoTransitionAtOrBefore`.
    #[arg(long)]
    pub(crate) as_of: Option<AsOf>,
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
/// file (`parse`). No database connection.
#[derive(clap::Args, Debug)]
pub(crate) struct SourceFileArgs {
    /// Path to a `.morph` source file.
    pub(crate) file: PathBuf,
}

/// Arguments for `check`: a `.morph` source file plus the optional
/// summary flag. Success stays silent by default - scripts rely on
/// the empty-streams contract - so the reassurance a first-time user
/// wants is opt-in rather than the script-facing default.
#[derive(clap::Args, Debug)]
pub(crate) struct CheckArgs {
    /// Path to a `.morph` source file.
    pub(crate) file: PathBuf,

    /// Print a one-screen summary of the validated programme: its name
    /// and the count of each declaration kind.
    #[arg(short, long)]
    pub(crate) verbose: bool,
}

/// Arguments for `schema`. A `.morph` source file plus exactly one of:
/// a transformation name (its argument contract) or `--intent <Type>`
/// (an emitted intent's payload contract, for a deliverer decoding an
/// outbox row by name). No database connection - schema generation is a
/// pure static read over the parsed and validated programme.
#[derive(clap::Args, Debug)]
pub(crate) struct SchemaArgs {
    /// Path to a `.morph` source file.
    pub(crate) file: PathBuf,

    /// Transformation name whose argument contract to emit.
    #[arg(required_unless_present = "intent", conflicts_with = "intent")]
    pub(crate) transformation: Option<String>,

    /// Intent type name whose payload contract to emit, instead of a
    /// transformation's arguments.
    #[arg(long, required_unless_present = "transformation")]
    pub(crate) intent: Option<String>,
}

/// Arguments for the `run` subcommand: a `.morph` source file plus the
/// transformation, JSON args (in one of two codecs), actor, connection
/// string, and optional trace flag.
///
/// `--args` and `--args-named` are mutually exclusive at the Clap level
/// and exactly one of the two is required. The first is the
/// implementer-facing tagged-EvalValue codec; the second is the
/// embedder-facing bare-by-name codec that mirrors the JSON Schema
/// `morpholog schema` emits.
#[derive(clap::Args, Debug)]
pub(crate) struct RunArgs {
    /// Path to a `.morph` source file containing the programme.
    pub(crate) file: PathBuf,

    /// Transformation name within the parsed programme.
    pub(crate) transformation: String,

    /// JSON array of arguments matching the transformation's parameter
    /// list. Each element is an `EvalValue` in the tagged form:
    /// `{"type":"subject","value":"..."}`, `{"type":"decimal",
    /// "value":"100"}`, `{"type":"bool","value":true}`, or
    /// `{"type":"collection","value":[...]}`. The implementer-facing
    /// codec; carries Polymorphic / Ambiguous / Collection inputs the
    /// schema cannot describe unambiguously.
    #[arg(
        long,
        conflicts_with = "args_named",
        required_unless_present = "args_named"
    )]
    pub(crate) args: Option<String>,

    /// JSON object keyed by parameter name with bare values matching
    /// the JSON Schema emitted by `morpholog schema`. The embedder-
    /// facing codec; strict (missing required, unknown keys, wrong
    /// types, and `null` all error). Refuses Polymorphic, Ambiguous,
    /// Unconstrained, and Collection parameters; use `--args` for
    /// those.
    #[arg(long, conflicts_with = "args", required_unless_present = "args")]
    pub(crate) args_named: Option<String>,

    /// Subject identifying the actor under whose authority this
    /// transition is proposed. Wrapped as an `EvalValue::Subject` and
    /// persisted to `morpholog.audit.actor`.
    #[arg(long)]
    pub(crate) actor: String,

    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: String,

    /// When set, emit a structured per-statement trace alongside the
    /// outcome - the kernel's `propose_with_trace` shape on the wire.
    #[arg(long)]
    pub(crate) trace: bool,
}

/// Arguments for `explain`. The same source/transformation/args/actor
/// shape as [`RunArgs`] - it builds the identical `Transition` - but with
/// `--json` in place of `--trace`: explain's whole output already is the
/// interpreted trace, so prose-or-JSON is the only output choice.
///
/// `--args` and `--args-named` are mutually exclusive at the Clap level
/// and exactly one is required. Same semantics as `run`: the first is
/// the implementer-facing tagged codec, the second is the embedder-
/// facing bare-by-name codec.
#[derive(clap::Args, Debug)]
pub(crate) struct ExplainArgs {
    /// Path to a `.morph` source file containing the programme.
    pub(crate) file: PathBuf,

    /// Transformation name within the parsed programme.
    pub(crate) transformation: String,

    /// JSON array of arguments matching the transformation's parameter
    /// list, in the tagged-EvalValue codec - e.g.
    /// `[{"type":"subject","value":"c1"},{"type":"decimal","value":"100"}]`.
    /// See `run --args` for the full codec description.
    #[arg(
        long,
        conflicts_with = "args_named",
        required_unless_present = "args_named"
    )]
    pub(crate) args: Option<String>,

    /// JSON object keyed by parameter name with bare values matching
    /// the JSON Schema emitted by `morpholog schema`. The embedder-
    /// facing codec; same strict semantics as `run --args-named`.
    #[arg(long, conflicts_with = "args", required_unless_present = "args")]
    pub(crate) args_named: Option<String>,

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
        Command::Parse(args) => commands::parse::run(args),
        Command::Check(args) => commands::check::run(args),
        Command::Run(args) => commands::run::run(args).await,
        Command::Explain(args) => commands::explain::run(args).await,
        Command::Outbox { what } => match what {
            OutboxCmd::Claim(args) => commands::outbox::claim(args).await,
            OutboxCmd::Complete(args) => commands::outbox::complete(args).await,
            OutboxCmd::Release(args) => commands::outbox::release(args).await,
        },
        Command::Schema(args) => commands::schema::run(args),
        Command::Verify(args) => commands::verify::run(args).await,
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
            Some(AsOf::Transition(Uuid::parse_str(tid).unwrap())),
            "--as-of with a UUID must parse into the transition form"
        );
    }

    /// `--as-of` also accepts an RFC 3339 timestamp, parsed into the
    /// at-or-before form (resolved against the audit log at run time).
    #[test]
    fn inspect_claims_with_as_of_timestamp_parses() {
        let cli = Cli::parse_from([
            "morpholog",
            "inspect",
            "claims",
            "--database-url",
            "postgres:///morpholog_dev",
            "--as-of",
            "2026-06-30T12:00:00Z",
        ]);
        let Command::Inspect {
            what: Inspect::Claims(args),
        } = cli.command
        else {
            panic!("expected Inspect::Claims, got {:?}", cli.command);
        };
        let Some(AsOf::AtOrBefore(at)) = args.as_of else {
            panic!("expected the at-or-before form, got {:?}", args.as_of);
        };
        assert_eq!(at.to_rfc3339(), "2026-06-30T12:00:00+00:00");
    }

    /// A bare date is rejected: the coordinate must be explicit about
    /// the instant, not leave the time of day to a guess.
    #[test]
    fn inspect_claims_with_bare_date_as_of_errors_at_parse_time() {
        let err = Cli::try_parse_from([
            "morpholog",
            "inspect",
            "claims",
            "--database-url",
            "postgres:///morpholog_dev",
            "--as-of",
            "2026-06-30",
        ])
        .expect_err("bare date must surface a clap parse error");
        assert!(
            matches!(
                err.kind(),
                ErrorKind::ValueValidation | ErrorKind::InvalidValue
            ),
            "expected a value-validation error, got {:?}",
            err.kind()
        );
    }

    /// `--predicate` repeats into a Vec, in argv order; without it the
    /// filter defaults to empty (the unfiltered read stays the default).
    #[test]
    fn inspect_claims_predicate_flag_repeats_into_vec() {
        let cli = Cli::parse_from([
            "morpholog",
            "inspect",
            "claims",
            "--database-url",
            "postgres:///morpholog_dev",
            "--predicate",
            "OfficialPrice",
            "--predicate",
            "CurrentOfficialPrice",
        ]);
        let Command::Inspect {
            what: Inspect::Claims(args),
        } = cli.command
        else {
            panic!("expected Inspect::Claims, got {:?}", cli.command);
        };
        assert_eq!(args.predicate, ["OfficialPrice", "CurrentOfficialPrice"]);
    }

    #[test]
    fn inspect_claims_without_predicate_defaults_to_empty_filter() {
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
        assert!(args.predicate.is_empty(), "no flag means no filter");
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
        assert_eq!(
            args.as_of,
            Some(AsOf::Transition(Uuid::parse_str(tid).unwrap()))
        );
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
    fn run_with_all_args_parses() {
        let cli = Cli::parse_from([
            "morpholog",
            "run",
            "examples/03_double_entry_ledger/ledger.morph",
            "post_simple_entry",
            "--args",
            "[]",
            "--actor",
            "jordan",
            "--database-url",
            "postgres:///morpholog_dev",
        ]);
        let Command::Run(args) = cli.command else {
            panic!("expected Run, got {:?}", cli.command);
        };
        assert_eq!(
            args.file,
            std::path::PathBuf::from("examples/03_double_entry_ledger/ledger.morph")
        );
        assert_eq!(args.transformation, "post_simple_entry");
        assert_eq!(args.args.as_deref(), Some("[]"));
        assert!(args.args_named.is_none());
        assert_eq!(args.actor, "jordan");
        assert_eq!(args.database_url, "postgres:///morpholog_dev");
    }

    /// `run --args-named '{...}'` parses with `args_named: Some(...)`
    /// and `args: None`. Confirms the new flag plumbs through.
    #[test]
    fn run_with_args_named_parses_into_the_named_slot() {
        let cli = Cli::parse_from([
            "morpholog",
            "run",
            "examples/03_double_entry_ledger/ledger.morph",
            "post_simple_entry",
            "--args-named",
            r#"{"trade":"a"}"#,
            "--actor",
            "jordan",
            "--database-url",
            "postgres:///morpholog_dev",
        ]);
        let Command::Run(args) = cli.command else {
            panic!("expected Run, got {:?}", cli.command);
        };
        assert!(args.args.is_none(), "--args should not be set");
        assert_eq!(args.args_named.as_deref(), Some(r#"{"trade":"a"}"#));
    }

    /// Passing BOTH `--args` and `--args-named` must be rejected at
    /// Clap-parse time so the run path never sees an ambiguous
    /// request shape.
    #[test]
    fn run_with_both_args_codecs_errors() {
        let err = Cli::try_parse_from([
            "morpholog",
            "run",
            "examples/03_double_entry_ledger/ledger.morph",
            "post_simple_entry",
            "--args",
            "[]",
            "--args-named",
            "{}",
            "--actor",
            "jordan",
            "--database-url",
            "postgres:///morpholog_dev",
        ])
        .expect_err("both --args and --args-named should error");
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn run_missing_args_flag_errors() {
        let err = Cli::try_parse_from([
            "morpholog",
            "run",
            "examples/03_double_entry_ledger/ledger.morph",
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
    fn run_missing_actor_flag_errors() {
        let err = Cli::try_parse_from([
            "morpholog",
            "run",
            "examples/03_double_entry_ledger/ledger.morph",
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
    fn run_missing_positional_errors() {
        let err = Cli::try_parse_from([
            "morpholog",
            "run",
            "examples/03_double_entry_ledger/ledger.morph",
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
            "examples/03_double_entry_ledger/ledger.morph",
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
        assert_eq!(
            args.file,
            std::path::PathBuf::from("examples/03_double_entry_ledger/ledger.morph")
        );
        assert_eq!(args.derived, "TrialBalanceRow");
        assert_eq!(args.database_url, "postgres:///morpholog_dev");
    }

    #[test]
    fn inspect_derived_missing_derived_name_errors() {
        // Two positionals are required (file + derived name). Omit the
        // derived name; clap must surface MissingRequiredArgument rather
        // than silently taking the flag as the missing arg.
        let err = Cli::try_parse_from([
            "morpholog",
            "inspect",
            "derived",
            "examples/03_double_entry_ledger/ledger.morph",
            "--database-url",
            "postgres:///morpholog_dev",
        ])
        .expect_err("missing derived positional should error");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    /// `inspect predicates <file.morph>` parses to `Inspect::Predicates`
    /// with the file path on the args struct. No `--database-url` flag:
    /// predicate declarations are programme metadata, not state.
    #[test]
    fn inspect_predicates_parses_with_file_argument() {
        let cli = Cli::parse_from([
            "morpholog",
            "inspect",
            "predicates",
            "examples/06_clinical_trial_enrolment/clinical_trial_enrolment.morph",
        ]);
        let Command::Inspect { what } = cli.command else {
            panic!("expected Inspect, got {:?}", cli.command);
        };
        let Inspect::Predicates(args) = what else {
            panic!("expected Inspect::Predicates, got {what:?}");
        };
        assert_eq!(
            args.file,
            std::path::PathBuf::from(
                "examples/06_clinical_trial_enrolment/clinical_trial_enrolment.morph"
            )
        );
    }

    /// Omitting the file positional must produce a clap
    /// MissingRequiredArgument error.
    #[test]
    fn inspect_predicates_missing_file_errors() {
        let err = Cli::try_parse_from(["morpholog", "inspect", "predicates"])
            .expect_err("missing file positional should error");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }

    /// `run --trace` parses to a `RunArgs` with `trace: true`. All other
    /// fields keep their existing behaviour.
    #[test]
    fn run_with_trace_flag_parses() {
        let cli = Cli::parse_from([
            "morpholog",
            "run",
            "examples/01_settlement_netting/netting.morph",
            "create_net_settlement",
            "--actor",
            "jordan",
            "--args",
            "[]",
            "--database-url",
            "postgres:///morpholog_dev",
            "--trace",
        ]);
        let Command::Run(args) = cli.command else {
            panic!("expected Run, got {:?}", cli.command);
        };
        assert!(args.trace, "expected trace flag to be set");
        assert_eq!(args.transformation, "create_net_settlement");
        assert_eq!(args.actor, "jordan");
    }

    /// Without `--trace`, `RunArgs.trace` defaults to false. The non-trace
    /// path must not be affected by the flag.
    #[test]
    fn run_without_trace_flag_defaults_to_false() {
        let cli = Cli::parse_from([
            "morpholog",
            "run",
            "examples/01_settlement_netting/netting.morph",
            "create_net_settlement",
            "--actor",
            "jordan",
            "--args",
            "[]",
            "--database-url",
            "postgres:///morpholog_dev",
        ]);
        let Command::Run(args) = cli.command else {
            panic!("expected Run, got {:?}", cli.command);
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

    /// `check -v` parses to `CheckArgs { verbose: true }`. Pins the
    /// short form alongside the long one.
    #[test]
    fn check_with_verbose_flag_parses() {
        let cli = Cli::parse_from(["morpholog", "check", "demo.morph", "-v"]);
        let Command::Check(args) = cli.command else {
            panic!("expected Command::Check, got {:?}", cli.command);
        };
        assert!(args.verbose, "expected -v to set verbose");
        assert_eq!(args.file.as_os_str(), "demo.morph");
    }

    /// Without `--verbose`, `CheckArgs.verbose` defaults to false -
    /// the silent-success contract scripts rely on stays the default.
    #[test]
    fn check_without_verbose_defaults_to_false() {
        let cli = Cli::parse_from(["morpholog", "check", "demo.morph"]);
        let Command::Check(args) = cli.command else {
            panic!("expected Command::Check, got {:?}", cli.command);
        };
        assert!(!args.verbose, "expected verbose to default to false");
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
        assert_eq!(args.args.as_deref(), Some("[]"));
        assert!(args.args_named.is_none());
        assert_eq!(args.actor, "jordan");
        assert_eq!(args.database_url, "postgres:///morpholog_dev");
        assert!(!args.json, "expected --json to default to false");
    }

    /// `explain --args-named` parses with `args_named: Some(...)` and
    /// `args: None`. Mirrors `run_with_args_named_parses_into_the_named_slot`
    /// to confirm explain plumbs the new flag identically.
    #[test]
    fn explain_with_args_named_parses_into_the_named_slot() {
        let cli = Cli::parse_from([
            "morpholog",
            "explain",
            "model.morph",
            "issue_credit",
            "--args-named",
            r#"{"x":"y"}"#,
            "--actor",
            "jordan",
            "--database-url",
            "postgres:///morpholog_dev",
        ]);
        let Command::Explain(args) = cli.command else {
            panic!("expected Explain, got {:?}", cli.command);
        };
        assert!(args.args.is_none(), "--args should not be set");
        assert_eq!(args.args_named.as_deref(), Some(r#"{"x":"y"}"#));
    }

    /// Mutual exclusion at parse time for explain too: passing both
    /// `--args` and `--args-named` is a hard error.
    #[test]
    fn explain_with_both_args_codecs_errors() {
        let err = Cli::try_parse_from([
            "morpholog",
            "explain",
            "model.morph",
            "issue_credit",
            "--args",
            "[]",
            "--args-named",
            "{}",
            "--actor",
            "jordan",
            "--database-url",
            "postgres:///morpholog_dev",
        ])
        .expect_err("both --args and --args-named should error");
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
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

    /// `morpholog schema <file> <transformation>` parses into the
    /// expected positional args. The schema subcommand takes no flags
    /// (no `--json`, no `--database-url`), so the test pins that the
    /// minimal positional surface is what the embedder will type.
    #[test]
    fn schema_with_file_and_transformation_parses() {
        let cli = Cli::parse_from([
            "morpholog",
            "schema",
            "examples/10_trade_lifecycle/trade_lifecycle.morph",
            "capture_trade",
        ]);
        let Command::Schema(args) = cli.command else {
            panic!("expected Command::Schema, got {:?}", cli.command);
        };
        assert_eq!(args.transformation.as_deref(), Some("capture_trade"));
        assert!(args.intent.is_none());
        assert_eq!(
            args.file.to_string_lossy(),
            "examples/10_trade_lifecycle/trade_lifecycle.morph"
        );
    }

    /// `--intent <Type>` parses as the payload-schema alternative to a
    /// positional transformation name.
    #[test]
    fn schema_with_intent_parses() {
        let cli = Cli::parse_from([
            "morpholog",
            "schema",
            "file.morph",
            "--intent",
            "TradeSettlementRequested",
        ]);
        let Command::Schema(args) = cli.command else {
            panic!("expected Command::Schema, got {:?}", cli.command);
        };
        assert_eq!(args.intent.as_deref(), Some("TradeSettlementRequested"));
        assert!(args.transformation.is_none());
    }

    /// Supplying both a transformation and `--intent` is a conflict.
    #[test]
    fn schema_transformation_and_intent_conflict() {
        let err =
            Cli::try_parse_from(["morpholog", "schema", "file.morph", "cap", "--intent", "X"])
                .expect_err("transformation + --intent should conflict");
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    }

    /// Neither a transformation nor `--intent` should error at
    /// clap-parse time, before any file IO happens.
    #[test]
    fn schema_missing_transformation_errors() {
        let err = Cli::try_parse_from(["morpholog", "schema", "file.morph"])
            .expect_err("missing transformation name should error");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }
}
