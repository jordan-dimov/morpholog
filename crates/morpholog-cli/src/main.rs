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
#[command(
    version,
    about = "Business rules the database itself enforces: declare them once in a \
             .morph file, and no change that breaks them can ever commit.",
    help_template = "{name} {version}\n{about-with-newline}\n{usage-heading} {usage}\n\n{all-args}{after-help}",
    after_help = "Getting started:\n  \
        morpholog check rules.morph        are my rules sound?\n  \
        morpholog init                     set up the database tables\n  \
        morpholog propose rules.morph <transformation> --actor you --args-named '{...}'\n  \
        morpholog inspect claims           what is admitted right now?\n\n\
        Database commands read the connection from --database-url or $DATABASE_URL.\n\
        Every command has deeper help: morpholog help <command>."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

// Listed in the order a new user meets them: write rules, set up a
// database, propose changes, ask why, look at what happened - then
// the integrity, contract, and plumbing commands. The first doc line
// of each variant is the whole story a beginner needs; the paragraphs
// after it are the depth `morpholog help <command>` shows.
#[derive(Subcommand, Debug)]
enum Command {
    /// Check that a `.morph` file parses and its rules are sound.
    ///
    /// Validates the whole programme: declarations and arity for
    /// predicates and intents, kind/type compatibility, binding flow
    /// (unbound variables), expression shape, actor context, and a
    /// nesting-depth bound. Exits zero on a clean programme -
    /// silently by default, with a one-screen summary under
    /// `--verbose`; hint-grade lints print to stderr and `--strict`
    /// promotes them to errors. Exits one with diagnostics pointing
    /// at the source line on either a parse or a validation failure.
    /// `--ir` additionally prints the validated programme's internal
    /// representation as JSON - the debugging view.
    Check(CheckArgs),

    /// Set up the Morpholog tables in an existing PostgreSQL database.
    ///
    /// Provisions the schema (claims, audit, outbox, rejections) from
    /// the canonical copy embedded in this binary, so a binary-only
    /// deployment provisions exactly the schema this build expects -
    /// nothing to vendor, nothing to drift. Day-zero only: refuses if
    /// the `morpholog` schema already exists (`--skip-if-exists`
    /// reports and exits zero instead, for idempotent entrypoints);
    /// never drops, never migrates.
    Init(InitArgs),

    /// Propose a change: it commits only if every rule holds.
    ///
    /// Parses and validates the `.morph` source, then proposes the
    /// named transformation with the supplied actor and arguments
    /// (`--args-named` for a field-keyed object, `--args` for the
    /// tagged positional form). On commit, prints the outcome as JSON
    /// and exits zero; on a refusal, prints the reason and exits one
    /// (a lawful answer, on the record); on any other error - bad
    /// args, unknown transformation, connection failure - prints to
    /// stderr and exits one. `--batch -` admits NDJSON rows from
    /// stdin, one receipt per row; `--explain-on-reject` attaches the
    /// structured explanation to refusals.
    Propose(ProposeArgs),

    /// Preview whether a change would be admitted or refused, and why.
    ///
    /// Nothing is committed and nothing is recorded: this is a
    /// dry-run diagnosis against live state. Renders the gate that
    /// would fail with the directly-missing claims, the violated
    /// invariant, or admissibility - as plain prose, or as JSON with
    /// `--json`. The verdict does not affect the exit code (zero on
    /// both admissible and refused); only operational failures exit
    /// non-zero.
    Explain(ExplainArgs),

    /// Look inside a running system: state, history, refusals, rules.
    ///
    /// Every view is read-only. `claims` and `derived` read what is
    /// admitted (now, or at any past moment via `--as-of`); `audit`
    /// streams the full history of committed changes; `rejections`
    /// lists refusals; `coverage`, `guarantees`, `controls`, and
    /// `predicates` answer what the rules forbid, require, and have
    /// actually been doing.
    Inspect {
        #[command(subcommand)]
        what: Inspect,
    },

    /// Check that the claims table and the audit log still agree.
    ///
    /// The two tables are independent records of the same history, so
    /// replaying the audit log must land on exactly the current
    /// claims - a difference is evidence that one was modified
    /// outside the runtime. Prints the outcome as JSON; consistent
    /// exits zero, divergent prints the claims each record holds that
    /// the other does not and exits one. Read-only; an empty database
    /// is trivially consistent.
    ///
    /// Also recomputes the audit Merkle tree against its checkpoints
    /// (tamper evidence). Pass `--anchor-file` with a checkpoint saved
    /// earlier by `checkpoint` to detect a coordinated edit of the audit
    /// log and the checkpoint table - the only check an attacker with
    /// full database access cannot defeat.
    Verify(VerifyArgs),

    /// Record a tamper-evident checkpoint over the audit log.
    ///
    /// Computes the RFC 6962 Merkle root of the committed audit prefix
    /// and chains it onto the previous checkpoint. Prints the checkpoint
    /// as JSON - save it outside the database as an anchor: a later
    /// `verify --anchor-file` against it catches any rewrite of the log,
    /// even one that also rewrites the checkpoint table.
    Checkpoint(DatabaseArgs),

