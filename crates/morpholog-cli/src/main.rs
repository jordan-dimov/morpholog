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
        The commands, by what you are doing:\n  \
        authoring a programme    check, hash, schema, generate\n  \
        governing state          propose, explain, inspect, session\n  \
        running a deployment     init, refresh, outbox\n  \
        proving the record       audit\n  \
        discovering rules        evaluate\n\n\
        Only invariants and transformations are first-class in Morpholog: propose\n\
        is the one verb by which governed state changes, and everything else\n\
        either prepares a programme or observes what the rules have done.\n\n\
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
    /// never drops, never migrates. `--least-privilege` additionally
    /// locks the governed tables down to dedicated writer and reader
    /// roles, so the governed path is the only way in by default.
    Init(InitArgs),

    /// Bring an existing database up to the schema this binary expects.
    ///
    /// The numbered migrations are embedded here, like the schema itself,
    /// so upgrading needs only the released artifact - there is no SQL to
    /// fetch from a source tree. Applies every migration the database has
    /// not recorded, in order, each in its own transaction with its record
    /// written alongside; already-current databases are left alone.
    ///
    /// `--check` reports and changes nothing, exiting non-zero when the
    /// database is behind - a deploy gate can ask before a workload does.
    ///
    /// Companion to `init`, not part of it: `init` provisions and is safe
    /// to run against a live database precisely because it never alters one.
    Migrate(MigrateArgs),

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

    /// Serve propose and the page reads over stdio, resident.
    ///
    /// Parses and validates the programme once, holds one warm
    /// database connection, and answers NDJSON requests on stdin with
    /// the same JSON envelopes the one-shot commands print - one
    /// compact line per request, in order. The escape from the
    /// per-call subprocess and connection tax when an embedder drives
    /// many operations: a propose request answers with the batch
    /// receipt shape, the claims and derived reads with their pinned
    /// arrays, and a per-request failure with an error receipt
    /// carrying a stable `code`. The first line out is `status:
    /// "ready"` with the programme's canonical hash - the programme
    /// is pinned for the session's lifetime. EOF on stdin ends the
    /// session cleanly; operational failure aborts it with a
    /// non-zero exit. Protocol reference in
    /// docs/embedder-integration.md.
    Session(SessionArgs),

    /// Propose several changes as one decision: every act or none.
    ///
    /// Reads NDJSON acts (`--acts -` for stdin) in the batch row shape
    /// and applies them in order inside one transaction - each act's
    /// gates, rules and actor authorisation see what the acts before
    /// it staged - then commits all of them or none. Prints one JSON
    /// object: every act's receipt on commit (exit 0); the refusing
    /// act, its rule and witness on a refusal, with nothing written
    /// (exit 1); or a coded error - `serialization_failure` is the one
    /// safe to re-submit whole, `not_committed` once its cause is
    /// fixed, and `commit_outcome_unknown` only after reading the
    /// record (exit 3). Not `propose --batch`, which is the import
    /// shape: one receipt per row and carry on.
    Transact(TransactArgs),

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

    /// Everything about the audit log: prove it, anchor it, carry it.
    ///
    /// The audit log is the record a regulator reads and an auditor
    /// challenges, so the commands that establish its integrity live
    /// together: `verify` checks it against the claims table and its
    /// own Merkle tree, `checkpoint` records an anchor to hold outside
    /// the database, `export` writes a portable pack, `verify-pack`
    /// checks such a pack with no database at all, and `keygen` makes
    /// the signing key that turns an anchor from tamper-evident into
    /// attributable.
    Audit {
        #[command(subcommand)]
        what: AuditCmd,
    },

    /// Prepare a database for a programme, beyond the schema.
    ///
    /// `indexes` reconciles the partial expression indexes a programme's
    /// compiled invariants can seek on: creates what is missing, repairs
    /// an interrupted build, reports a conflict for an operator, and
    /// leaves an equivalent index someone else made alone. Correctness
    /// never depends on it - the checks are right without any index,
    /// only slower.
    Provision {
        #[command(subcommand)]
        what: ProvisionCmd,
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

    /// Compare the would-be output against what is already at `--out`
    /// and write nothing: exit zero when they agree, non-zero when any
    /// file differs or is missing, naming every one. The drift gate both
    /// consumer repos wrote by hand as regenerate-into-a-tempdir-and-diff.
    #[arg(long)]
    pub(crate) check: bool,
}