    /// Score a candidate programme against committed history.
    ///
    /// Replays the committed audit log under the candidate's invariants -
    /// which are NOT deployed - and reports, per invariant, which already-
    /// admitted commits it would have refused: a fresh violation, where the
    /// commit's resulting state violates an invariant the prior state
    /// satisfied. The fitness signal for discovering controls nobody
    /// hand-authored. Output is JSON. Scores state invariants only;
    /// transition-relational candidates using `pre(...)` are rejected.
    Evaluate(EvaluateArgs),

    /// Export and verify portable evidence packs over the audit log.
    ///
    /// `export` writes a complete, checkpointed prefix of the log (its
    /// rows, the checkpoint chain, a thin manifest) as JSON; `verify`
    /// checks one offline - recomputing the Merkle root and matching it
    /// against the pack's checkpoints and an external anchor - with no
    /// database access at all. A pack carries the full audit prefix
    /// (actors, arguments, claims, intents): it is not selective
    /// disclosure and may hold confidential business data.
    Evidence {
        #[command(subcommand)]
        what: EvidenceCmd,
    },

    /// Print a stable fingerprint of a programme's rules.
    ///
    /// SHA-256 over the canonical (formatter-rendered) source, as
    /// `{"program": ..., "hash": "sha256:..."}`. Formatting-only
    /// edits do not change the hash and comments do not survive
    /// canonicalisation, so this is rules-identity, not
    /// file-identity - the right value for a ruleset version in
    /// deployment metadata or an evidence pack. Only a valid
    /// programme hashes.
    Hash(SourceFileArgs),

    /// Print the JSON Schema contracts an external system integrates against.
    ///
    /// A named transformation's argument object, an intent's payload
    /// (`--intent <Type>`), the machine-readable outcome envelopes
    /// (`--result`), or one manifest covering the whole programme
    /// (`--all`: every schema, the predicate vocabulary, the
    /// declaration-order arrays, and the canonical model hash). The
    /// schema is the public contract a non-Rust embedder uses to
    /// validate request bodies, generate forms, or derive typed
    /// models without touching Rust. Output is JSON Schema (Draft
    /// 2020-12); no `--json` flag because the output IS JSON.
    Schema(SchemaArgs),

    /// Generate a typed client that speaks exactly this binary's contract.
    ///
    /// The client is a projection of the programme, like the schema
    /// and the envelopes; generating it here is what keeps it from
    /// being hand-maintained downstream, where it drifts.
    Generate {
        #[command(subcommand)]
        what: GenerateCmd,
    },

    /// Refresh a kernel-computed read model in the database.
    ///
    /// Out-of-band, never on the commit path: an explicit operator step
    /// (run after a batch, or on a schedule), so read-model freshness
    /// stays operational rather than slowing every governed transition.
    Refresh {
        #[command(subcommand)]
        what: RefreshCmd,
    },

    /// Drive outbox delivery from a shell or script.
    ///
    /// Lets any external deliverer participate in the lease protocol
    /// (`claim` to acquire a row, `complete` to resolve it, `release`
    /// to abandon it back to pending) without writing a Rust
    /// `Deliverer` impl.
    Outbox {
        #[command(subcommand)]
        what: OutboxCmd,
    },
}

/// Read-model refresh targets. `derived` recomputes every derived claim
/// with the kernel and publishes it to the `morpholog_read` projection
/// that derived SQL views read.
#[derive(Subcommand, Debug)]
pub(crate) enum RefreshCmd {
    /// Recompute all derived claims and publish a new generation of the
    /// `morpholog_read` read model (exact kernel output, never governed
    /// state). Derived SQL views read this projection; it is as fresh as
    /// the last refresh.
    Derived(RefreshDerivedArgs),
}

/// Arguments for `refresh derived`.
#[derive(clap::Args, Debug)]
pub(crate) struct RefreshDerivedArgs {
    /// Path to a `.morph` source file.
    pub(crate) file: PathBuf,

    #[command(flatten)]
    pub(crate) db: DatabaseArgs,
}

/// Client-generation targets. One language per worked embedder that
/// forces it; `python-client` is forced by Glasshouse and the worked
/// embedder example converging on the same hand-written layer.
#[derive(Subcommand, Debug)]
pub(crate) enum GenerateCmd {
    /// Emit a complete, self-contained, stdlib-only Python client
    /// package (`morpholog_client/`) for the programme: value codecs,
    /// envelope models, the subprocess adapter, a typed request model
    /// per transformation, a typed read model per predicate, and a
    /// typed payload per intent - stamped with the canonical model
    /// hash and this binary's version. Deterministic: the same binary
    /// and programme produce byte-identical output, so drift-checking
    /// is regenerate-and-diff.
    #[command(name = "python-client")]
    PythonClient(GeneratePythonClientArgs),

    /// Emit a typed, read-only SQL view surface over `morpholog.claims`
    /// for the programme's base predicates: one `CREATE OR REPLACE VIEW`
    /// per declared base predicate, columns cast to natural PostgreSQL
    /// types, plus a model-hash catalogue - all in a single atomic
    /// (`BEGIN; ... COMMIT;`) script. Non-updatable by construction;
    /// regenerate-and-diff for drift, like `python-client`. Derived
    /// claims are read via `inspect derived`, not views.
    Views(GenerateViewsArgs),
}

/// Arguments for `generate views`.
#[derive(clap::Args, Debug)]
pub(crate) struct GenerateViewsArgs {
    /// Path to a `.morph` source file.
    pub(crate) file: PathBuf,

    /// Schema the views are created in (created with
    /// `CREATE SCHEMA IF NOT EXISTS`). Namespaced away from the governed
    /// `morpholog` schema.
    #[arg(long, default_value = "morpholog_views")]
    pub(crate) schema: String,

    /// Write the SQL script to this file instead of stdout.
    #[arg(long)]
    pub(crate) out: Option<PathBuf>,
}

/// Arguments for `generate python-client`.
#[derive(clap::Args, Debug)]
pub(crate) struct GeneratePythonClientArgs {
    /// Path to a `.morph` source file.
    pub(crate) file: PathBuf,

    /// Directory to write the `morpholog_client/` package under.
    #[arg(long)]
    pub(crate) out: PathBuf,
}

/// The connection-string flag every database-backed subcommand
/// shares, declared once and `#[command(flatten)]`ed in. Subcommands
/// whose only input is the connection take this struct directly.
#[derive(clap::Args, Debug)]
pub(crate) struct DatabaseArgs {
    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: String,
}

/// Arguments for `verify`: the connection plus an optional external
/// checkpoint anchor to verify the audit tree against.
#[derive(clap::Args, Debug)]
pub(crate) struct VerifyArgs {
    #[command(flatten)]
    pub(crate) db: DatabaseArgs,

    /// Path to a checkpoint JSON file (as printed by `checkpoint`), held
    /// outside the database. The audit tree is verified to still match
    /// it - the check a coordinated rewrite of audit + checkpoints cannot
    /// pass. Omit to verify only internal checkpoint consistency.
    #[arg(long)]
    pub(crate) anchor_file: Option<std::path::PathBuf>,
}

/// Arguments for `evaluate`: a candidate `.morph` path, and the history to
/// score it against - either a live connection or a portable evidence pack.
#[derive(clap::Args, Debug)]
pub(crate) struct EvaluateArgs {
    /// Path to the candidate `.morph` source file to score.
    pub(crate) file: std::path::PathBuf,

    /// Score against the committed audit log at this connection (or
    /// `DATABASE_URL`). The default mode; omit when using `--pack`.
    #[arg(long, env = "DATABASE_URL")]
    pub(crate) database_url: Option<String>,

    /// Score offline against a portable evidence pack instead of a
    /// database. When given, no connection is opened or used.
    #[arg(long)]
    pub(crate) pack: Option<std::path::PathBuf>,

    /// Batch mode: score offline against every `*.json` evidence pack in
    /// this directory, in one process, returning one JSON report with a
    /// case per pack. No connection is opened. Anchors are single-pack only.
    #[arg(long, conflicts_with = "pack")]
    pub(crate) packs: Option<std::path::PathBuf>,

    /// An external checkpoint anchor for `--pack`, held outside the
    /// database: the pack must verify against it before it is scored.
    /// Single-pack only - a single anchor is meaningless across a batch.
    #[arg(long, requires = "pack", conflicts_with = "packs")]
    pub(crate) anchor_file: Option<std::path::PathBuf>,
}

/// Evidence-pack subcommands. `export` is database-backed; `verify` is
/// deliberately offline - it takes no connection string, only files.
#[derive(clap::Subcommand, Debug)]
pub(crate) enum EvidenceCmd {
    /// Export a complete-prefix evidence pack as JSON (redirect to a
    /// file). Covers the latest checkpoint, or the checkpoint at
    /// `--tree-size N`. Refuses if there is no such checkpoint.
    Export(EvidenceExportArgs),

    /// Verify a pack offline, with no database. Recomputes the Merkle
    /// root from the pack's rows and checks it against the pack's
    /// checkpoints, and against an external `--anchor-file` if given.
    /// Exit one on any tamper, divergence, or malformed pack.
    Verify(EvidenceVerifyArgs),
}

/// Arguments for `evidence export`: the connection plus an optional exact
/// checkpoint size to cover.
#[derive(clap::Args, Debug)]
pub(crate) struct EvidenceExportArgs {
    #[command(flatten)]
    pub(crate) db: DatabaseArgs,

    /// Cover the checkpoint whose `tree_size` equals this value, instead
    /// of the latest. Must match an existing checkpoint exactly - a later
    /// checkpoint does not prove an arbitrary earlier prefix until
    /// consistency proofs exist.
    #[arg(long)]
    pub(crate) tree_size: Option<i64>,
}

/// Arguments for `evidence verify`: a pack file and an optional external
/// anchor. No connection string - the offline guarantee is in the shape.
#[derive(clap::Args, Debug)]
pub(crate) struct EvidenceVerifyArgs {
    /// Path to a pack JSON file (as printed by `evidence export`).
    pub(crate) pack_file: std::path::PathBuf,