/// The connection-string flag every database-backed subcommand
/// shares, declared once and `#[command(flatten)]`ed in. Subcommands
/// whose only input is the connection take this struct directly.
/// `migrate`: apply the migrations this binary carries.
#[derive(clap::Args, Debug)]
pub(crate) struct MigrateArgs {
    #[command(flatten)]
    pub(crate) db: DatabaseArgs,
    /// Report what is pending and change nothing. Exits non-zero when the
    /// database is behind, so a deploy step can gate on it.
    #[arg(long)]
    pub(crate) check: bool,
}

/// `inspect rejections`: the log, newest first, bounded.
///
/// Newest-first because the question is always "what just refused", and
/// bounded because this table grows with every refusal - an unbounded read
/// is the one query that fails when it is most needed, during a storm.
#[derive(clap::Args, Debug)]
pub(crate) struct RejectionsArgs {
    #[command(flatten)]
    pub(crate) db: DatabaseArgs,
    /// How many refusals to return, newest first. Raise it to reach further
    /// back; there is no cursor yet, so depth comes from raising this.
    #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(u32).range(1..))]
    pub(crate) limit: u32,
}

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

    /// Path to a checkpoint JSON file (as printed by `audit checkpoint`),
    /// held outside the database. The audit tree is verified to still match
    /// it - the check a coordinated rewrite of audit + checkpoints cannot
    /// pass. Omit to verify only internal checkpoint consistency.
    #[arg(long)]
    pub(crate) anchor_file: Option<std::path::PathBuf>,

    /// Compliance mode: require every checkpoint in the chain to be
    /// signed. An unsigned checkpoint then fails (`signature_required`).
    /// Off by default - signing is opt-in.
    #[arg(long, conflicts_with = "require_signatures_from")]
    pub(crate) require_signatures: bool,

    /// Require signatures only from this tree size on: checkpoints
    /// before it - honest history from before signing began - are not
    /// asked. `--require-signatures` is this at zero.
    #[arg(long, value_name = "TREE_SIZE", value_parser = clap::value_parser!(i64).range(0..))]
    pub(crate) require_signatures_from: Option<i64>,

    /// Pin the signing key: a file holding the `ed25519-pub:<hex>` key
    /// `audit keygen` wrote. Every checkpoint the policy covers must
    /// then carry a signature by this key (`signature_required` if it
    /// carries none, `signing_key_required` if none is by this key),
    /// on top of that key being authorised in the log - the pin narrows
    /// which authorised signer you accept, it never admits an
    /// unauthorised one. A signature on the anchor you hold counts for
    /// its checkpoint. Implies requiring signatures.
    #[arg(long, value_name = "FILE")]
    pub(crate) require_signing_key: Option<std::path::PathBuf>,

    /// Trust anchors for the external witnesses checkpoints carry: a PEM
    /// file of the timestamp authorities' CA certificates. A witness whose
    /// token chains to one of them reports `verified`; one that does not,
    /// `untrusted`. Without this file every intact witness is
    /// `unverified` - present and consistent, but vouched for by no one
    /// you named. A witness that does not match its checkpoint is
    /// `invalid` either way, and fails the command.
    #[arg(long, value_name = "FILE")]
    pub(crate) trusted_tsa_file: Option<std::path::PathBuf>,

    /// Also verify the generated SQL view surface in this schema: each
    /// catalogued view's live definition (as PostgreSQL stores it) must
    /// hash to the seal recorded when the views were applied, so a view
    /// redefined in place under the same name is evident. The report
    /// gains a `views` verdict; a tampered surface exits one. A surface
    /// generated before sealing reports `not_sealed` and passes.
    #[arg(long, value_name = "SCHEMA")]
    pub(crate) views_schema: Option<String>,
}

/// Arguments for `checkpoint`: the connection, an optional Ed25519
/// signing key, and the writer-set assertion. With `--signing-key`
/// the new tree head is signed; both `--signing-key` and `--key-id`
/// must be supplied together.
#[derive(clap::Args, Debug)]
pub(crate) struct CheckpointArgs {
    #[command(flatten)]
    pub(crate) db: DatabaseArgs,

    /// PKCS#8 PEM private key to sign the new checkpoint's tree head.
    #[arg(long, requires = "key_id")]
    pub(crate) signing_key: Option<std::path::PathBuf>,

    /// The key id the signature is published under; it must match an
    /// authorised `AuditSigningKey(key_id, "audit_checkpoint_v1", <public
    /// key>)` claim as of the checkpoint's prefix, or signing is refused.
    #[arg(long, requires = "signing_key")]
    pub(crate) key_id: Option<String>,

    #[command(flatten)]
    pub(crate) writers: WriterRoleArgs,

    /// Also have an outside authority witness the new head:
    /// `rfc3161:<url>` posts a timestamp request for it to that RFC 3161
    /// authority and stores the exact response on the checkpoint, so a
    /// later verifier can show the head existed no later than the
    /// authority's time. Repeat for several authorities: every one is
    /// attempted and each response stored as it arrives. The checkpoint
    /// is recorded first and printed whatever the authorities do; any
    /// failed submission exits one and names the `audit witness` command
    /// that retries exactly those. Skipped when no new rows were
    /// checkpointed.
    #[arg(long, value_name = "SCHEME:URL")]
    pub(crate) witness: Vec<commands::witness::WitnessTarget>,
}

/// Arguments for `audit witness`: which recorded checkpoint, and which
/// authorities.
#[derive(clap::Args, Debug)]
pub(crate) struct WitnessArgs {
    #[command(flatten)]
    pub(crate) db: DatabaseArgs,

    /// The recorded checkpoint to have witnessed, by its tree size (as
    /// `audit checkpoint` printed it).
    #[arg(long, value_parser = clap::value_parser!(i64).range(0..))]
    pub(crate) tree_size: i64,

    /// `rfc3161:<url>` - the RFC 3161 authority to post the request to.
    /// Repeat for several: every one is attempted, each response is
    /// stored as it arrives, and the command exits one naming any that
    /// failed and the one command that retries exactly those.
    #[arg(long, value_name = "SCHEME:URL", required = true)]
    pub(crate) witness: Vec<commands::witness::WitnessTarget>,
}

/// Arguments for `keygen`: where to write the new Ed25519 keypair.
#[derive(clap::Args, Debug)]
pub(crate) struct KeygenArgs {
    /// Where to write the PKCS#8 PEM private key. Keep it secret.
    #[arg(long)]
    pub(crate) private_out: std::path::PathBuf,

    /// Where to write the `ed25519-pub:<hex>` public key.
    #[arg(long)]
    pub(crate) public_out: std::path::PathBuf,
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

    /// Split the replay into a training slice (everything at or before
    /// this boundary) and a held-out test slice (everything after),
    /// reported separately - the overfitting guard: a rule discovered
    /// on the training slice is honestly judged on history it never
    /// saw. Takes a transition id or an RFC 3339 timestamp. One
    /// continuous replay: rule state carries across the boundary and
    /// each violation counts in the slice that introduced it. Not
    /// available with `--packs`, where each per-case pack is already
    /// the unit a harness assigns to a slice.
    #[arg(long, conflicts_with = "packs")]
    pub(crate) train_until: Option<String>,
}

/// The audit log's integrity, end to end. `verify`, `checkpoint` and
/// `export` read the database; `verify-pack` is deliberately offline -
/// it takes no connection string, only files - and `keygen` touches
/// neither.
#[derive(clap::Subcommand, Debug)]
pub(crate) enum ProvisionCmd {
    /// Reconcile the indexes a programme's compiled invariants can seek
    /// on. Prints one line per index with its action - KEEP, CREATE,
    /// REPAIR INVALID, SATISFIED EXTERNALLY, STALE, CONFLICT - and exits
    /// non-zero on a conflict, which needs an operator. Builds run
    /// concurrently, so the claims table stays writable throughout.
    Indexes(ProvisionIndexesArgs),
}

#[derive(clap::Args, Debug)]
pub(crate) struct ProvisionIndexesArgs {
    /// The `.morph` programme whose compiled invariants set the requirement.
    pub(crate) file: std::path::PathBuf,

    #[command(flatten)]
    pub(crate) db: DatabaseArgs,

    /// Print the plan and change nothing.
    #[arg(long)]
    pub(crate) dry_run: bool,