    /// Path to a checkpoint JSON file (as printed by `checkpoint`), held
    /// outside the database. The checkpoint at the anchor's `tree_size` in
    /// the pack's chain is verified to match it (an older anchor is fine,
    /// as long as the pack still covers it) - the check a coordinated
    /// rewrite cannot pass. Omit to verify only the pack's internal
    /// consistency.
    #[arg(long)]
    pub(crate) anchor_file: Option<std::path::PathBuf>,
}

/// Arguments for `init`: the connection string plus the idempotent
/// entrypoint escape hatch.
#[derive(clap::Args, Debug)]
pub(crate) struct InitArgs {
    #[command(flatten)]
    pub(crate) db: DatabaseArgs,

    /// Exit zero with an `already-initialised` report when the
    /// `morpholog` schema already exists, instead of erroring. For
    /// deployment entrypoints that may run more than once.
    #[arg(long)]
    pub(crate) skip_if_exists: bool,
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

    #[command(flatten)]
    pub(crate) db: DatabaseArgs,
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

    #[command(flatten)]
    pub(crate) db: DatabaseArgs,
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

    #[command(flatten)]
    pub(crate) db: DatabaseArgs,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Inspect {
    /// List what is admitted right now - or at any past moment.
    ///
    /// A repeatable `--predicate <Name>` narrows the read to the
    /// named predicates - the targeted query an embedder uses to read
    /// governed state back; `--named <file.morph>` decodes arguments
    /// by declared field name under that programme's authority.
    /// `--as-of` reads the state as it was at a past transition id or
    /// RFC 3339 timestamp.
    Claims(InspectClaimsArgs),
    /// Compute a read-side view (a derived claim) from the admitted state.
    ///
    /// Enumerates the named derived claim against current state, or
    /// against a past state via `--as-of`. Read-only: no claims are
    /// written, no audit row is produced.
    Derived(InspectDerivedArgs),
    /// Stream the history of committed changes, one JSON line each.
    ///
    /// Commit order, the blessed tail for downstream projectors.
    /// `--after <transition_id>` resumes strictly after a previously
    /// seen transition (lossless: rows whose writers were still in
    /// flight are withheld until the next invocation, never skipped).
    /// `--named <file.morph>` decodes the asserted/retracted claim
    /// arrays by declared field name under that programme's
    /// authority; `arguments` and `emitted_intents` stay tagged.
    /// `--as-of` does not apply: the audit table IS the chronological
    /// record.
    Audit(InspectAuditArgs),
    /// List every refusal: who proposed what, and which rule said no.
    ///
    /// Operational evidence, written after each rollback
    /// at-most-once - the audit table remains the legitimacy-grade
    /// record of what was admitted.
    Rejections(DatabaseArgs),
    /// Report which rules have actually done work over the history.
    ///
    /// Replays the audit log and reports, per invariant, whether its
    /// condition ever matched anything - which rules have fired,
    /// which have only ever been trivially true, which
    /// transformations have never been used - and, from the
    /// rejection log, which rules have demonstrably refused a
    /// proposal (the `constrained` verdict). Read-only, safe against
    /// a live system. Prose with a legend by default; `--json` for
    /// the structured form. Always exits zero: coverage answers a
    /// question, it does not enforce.
    Coverage(InspectCoverageArgs),
    /// Show the states a programme makes impossible, rule by rule.
    ///
    /// One entry per invariant, naming the forbidden state where it
    /// is mechanically obvious. Static: no database, no state. Prose
    /// by default; `--json` for the structured form.
    Guarantees(InspectGuaranteesArgs),
    /// Show what each action requires first, and what can never hold.
    ///
    /// The control matrix: every transformation's `require` and
    /// `bind` preconditions with the predicates each consults, beside
    /// the invariant guarantees. The view an auditor reads, and the
    /// table a compliance mapping cites rule by rule. Static: no
    /// database, no state. Prose by default; `--json` for the
    /// structured form.
    Controls(InspectGuaranteesArgs),
    /// List the kinds of claims a programme declares.
    ///
    /// Static programme metadata - the same declarations
    /// `Program::validate` checks references against. No database
    /// connection.
    Predicates(InspectPredicatesArgs),
    /// List outbox intents awaiting (or past) delivery.
    ///
    /// Enqueue order; defaults to `--status pending`. Use `--status
    /// all` for a full view, or any of `delivered|failed|in-progress`
    /// for a slice. `--as-of` does not apply: outbox is delivery
    /// state, not claim state.
    Outbox(InspectOutboxArgs),
}

/// Arguments for `inspect coverage`: a `.morph` source file, the
/// connection flag, and the prose/JSON toggle.
#[derive(clap::Args, Debug)]
pub(crate) struct InspectCoverageArgs {
    /// Path to a `.morph` source file.
    pub(crate) file: PathBuf,

    #[command(flatten)]
    pub(crate) db: DatabaseArgs,

    /// Emit the structured JSON form instead of prose.
    #[arg(long)]
    pub(crate) json: bool,
}

/// Arguments for `inspect audit`: the connection flag, an optional
/// resume cursor, and the optional named decode.
#[derive(clap::Args, Debug)]
pub(crate) struct InspectAuditArgs {
    #[command(flatten)]
    pub(crate) db: DatabaseArgs,

    /// Resume strictly after this transition id (the cursor a
    /// previous invocation's last line carried). Unknown ids are an
    /// error, never a silent restart from zero.
    #[arg(long, value_name = "TRANSITION_ID")]
    pub(crate) after: Option<uuid::Uuid>,

    /// Decode each transition's asserted/retracted claims by declared
    /// field name under this `.morph` programme's authority. A
    /// returned claim whose predicate is undeclared, or whose arity
    /// disagrees with its declaration, is a hard error naming both
    /// sides.
    #[arg(long, value_name = "FILE")]
    pub(crate) named: Option<PathBuf>,
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

/// Arguments for `inspect claims`: the connection flag plus an
/// optional `--as-of` for historical claim listing and a repeatable
/// `--predicate` filter for targeted reads.
#[derive(clap::Args, Debug)]
pub(crate) struct InspectClaimsArgs {
    #[command(flatten)]
    pub(crate) db: DatabaseArgs,

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

    /// Optional: decode each claim's positional args into a bare named
    /// object using the declared vocabulary of this `.morph` file -
    /// the read-side mirror of `--args-named`. With it, the programme
    /// becomes the authority: a returned claim whose predicate is
    /// undeclared, or whose arity disagrees with its declaration, is a
    /// hard error naming both sides (programme/database skew), never a
    /// silent skip. Composes with `--predicate` and `--as-of`.
    #[arg(long = "named", value_name = "FILE")]
    pub(crate) named: Option<PathBuf>,
}

/// Arguments for `inspect derived`.
#[derive(clap::Args, Debug)]
pub(crate) struct InspectDerivedArgs {
    /// Path to a `.morph` source file.
    pub(crate) file: PathBuf,

    /// Derived claim predicate name (e.g. `TrialBalanceRow`). Looked
    /// up against the program's `derived_claims` by `predicate`.
    pub(crate) derived: String,

    #[command(flatten)]
    pub(crate) db: DatabaseArgs,

    /// Optional: enumerate against the state at a past moment - either
    /// a `transition_id` (UUIDv7) or an RFC 3339 timestamp resolved to
    /// the last transition committed at or before it - instead of
    /// current state. Same predicate-scoped replay; unknown ids return
    /// `TransitionNotFound`, and a timestamp earlier than every commit
    /// returns `NoTransitionAtOrBefore`.
    #[arg(long)]
    pub(crate) as_of: Option<AsOf>,
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

    #[command(flatten)]
    pub(crate) db: DatabaseArgs,
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

    /// Promote lint hints to errors: a finding that prints as
    /// `hint: ...` by default fails the check under `--strict`.
    /// Hints flag shapes with a deliberate reading (the
    /// gate-vs-invariant lint, for one), so the default keeps them
    /// advisory.
    #[arg(long)]
    pub(crate) strict: bool,

    /// Print the validated programme's internal representation as
    /// JSON - the debugging view. Behind validation on purpose: only
    /// a sound programme renders, which is what makes the view
    /// trustworthy.
    #[arg(long, conflicts_with_all = ["json", "verbose"])]
    pub(crate) ir: bool,

    /// Emit every finding - parse errors, validation errors, lints -
    /// as one JSON object on stdout, each with byte offsets and
    /// 1-based line/column where the finding has a source location.
    /// Exit semantics are unchanged.
    #[arg(long, conflicts_with = "verbose")]
    pub(crate) json: bool,
}

/// Arguments for `schema`. A `.morph` source file plus exactly one of:
/// a transformation name (its argument contract) or `--intent <Type>`
/// (an emitted intent's payload contract, for a deliverer decoding an
/// outbox row by name). No database connection - schema generation is a
/// pure static read over the parsed and validated programme.
#[derive(clap::Args, Debug)]
pub(crate) struct SchemaArgs {
    /// Path to a `.morph` source file. Not needed for `--result`,
    /// whose envelope contract is programme-independent.
    #[arg(required_unless_present = "result")]
    pub(crate) file: Option<PathBuf>,

    /// Transformation name whose argument contract to emit.
    #[arg(
        required_unless_present_any = ["intent", "all", "result"],
        conflicts_with_all = ["intent", "all", "result"]
    )]
    pub(crate) transformation: Option<String>,

    /// Intent type name whose payload contract to emit, instead of a
    /// transformation's arguments.
    #[arg(
        long,
        required_unless_present_any = ["transformation", "all", "result"],
        conflicts_with_all = ["all", "result"]
    )]
    pub(crate) intent: Option<String>,

    /// Emit one manifest covering the whole programme: every
    /// transformation's argument schema, every intent's payload
    /// schema, the declared predicate vocabulary, and the canonical
    /// model hash. One artefact for codegen to consume and CI to
    /// drift-check, instead of N subprocess calls.
    #[arg(long, conflicts_with = "result")]
    pub(crate) all: bool,

    /// Emit the outcome-envelope contract: one JSON Schema document
    /// whose `$defs` cover every machine-readable envelope the CLI
    /// prints (run outcomes, explanations, batch receipts, outbox
    /// rows, check reports). Programme-independent - the shapes vary
    /// only with the binary, so no `.morph` file is taken.
    #[arg(long)]
    pub(crate) result: bool,
}