    /// Also drop managed indexes no programme requires any more.
    #[arg(long)]
    pub(crate) prune: bool,
}

#[derive(clap::Subcommand, Debug)]
pub(crate) enum AuditCmd {
    /// Check that the claims table and the audit log still agree.
    ///
    /// The two tables are independent records of the same history, so
    /// replaying the audit log must land on exactly the current
    /// claims - a difference is evidence that one was modified outside
    /// the runtime. Also recomputes the audit Merkle tree against its
    /// checkpoints. Pass `--anchor-file` with a checkpoint saved
    /// earlier to detect a coordinated edit of the log AND the
    /// checkpoint table - the only check an attacker with full
    /// database access cannot defeat. Read-only; exits one on
    /// divergence.
    Verify(VerifyArgs),

    /// Record a tamper-evident checkpoint over the audit log.
    ///
    /// Computes the RFC 6962 Merkle root of the committed prefix and
    /// chains it onto the previous checkpoint. Save the printed JSON
    /// outside the database as an anchor; a later `audit verify
    /// --anchor-file` against it catches any rewrite. With
    /// `--signing-key` the tree head is signed, so the anchor is
    /// attributable as well as tamper-evident.
    Checkpoint(CheckpointArgs),

    /// Have an outside authority witness a recorded checkpoint.
    ///
    /// Posts an RFC 3161 timestamp request over the checkpoint's head
    /// and stores the authority's exact response on it, after checking
    /// the response is over this head. A later `audit verify` or
    /// `verify-pack` judges the stored token against the authorities
    /// the verifier trusts. Same as `audit checkpoint --witness`, for a
    /// head recorded earlier or a submission that failed then.
    Witness(WitnessArgs),

    /// Export a portable evidence pack as JSON (redirect to a file).
    ///
    /// A complete checkpointed prefix by default; with
    /// `--from-anchor`/`--from-tree-size` the window between that
    /// earlier checkpoint and the covering one, proving it extends the
    /// earlier anchor; with `--transition` a selective pack of just
    /// those rows. A prefix or window pack carries the full audit
    /// detail (actors, arguments, claims, intents) - it is not
    /// selective disclosure and may hold confidential business data.
    Export(EvidenceExportArgs),

    /// Verify an exported pack offline, with no database at all.
    ///
    /// Recomputes the Merkle root from the pack's own rows and checks
    /// it against the pack's checkpoints, and against an external
    /// `--anchor-file` if given. This is the check a recipient runs:
    /// the database that produced the pack is not consulted, and
    /// cannot be. Exits one on any tamper, divergence, or malformed
    /// pack.
    VerifyPack(EvidenceVerifyArgs),

    /// Generate an Ed25519 audit-signing keypair.
    ///
    /// Writes the private key as PKCS#8 PEM (keep it secret) and the
    /// public key as `ed25519-pub:<hex>` - the value you admit as an
    /// `AuditSigningKey` claim and give to verifiers. No database.
    Keygen(KeygenArgs),
}

/// Arguments for `audit export`. With no `--from-*` it exports a
/// complete prefix; with one it exports the window between that earlier
/// checkpoint and the covering one.
#[derive(clap::Args, Debug)]
pub(crate) struct EvidenceExportArgs {
    #[command(flatten)]
    pub(crate) db: DatabaseArgs,

    /// Cover the checkpoint whose `tree_size` equals this value, instead of
    /// the latest. In window mode this is the window's end (`to`). Must
    /// match an existing checkpoint exactly.
    #[arg(long)]
    pub(crate) tree_size: Option<i64>,

    /// Export a WINDOW starting at the checkpoint in this anchor file (as
    /// printed by `audit checkpoint`, the prior period's externally-held anchor):
    /// the pack proves the covered range extends it. The trusted start is
    /// the whole checkpoint, which is why a file is the main path.
    #[arg(long, conflicts_with = "from_tree_size")]
    pub(crate) from_anchor: Option<std::path::PathBuf>,

    /// Export a window starting at this checkpoint `tree_size` - a
    /// database-side convenience for `--from-anchor`. The pack still carries
    /// the full start checkpoint; prefer `--from-anchor` for the trust object.
    #[arg(long)]
    pub(crate) from_tree_size: Option<i64>,