/// Arguments for the `propose` subcommand: a `.morph` source file plus the
/// transformation, JSON args (in one of two codecs), actor, connection
/// string, and optional trace flag.
///
/// `--args` and `--args-named` are mutually exclusive at the Clap level
/// and exactly one of the two is required. The first is the
/// implementer-facing tagged-EvalValue codec; the second is the
/// embedder-facing bare-by-name codec that mirrors the JSON Schema
/// `morpholog schema` emits.
#[derive(clap::Args, Debug)]
pub(crate) struct ProposeArgs {
    /// Path to a `.morph` source file containing the programme.
    pub(crate) file: PathBuf,

    /// Transformation name within the parsed programme. Omitted in
    /// batch mode, where every row names its own.
    #[arg(required_unless_present = "batch", conflicts_with = "batch")]
    pub(crate) transformation: Option<String>,

    /// JSON array of arguments matching the transformation's parameter
    /// list. Each element is an `EvalValue` in the tagged form:
    /// `{"type":"subject","value":"..."}`, `{"type":"decimal",
    /// "value":"100"}`, `{"type":"quantity","value":{"amount":"100",
    /// "unit":"USD"}}`, `{"type":"bool","value":true}`, or
    /// `{"type":"collection","value":[...]}`. The implementer-facing
    /// codec; carries Polymorphic / Ambiguous / Collection inputs the
    /// schema cannot describe unambiguously.
    #[arg(
        long,
        conflicts_with_all = ["args_named", "batch"],
        required_unless_present_any = ["args_named", "batch"]
    )]
    pub(crate) args: Option<String>,

    /// JSON object keyed by parameter name with bare values matching
    /// the JSON Schema emitted by `morpholog schema`. The embedder-
    /// facing codec; strict (missing required, unknown keys, wrong
    /// types, and `null` all error). Refuses Polymorphic, Ambiguous,
    /// Unconstrained, and Collection parameters; use `--args` for
    /// those.
    #[arg(
        long,
        conflicts_with_all = ["args", "batch"],
        required_unless_present_any = ["args", "batch"]
    )]
    pub(crate) args_named: Option<String>,

    /// Subject identifying the actor under whose authority this
    /// transition is proposed. Wrapped as an `EvalValue::Subject` and
    /// persisted to `morpholog.audit.actor`. Omitted in batch mode,
    /// where every row carries its own.
    #[arg(long, required_unless_present = "batch", conflicts_with = "batch")]
    pub(crate) actor: Option<String>,

    /// Batch mode: a path to NDJSON rows (`-` for stdin), one
    /// transition per line as
    /// `{"transformation": "...", "actor": "...", "args_named": {...}}`
    /// (or `"args": [...]` in the tagged codec). Each row commits or
    /// rolls back on its own - an import is explicitly NOT
    /// all-or-nothing - and produces one NDJSON receipt on stdout in
    /// row order. Rejections and malformed rows are receipts, not
    /// process failures: the exit code is zero whenever every row was
    /// processed, reserving non-zero for operational failure.
    #[arg(long)]
    pub(crate) batch: Option<PathBuf>,

    #[command(flatten)]
    pub(crate) db: DatabaseArgs,

    /// When set, emit a structured per-statement trace alongside the
    /// outcome - the kernel's `propose_with_trace` shape on the wire.
    #[arg(long, conflicts_with_all = ["explain_on_reject", "batch"])]
    pub(crate) trace: bool,

    /// When set, a business rejection carries an `explanation` field:
    /// the same structured account `explain --json` produces, computed
    /// against the exact pre-state the gates evaluated - one snapshot,
    /// not a run-then-explain pair that can describe different states.
    /// Committed outcomes are unchanged; exit codes are unchanged.
    #[arg(long, conflicts_with = "trace")]
    pub(crate) explain_on_reject: bool,
}