    /// Export a SELECTIVE pack disclosing only this transition (repeat the
    /// flag for several). Each disclosed row carries a proof that it is
    /// genuinely at its position under the covering checkpoint; undisclosed
    /// rows are absent entirely. The pack proves the disclosed rows
    /// authentic - it does not prove the selection complete, and the
    /// disclosed positions and count are themselves visible.
    #[arg(long, conflicts_with_all = ["from_anchor", "from_tree_size"])]
    pub(crate) transition: Vec<uuid::Uuid>,
}

/// Arguments for `audit verify-pack`: a pack file and an optional external
/// anchor. No connection string - the offline guarantee is in the shape.
#[derive(clap::Args, Debug)]
pub(crate) struct EvidenceVerifyArgs {
    /// Path to a pack JSON file (as printed by `audit export`).
    pub(crate) pack_file: std::path::PathBuf,

    /// Path to a checkpoint JSON file (as printed by `audit checkpoint`), held
    /// outside the database - the check a coordinated rewrite cannot pass.
    /// For a prefix pack the checkpoint at the anchor's `tree_size` in the
    /// pack's chain must match it (an older anchor is fine, as long as the
    /// pack still covers it); a window pack's anchor is its from-checkpoint;
    /// a selective pack's anchor is its one covering checkpoint. Omit to
    /// verify only the pack's internal consistency.
    #[arg(long)]
    pub(crate) anchor_file: Option<std::path::PathBuf>,

    /// Compliance mode: require every checkpoint in the pack to be signed
    /// (`signature_required` otherwise). Off by default.
    #[arg(long, conflicts_with = "require_signatures_from")]
    pub(crate) require_signatures: bool,

    /// Require signatures only from this tree size on; earlier
    /// checkpoints are not asked. `--require-signatures` is this at zero.
    #[arg(long, value_name = "TREE_SIZE", value_parser = clap::value_parser!(i64).range(0..))]
    pub(crate) require_signatures_from: Option<i64>,

    /// Pin the signing key (a file holding the `ed25519-pub:<hex>` key
    /// `audit keygen` wrote): every covered checkpoint must carry a
    /// signature by this key (`signature_required` if it carries none,
    /// `signing_key_required` if none is by this key), on top of the
    /// key being authorised in the log; a signature on the anchor you
    /// hold counts for its checkpoint. Complete-prefix packs only - a
    /// window or selective pack cannot establish key authority, so on
    /// an intact one the pin is refused rather than weakened to a bare
    /// cryptographic match; a broken pack reports as broken first.
    /// Implies requiring signatures.
    #[arg(long, value_name = "FILE")]
    pub(crate) require_signing_key: Option<std::path::PathBuf>,

    /// Also report what the external witnesses on the pack's checkpoints
    /// prove. The output becomes `{"verdict": <the pack verdict>,
    /// "witnesses": ...}`; without this flag it stays the bare verdict.
    #[arg(long)]
    pub(crate) witnesses: bool,

    /// Trust anchors for those witnesses: a PEM file of the timestamp
    /// authorities' CA certificates (`verified` if a token chains to one,
    /// `untrusted` if not, `unverified` without the file). Implies
    /// `--witnesses`. An `invalid` witness fails the command.
    #[arg(long, value_name = "FILE")]
    pub(crate) trusted_tsa_file: Option<std::path::PathBuf>,
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

    /// DESTRUCTIVE: drop the `morpholog` schema and everything in it,
    /// then provision it fresh. For a development database that wants
    /// a clean slate - the `DROP SCHEMA morpholog CASCADE` two repos
    /// were shelling out to psql for. Requires --i-know-this-deletes-data.
    #[arg(long)]
    pub(crate) reset: bool,

    /// Acknowledge that --reset destroys every claim, audit row, and
    /// outbox entry in the target database. Required with --reset, and
    /// meaningless without it: a fat-fingered production URL should not
    /// be survivable by default.
    #[arg(long)]
    pub(crate) i_know_this_deletes_data: bool,

    /// Also provision the least-privilege floor: dedicated writer and
    /// reader group roles, PUBLIC revoked from the governed tables, and
    /// the audit log append-only even for the writer. The report names
    /// the membership grants the operator still runs. Idempotent;
    /// combine with --skip-if-exists to retrofit an existing database.
    #[arg(long)]
    pub(crate) least_privilege: bool,
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
    AtOrBefore(jiff::Timestamp),
}

impl std::str::FromStr for AsOf {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if let Ok(tid) = Uuid::parse_str(s) {
            return Ok(AsOf::Transition(tid));
        }
        if let Ok(at) = morpholog_postgres::wire_time::parse(s) {
            return Ok(AsOf::AtOrBefore(at));
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
    /// against a past state via `--as-of`. `--named` decodes each
    /// row's arguments by declared field name under the same
    /// programme's authority. Read-only: no claims are written, no
    /// audit row is produced.
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
    Rejections(RejectionsArgs),
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

    #[command(flatten)]
    pub(crate) writers: WriterRoleArgs,
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

    /// Optional, repeatable: return only claims whose named field equals
    /// this value (`--where invoice_id=inv_2026_03`). The comparison runs
    /// in the database, so rows that cannot match are never transferred
    /// or decoded - it saves carrying the predicate, not scanning it,
    /// since no index covers argument positions.
    ///
    /// Field names come from a declaration, so this needs `--named` and
    /// exactly one `--predicate` - without them a field name has no
    /// meaning. An undeclared field is a hard error naming the ones that
    /// exist, never an empty result. Equality only, and repeats are
    /// conjunctive; ranges and disjunction wait for a case that needs
    /// them. Composes with `--as-of`, where the filter applies to the
    /// replayed state.
    #[arg(long = "where", value_name = "FIELD=VALUE")]
    pub(crate) filter: Vec<String>,
}

/// The writer-set assertion both watermark consumers share
/// (`inspect audit`, `audit checkpoint`).
#[derive(clap::Args, Debug)]
pub(crate) struct WriterRoleArgs {
    /// Assert the session roles that write audit (repeatable), so the
    /// resume horizon is computed over their sessions only - for
    /// managed PostgreSQL, where the platform's own sessions are
    /// hidden and `pg_read_all_stats` cannot be granted. The
    /// assertion is verified against the catalog (every non-superuser
    /// role that can write `morpholog.audit` must be asserted);
    /// superuser writes are the residue the flag explicitly accepts,
    /// and role grants, memberships, and login attributes must stay
    /// unchanged until the command establishes its read snapshot.
    #[arg(long = "writer-role", value_name = "ROLE")]
    pub(crate) writer_role: Vec<String>,
}

impl WriterRoleArgs {
    /// `None` when unasserted - the adapter's lossless-or-loud default.
    pub(crate) fn as_writers(&self) -> Option<&[String]> {
        (!self.writer_role.is_empty()).then_some(self.writer_role.as_slice())
    }
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

    /// Decode each row's arguments by declared field name - the same
    /// named shape as `inspect claims --named`, under the authority of
    /// the programme already named by FILE (so the flag takes no
    /// argument here).
    #[arg(long)]
    pub(crate) named: bool,