/// Arguments for `explain`. The same source/transformation/args/actor
/// shape as [`ProposeArgs`] - it builds the identical `Transition` - but with
/// `--json` in place of `--trace`: explain's whole output already is the
/// interpreted trace, so prose-or-JSON is the only output choice.
///
/// `--args` and `--args-named` are mutually exclusive at the Clap level
/// and exactly one is required. Same semantics as `propose`: the first is
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
    /// See `propose --args` for the full codec description.
    #[arg(
        long,
        conflicts_with = "args_named",
        required_unless_present = "args_named"
    )]
    pub(crate) args: Option<String>,

    /// JSON object keyed by parameter name with bare values matching
    /// the JSON Schema emitted by `morpholog schema`. The embedder-
    /// facing codec; same strict semantics as `propose --args-named`.
    #[arg(long, conflicts_with = "args", required_unless_present = "args")]
    pub(crate) args_named: Option<String>,

    /// Subject identifying the actor under whose authority the explained
    /// transition is proposed. Wrapped as an `EvalValue::Subject`.
    #[arg(long)]
    pub(crate) actor: String,

    #[command(flatten)]
    pub(crate) db: DatabaseArgs,

    /// Emit the structured JSON `Explanation` instead of prose.
    #[arg(long)]
    pub(crate) json: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Inspect { what } => commands::inspect::run(what).await,
        Command::Check(args) => commands::check::run(args),
        Command::Propose(args) => commands::propose::run(args).await,
        Command::Explain(args) => commands::explain::run(args).await,
        Command::Outbox { what } => match what {
            OutboxCmd::Claim(args) => commands::outbox::claim(args).await,
            OutboxCmd::Complete(args) => commands::outbox::complete(args).await,
            OutboxCmd::Release(args) => commands::outbox::release(args).await,
        },
        Command::Schema(args) => commands::schema::run(args),
        Command::Verify(args) => commands::verify::run(args).await,
        Command::Checkpoint(args) => commands::checkpoint::run(args).await,
        Command::Evaluate(args) => commands::evaluate::run(args).await,
        Command::Evidence { what } => commands::evidence::run(what).await,
        Command::Generate {
            what: GenerateCmd::PythonClient(args),
        } => commands::generate::run(&args),
        Command::Generate {
            what: GenerateCmd::Views(args),
        } => commands::generate_views::run(&args),
        Command::Refresh {
            what: RefreshCmd::Derived(args),
        } => commands::refresh::run(&args).await,
        Command::Hash(args) => commands::hash::run(args),
        Command::Init(args) => commands::init::run(args).await,
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
    /// that landed on the resulting inspect-subcommand args.
    fn parsed_url(argv: &[&str]) -> String {
        let cli = Cli::parse_from(argv);
        let Command::Inspect { what } = cli.command else {
            panic!("expected Command::Inspect, got {:?}", cli.command);
        };
        match what {
            Inspect::Claims(args) => args.db.database_url,
            Inspect::Audit(args) => args.db.database_url,
            Inspect::Rejections(args) => args.database_url,
            Inspect::Outbox(args) => args.db.database_url,
            Inspect::Coverage(args) => args.db.database_url,
            Inspect::Derived(_) => {
                panic!("use the dedicated inspect-derived parse tests, not parsed_url")
            }
            Inspect::Controls(_) => {
                panic!("inspect controls is static; it takes no database URL")
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
    /// `InspectAuditArgs` does not declare the `--as-of` flag. Pins
    /// the design decision that as-of does not apply to the audit
    /// subcommand: the audit table IS the chronological record, and
    /// the tail's coordinate is `--after`.
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
            "examples/03_double_entry_ledger/ledger.morph",
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
        assert_eq!(
            args.file,
            std::path::PathBuf::from("examples/03_double_entry_ledger/ledger.morph")
        );
        assert_eq!(args.transformation.as_deref(), Some("post_simple_entry"));
        assert_eq!(args.args.as_deref(), Some("[]"));
        assert!(args.args_named.is_none());
        assert_eq!(args.actor.as_deref(), Some("jordan"));
        assert_eq!(args.db.database_url, "postgres:///morpholog_dev");
    }

    /// `propose --args-named '{...}'` parses with `args_named: Some(...)`
    /// and `args: None`. Confirms the new flag plumbs through.
    #[test]
    fn propose_with_args_named_parses_into_the_named_slot() {
        let cli = Cli::parse_from([
            "morpholog",
            "propose",
            "examples/03_double_entry_ledger/ledger.morph",
            "post_simple_entry",
            "--args-named",
            r#"{"trade":"a"}"#,
            "--actor",
            "jordan",
            "--database-url",
            "postgres:///morpholog_dev",
        ]);
        let Command::Propose(args) = cli.command else {
            panic!("expected Propose, got {:?}", cli.command);
        };
        assert!(args.args.is_none(), "--args should not be set");
        assert_eq!(args.args_named.as_deref(), Some(r#"{"trade":"a"}"#));
    }

    /// Passing BOTH `--args` and `--args-named` must be rejected at
    /// Clap-parse time so the run path never sees an ambiguous
    /// request shape.
    #[test]
    fn propose_with_both_args_codecs_errors() {
        let err = Cli::try_parse_from([
            "morpholog",
            "propose",
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
    fn propose_missing_args_flag_errors() {
        let err = Cli::try_parse_from([
            "morpholog",
            "propose",
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
    fn propose_missing_actor_flag_errors() {
        let err = Cli::try_parse_from([
            "morpholog",
            "propose",
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
    fn propose_missing_positional_errors() {
        let err = Cli::try_parse_from([
            "morpholog",
            "propose",
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
        assert_eq!(args.db.database_url, "postgres:///morpholog_dev");
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

    /// `propose --trace` parses to a `ProposeArgs` with `trace: true`. All other
    /// fields keep their existing behaviour.
    #[test]
    fn propose_with_trace_flag_parses() {
        let cli = Cli::parse_from([
            "morpholog",
            "propose",
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
        let Command::Propose(args) = cli.command else {
            panic!("expected Propose, got {:?}", cli.command);
        };
        assert!(args.trace, "expected trace flag to be set");
        assert_eq!(
            args.transformation.as_deref(),
            Some("create_net_settlement")
        );
        assert_eq!(args.actor.as_deref(), Some("jordan"));
    }

    /// Without `--trace`, `ProposeArgs.trace` defaults to false. The non-trace
    /// path must not be affected by the flag.
    #[test]
    fn propose_without_trace_flag_defaults_to_false() {
        let cli = Cli::parse_from([
            "morpholog",
            "propose",
            "examples/01_settlement_netting/netting.morph",
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

    /// `parse` is no longer a subcommand - it folded into
    /// `check --ir`. Pins both the removal and the fold.
    #[test]
    fn parse_is_gone_and_check_ir_replaces_it() {
        let err = Cli::try_parse_from(["morpholog", "parse", "demo.morph"])
            .expect_err("parse must no longer be a subcommand");
        assert_eq!(err.kind(), ErrorKind::InvalidSubcommand);

        let cli = Cli::parse_from(["morpholog", "check", "demo.morph", "--ir"]);
        let Command::Check(args) = cli.command else {
            panic!("expected Command::Check, got {:?}", cli.command);
        };
        assert!(args.ir);
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
    fn check_missing_file_argument_errors() {
        let err =
            Cli::try_parse_from(["morpholog", "check"]).expect_err("expected clap parse error");
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
        assert_eq!(args.db.database_url, "postgres:///morpholog_dev");
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
            args.file.expect("file is present").to_string_lossy(),
            "examples/10_trade_lifecycle/trade_lifecycle.morph"
        );
    }

    /// `schema --result` needs no `.morph` file (the envelope contract
    /// is programme-independent) and conflicts with every per-programme
    /// mode.
    #[test]
    fn schema_result_parses_without_a_file() {
        let cli = Cli::parse_from(["morpholog", "schema", "--result"]);
        let Command::Schema(args) = cli.command else {
            panic!("expected Command::Schema, got {:?}", cli.command);
        };
        assert!(args.result);
        assert!(args.file.is_none());
    }

    #[test]
    fn schema_result_conflicts_with_per_programme_modes() {
        for extra in [
            vec!["file.morph", "capture_trade"],
            vec!["file.morph", "--intent", "X"],
            vec!["file.morph", "--all"],
        ] {
            let mut argv = vec!["morpholog", "schema", "--result"];
            argv.extend(extra.clone());
            let err = Cli::try_parse_from(argv)
                .expect_err("--result with a per-programme mode should conflict");
            assert_eq!(err.kind(), ErrorKind::ArgumentConflict, "case {extra:?}");
        }
    }

    /// Without `--result`, schema still demands a file plus exactly one
    /// mode - the pre-existing contract survives the file becoming
    /// optional.
    #[test]
    fn schema_without_result_still_requires_a_file_and_mode() {
        let err = Cli::try_parse_from(["morpholog", "schema"])
            .expect_err("bare schema should be missing required args");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
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

    #[test]
    fn generate_views_defaults_to_the_morpholog_views_schema() {
        let cli = Cli::parse_from(["morpholog", "generate", "views", "model.morph"]);
        let Command::Generate {
            what: GenerateCmd::Views(args),
        } = cli.command
        else {
            panic!("expected generate views, got {:?}", cli.command);
        };
        assert_eq!(args.schema, "morpholog_views");
        assert!(args.out.is_none());
    }

    #[test]
    fn generate_views_accepts_schema_and_out() {
        let cli = Cli::parse_from([
            "morpholog",
            "generate",
            "views",
            "model.morph",
            "--schema",
            "analytics",
            "--out",
            "views.sql",
        ]);
        let Command::Generate {
            what: GenerateCmd::Views(args),
        } = cli.command
        else {
            panic!("expected generate views, got {:?}", cli.command);
        };
        assert_eq!(args.schema, "analytics");
        assert_eq!(args.out.as_deref(), Some(std::path::Path::new("views.sql")));
    }

    #[test]
    fn refresh_derived_takes_a_file_and_database_url() {
        let cli = Cli::parse_from([
            "morpholog",
            "refresh",
            "derived",
            "model.morph",
            "--database-url",
            "postgres:///morpholog_dev",
        ]);
        let Command::Refresh {
            what: RefreshCmd::Derived(args),
        } = cli.command
        else {
            panic!("expected refresh derived, got {:?}", cli.command);
        };
        assert_eq!(args.file, std::path::Path::new("model.morph"));
        assert_eq!(args.db.database_url, "postgres:///morpholog_dev");
    }

    #[test]
    fn refresh_derived_requires_a_file() {
        let err = Cli::try_parse_from(["morpholog", "refresh", "derived"])
            .expect_err("missing file should error");
        assert_eq!(err.kind(), ErrorKind::MissingRequiredArgument);
    }
}