    /// Optional, repeatable: return only rows whose named field equals
    /// this value, resolved against the derived claim's own head under
    /// FILE. Same equality-only, conjunctive contract as `inspect claims
    /// --where`, and the same hard error for an undeclared field.
    ///
    /// Unlike the claims filter, this one runs after enumeration: a
    /// derived view is computed from claims, so the work happens either
    /// way and this narrows the answer rather than the effort.
    #[arg(long = "where", value_name = "FIELD=VALUE")]
    pub(crate) filter: Vec<String>,
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
    /// The shared [`morpholog_postgres::OutboxStatus`] this flag names,
    /// or `None` for the `All` filter (which drops the `WHERE status =
    /// ?` clause). Typed end to end: no string mapping to drift from
    /// the database vocabulary.
    pub(crate) fn db_filter(self) -> Option<morpholog_postgres::OutboxStatus> {
        use morpholog_postgres::OutboxStatus;
        match self {
            InspectOutboxStatus::Pending => Some(OutboxStatus::Pending),
            InspectOutboxStatus::InProgress => Some(OutboxStatus::InProgress),
            InspectOutboxStatus::Delivered => Some(OutboxStatus::Delivered),
            InspectOutboxStatus::Failed => Some(OutboxStatus::Failed),
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

    /// Another programme deployed against the same database. Every
    /// predicate this file admits or retracts that the other also
    /// admits or retracts is reported as a hint on this file's
    /// writing transformation: two programmes writing one predicate
    /// each escape the other's gates. Repeatable; each file must
    /// itself parse and validate (its own lints are its own `check`'s
    /// business).
    #[arg(long, value_name = "FILE")]
    pub(crate) against: Vec<PathBuf>,

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
/// Args for `session`: the programme file to pin for the session's
/// lifetime, and the database connection.
#[derive(clap::Args, Debug)]
pub(crate) struct TransactArgs {
    /// Path to a `.morph` source file containing the programme.
    pub(crate) file: PathBuf,

    /// Path to NDJSON acts (`-` for stdin), one per line as
    /// `{"transformation": ..., "actor": ..., "args_named": {...}}`
    /// (or `"args"` for the tagged codec), applied in order.
    #[arg(long, value_name = "PATH")]
    pub(crate) acts: PathBuf,

    #[command(flatten)]
    pub(crate) db: DatabaseArgs,
}

#[derive(clap::Args, Debug)]
pub(crate) struct SessionArgs {
    /// Path to a `.morph` source file containing the programme. Read
    /// once at startup; the ready line's `model_hash` is its
    /// canonical rules-identity hash.
    pub(crate) file: PathBuf,

    #[command(flatten)]
    pub(crate) db: DatabaseArgs,
}

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

/// The one place this binary decides the outcome of a dispatched
/// command.
///
/// Commands return their outcome rather than calling
/// `std::process::exit` from wherever a diagnostic was printed, which
/// is what makes them composable and testable in-process. Their pinned
/// exit-code semantics are unchanged - success 0, failure 1 - only
/// where they are enacted moves.
///
/// Argument parsing is deliberately NOT routed through here. Clap owns
/// that boundary along with its own conventions: a usage error exits
/// 2, `--help` and `--version` exit 0 having printed to stdout. Taking
/// it over would mean re-implementing those semantics to keep them
/// identical, for no gain - a command that never ran has nothing to
/// report.
#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        // The command has already rendered its diagnostics; printing
        // `Error: ...` on top would say the same thing twice, in a
        // worse voice.
        Err(err) if err.is::<commands::AlreadyReported>() => std::process::ExitCode::FAILURE,
        Err(err) => {
            // Byte-identical to what `Result`'s own `Termination` used
            // to print when `main` returned it.
            eprintln!("Error: {err:?}");
            std::process::ExitCode::from(exit_code_for(&err))
        }
    }
}

/// Every failure exits 1 except the one a caller must treat
/// differently: a commit whose outcome could not be proven.
fn exit_code_for(err: &anyhow::Error) -> u8 {
    if err.is::<commands::CommitOutcomeUnknown>() {
        commands::EXIT_COMMIT_OUTCOME_UNKNOWN
    } else {
        1
    }
}

async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Inspect { what } => commands::inspect::run(what).await,
        Command::Check(args) => commands::check::run(args),
        Command::Propose(args) => commands::propose::run(args).await,
        Command::Transact(args) => commands::transact::run(args).await,
        Command::Session(args) => commands::session::run(args).await,
        Command::Explain(args) => commands::explain::run(args).await,
        Command::Outbox { what } => match what {
            OutboxCmd::Claim(args) => commands::outbox::claim(args).await,
            OutboxCmd::Complete(args) => commands::outbox::complete(args).await,
            OutboxCmd::Release(args) => commands::outbox::release(args).await,
        },
        Command::Schema(args) => commands::schema::run(args),
        Command::Evaluate(args) => commands::evaluate::run(args).await,
        Command::Provision { what } => match what {
            ProvisionCmd::Indexes(args) => commands::provision::indexes(args).await,
        },
        Command::Audit { what } => match what {
            AuditCmd::Verify(args) => commands::verify::run(args).await,
            AuditCmd::Checkpoint(args) => commands::checkpoint::run(args).await,
            AuditCmd::Witness(args) => commands::witness::run(args).await,
            AuditCmd::Export(args) => commands::evidence::export(args).await,
            AuditCmd::VerifyPack(args) => commands::evidence::verify(args),
            AuditCmd::Keygen(args) => commands::keygen::run(&args),
        },
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
        Command::Migrate(args) => commands::migrate::run(args).await,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod cli_tests;
