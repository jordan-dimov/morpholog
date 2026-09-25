//! Morpholog scale-pressure benchmark.
//!
//! How the runtime behaves as state grows and as proposals contend. Each
//! subcommand is one scenario:
//!
//! - `write` times one proposal on top of N entries.
//! - `read` times the three phases of the read path over the same fixture.
//! - `as-of` times replaying a fabricated audit log up to a chosen point.
//! - `contend` times concurrent proposers, each with its own retry loop.
//! - `import` times N sequential commits from an empty book.
//! - `wide` times a proposal and a read on a predicate with many arguments.
//! - `suite` runs the fixed case matrix and prints one table
//!   (docs/benchmarking.md explains how to use it).
//!
//! Every scenario takes `--repeat`. Each repeat starts from the same
//! pre-state; the first sample is reported as `first`, the median of the
//! rest as `steady median`.
//!
//! The numbers are for finding bottlenecks, not regression gates. Don't
//! check them in as expected values.
//!
//! Every run truncates the whole morpholog schema. Never point this at a
//! database you want to keep.

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use morpholog_core::{
    ClaimInstance, CompiledProgram, EvalValue, Outcome, Program, State, Subject, Transformation,
    TransformationName, Transition, enumerate_derived, predicates_referenced_by_derived, propose,
};
use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{
    IndexAction, InvariantPlan, PgAtomicOutcome, PgError, PgPool, PgProgram, PgProposalOutcome,
    Proposal, coverage_replay, list_claims_for_predicates, list_derived_at, propose_against_pg,
    propose_against_pg_timed, propose_all_against_pg, reconstruct_state_at, score_candidate,
};
use rust_decimal::Decimal;
use sqlx::postgres::PgPoolOptions;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(version, about = "Morpholog scale-pressure benchmark", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Populate state with N entries, then time one propose_against_pg
    /// call (post_simple_entry adding one more entry on top).
    Write(ScenarioArgs),

    /// Populate state with N entries, then time one list_derived call
    /// against the trial-balance derived claim.
    Read(ScenarioArgs),

    /// Fabricate N audit transitions, then time one
    /// `reconstruct_state_at` and one `list_derived_at` against a
    /// target transition. Measures audit-log replay cost as a
    /// function of N (number of transitions to walk through),
    /// `--at <fraction>` (how far through the log the target sits),
    /// and `--retract-fraction K` (what share of the log retracts
    /// prior claims rather than asserting fresh ones).
    AsOf(AsOfArgs),

    /// Run W workers issuing concurrent `propose_against_pg` calls
    /// against a deliberately-contended fixture (all posting into one
    /// shared period), each with the SERIALIZABLE 40001 retry loop a
    /// real embedder must carry. Measures throughput and the
    /// serialization-conflict retry rate under concurrency - the axis
    /// the single-propose scenarios cannot see.
    Contend(ContendArgs),

    /// From an empty book, commit N entries sequentially through the
    /// kernel - the cumulative 0->N CORE import curve `write` cannot
    /// see (`write` times one proposal AT size N; `import` times the
    /// whole journey). The in-process core of the workload that forced
    /// `propose --batch` (an embedder's seed/replay path); the real
    /// batch adds NDJSON parsing, argument decoding, and receipt
    /// serialisation around each of these commits.
    Import(ImportArgs),

    /// Time N ledger postings as ONE decision (`propose_all_against_pg`)
    /// against the same N as sequential single proposals, from the
    /// same prepopulated book; with `--writers W`, W concurrent single
    /// proposers post into the same period while the atomic batch
    /// runs, and the batch's own 40001 retries are counted - the
    /// conflict window grows with N, and this is where that shows.
    Transact(TransactArgs),

    /// Propose against, and read back, a synthetic WIDE predicate
    /// (default arity 13 - the widest consumer-reported claim shape).
    /// The gallery's widest predicate is 7-ary, so this is the carrier
    /// for how argument count moves write and read cost.
    Wide(WideArgs),

    /// The kernel alone, no database: build an in-memory ledger of N
    /// entries, then time one proposal with the ledger's invariants,
    /// the same proposal with none (body and candidate build; the
    /// difference is invariant evaluation), and a run of sequential
    /// acts each proposed against the candidate the act before it
    /// produced - the in-process core of `transact`. Not destructive;
    /// needs no `--reset`.
    Kernel(KernelArgs),

    /// Fabricate N audit transitions (the `as-of` fixture), then time
    /// the two replays that walk the whole log: `inspect coverage` and
    /// `evaluate` of the ledger programme against itself - the
    /// audit-analysis arc's cost.
    Replay(ReplayArgs),

    /// Run the frozen canonical case matrix across per-case ladders and
    /// print one table (markdown by default; `--format json` for
    /// machine comparison). This is the whole-suite evidence a
    /// performance PR carries; see docs/benchmarking.md for the
    /// discipline. Sequential and destructive; the full ladder takes
    /// tens of minutes on today's interpreted runtime.
    Suite(SuiteArgs),

    /// Compare baseline and candidate runs of `suite --format json`
    /// from the same ruler: one row per case metric with the median of
    /// each side's runs, their ratio, and whether the runs separate, as
    /// the table a performance PR carries.
    /// Refuses reports whose suite contracts differ.
    Compare(CompareArgs),
}

#[derive(clap::Args, Debug)]
struct KernelArgs {
    /// Number of journal entries in the in-memory book (three claims
    /// each, as the database fixtures lay them out).
    n: usize,

    /// Sequential acts in the batch run, each against the candidate the
    /// act before it produced.
    #[arg(long, default_value_t = 370)]
    acts: usize,

    /// Timed repetitions.
    #[arg(long, default_value_t = 5)]
    repeat: usize,

    /// The label the rows carry. The kernel family runs the in-memory
    /// interpreter whatever is selected; the selector exists so one
    /// suite run labels every row alike.
    #[arg(long, value_enum, default_value = "interpreted")]
    implementation: Implementation,
}

#[derive(clap::Args, Debug)]
struct ReplayArgs {
    /// Number of audit transitions to fabricate, as for `as-of`.
    n: usize,

    /// Percentage (0-50) of transitions that retract an earlier
    /// transition's claims, as for `as-of`.
    #[arg(long, default_value_t = 0)]
    retract_fraction: usize,

    /// Timed repetitions.
    #[arg(long, default_value_t = 3)]
    repeat: usize,

    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// Required: acknowledge that the target database is truncated.
    #[arg(long)]
    reset: bool,

    /// The execution configuration to measure: the interpreter, the
    /// compiled invariant route with no compiler-required index, or
    /// the compiled route with its indexes provisioned. The bench
    /// establishes the index condition itself, outside every sample.
    #[arg(long, value_enum, default_value = "interpreted")]
    implementation: Implementation,
}

#[derive(clap::Args, Debug)]
struct CompareArgs {
    /// The baseline runs: one or more `suite --format json` reports.
    /// A verdict needs four runs a side, interleaved with the
    /// candidate's; see docs/benchmarking.md.
    #[arg(long, num_args = 1.., required = true)]
    before: Vec<PathBuf>,
    /// The candidate runs, from the same ruler.
    #[arg(long, num_args = 1.., required = true)]
    after: Vec<PathBuf>,
}

#[derive(clap::Args, Debug)]
struct ScenarioArgs {
    /// Number of journal entries to pre-populate. The fixture inserts
    /// `3 * N` claims total (one JournalEntry plus two JournalLines
    /// per entry).
    n: usize,

    /// Number of distinct accounts to spread the journal lines across.
    /// Each entry `i` debits `account_{i mod K}` and credits
    /// `account_{(i + 1) mod K}` for the same amount; the fixture is
    /// always self-balancing per entry. Default is `2`, which
    /// preserves the original K=2 baseline (entries alternate
    /// `account_0` / `account_1`) so older numbers remain comparable.
    ///
    /// `K = 1` is allowed but degenerate: both debit and credit land
    /// on `account_0`, so every entry self-balances on a single
    /// account and trial balance produces exactly one row with
    /// balance zero. Useful as a "no grouping" baseline.
    ///
    /// The trial-balance derived claim produces one row per distinct
    /// account, so K is the upper bound on derived rows expected on
    /// the read scenario. Larger K stresses `enumerate_derived`'s
    /// grouping and the per-account `Sum` lookups.
    #[arg(long, default_value_t = 2)]
    accounts: usize,

    /// Number of "noise" claims of an `UnrelatedNoise` predicate to
    /// pre-populate alongside the ledger fixture. The predicate is
    /// never referenced by `post_simple_entry`'s body or by any
    /// invariant in the double-entry-ledger programme, so a correct
    /// scoped `load_state` must skip these rows entirely; on an
    /// older unscoped `load_state`, they show up linearly in
    /// fetch + decode time.
    ///
    /// Default is `0` (no noise). Set to a value comparable to or
    /// larger than `3 * N` to expose the predicate-scoping win on
    /// the write path; with `noise-claims 0` and `N` large, the
    /// fixture is the same shape as before this flag landed and
    /// the scoped vs. unscoped difference is invisible.
    #[arg(long, default_value_t = 0)]
    noise_claims: usize,

    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    /// The target database is truncated before each run.
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// Required: acknowledge that this binary will TRUNCATE the
    /// entire morpholog schema (claims, audit, outbox) in the target
    /// database before running. Exists to prevent accidental
    /// destruction via the `DATABASE_URL` env-var fallback when a
    /// shell already points at a non-benchmark database.
    #[arg(long)]
    reset: bool,

    /// The execution configuration to measure: the interpreter, the
    /// compiled invariant route with no compiler-required index, or
    /// the compiled route with its indexes provisioned. The bench
    /// establishes the index condition itself, outside every sample.
    #[arg(long, value_enum, default_value = "interpreted")]
    implementation: Implementation,

    /// Timed repetitions. Every repeat starts from the same logical
    /// pre-state (mutating scenarios rebuild their fixture); the first
    /// sample reports as `first`, the median over the rest as `steady
    /// median`. Default 1 preserves the single-shot behaviour.
    #[arg(long, default_value_t = 1)]
    repeat: usize,
}

/// Arguments for the `as-of` scenario. Distinct from `ScenarioArgs`
/// because the as-of fixture bypasses the write path entirely - it
/// fabricates audit rows directly via SQL, so the `accounts` axis
/// does not apply.
#[derive(clap::Args, Debug)]
struct AsOfArgs {
    /// Number of audit transitions to fabricate. Each transition
    /// asserts a 3-claim payload (one JournalEntry + two
    /// JournalLines), so the total claim count after replay-to-latest
    /// is `3 * N`. Memory usage scales with both N (number of audit
    /// rows fetched) and 3N (working state during replay).
    n: usize,

    /// Fraction of N at which to target the as-of query. `1.0`
    /// (default) targets the last fabricated transition - full
    /// replay. `0.5` targets the middle - roughly half replay. `0.0`
    /// targets the first - shortest replay. Useful for showing that
    /// "as-of T" scales with T's position in the log, not with the
    /// log's total size.
    #[arg(long, default_value_t = 1.0)]
    at: f64,

    /// Percentage (0-50) of the N transitions that retract an earlier
    /// transition's claims rather than asserting a fresh entry. `0`
    /// (default) is the original asserts-only log. The fixture
    /// interleaves retracts at a fixed stride so each retract targets
    /// a still-live prior entry; the actual retract-transition count
    /// is echoed at run time. The purely-additive default is
    /// best-case for replay, so this axis is what exposes any
    /// non-linearity in the replay's retract path. Capped at 50
    /// because above that a retract would have to target a transition
    /// that itself only retracts.
    #[arg(long, default_value_t = 0)]
    retract_fraction: usize,

    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    /// The target database is truncated before each run.
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// Required: acknowledge that this binary will TRUNCATE the
    /// entire morpholog schema before running. Same contract as the
    /// other scenarios.
    #[arg(long)]
    reset: bool,

    /// The execution configuration to measure: the interpreter, the
    /// compiled invariant route with no compiler-required index, or
    /// the compiled route with its indexes provisioned. The bench
    /// establishes the index condition itself, outside every sample.
    #[arg(long, value_enum, default_value = "interpreted")]
    implementation: Implementation,

    /// Timed repetitions over the immutable fabricated log; `first` +
    /// `steady median` reporting, same contract as the other scenarios.
    #[arg(long, default_value_t = 1)]
    repeat: usize,
}

/// Arguments for the `contend` scenario. Concurrency is the axis the
/// single-propose scenarios cannot exercise: how the SERIALIZABLE
/// substrate behaves when many transitions race for the same claims.
#[derive(clap::Args, Debug)]
struct ContendArgs {
    /// Number of concurrent workers, each on its own pooled
    /// connection. The pool is sized to `workers + 2`. Real
    /// contention pressure rises with this number.
    #[arg(long, default_value_t = 8)]
    workers: usize,

    /// Number of `propose_against_pg` operations each worker attempts.
    /// Total committed entries (absent retry exhaustion) is
    /// `workers * ops_per_worker`.
    #[arg(long, default_value_t = 50)]
    ops_per_worker: usize,

    /// Number of pre-existing journal entries to populate before the
    /// concurrent phase, so `load_state` has non-trivial work on each
    /// proposal. `0` (default) measures contention against an almost-
    /// empty table. Distributed across two accounts, same fixture
    /// shape as the `write`/`read` scenarios.
    #[arg(long, default_value_t = 0)]
    prepopulate: usize,

    /// Number of partitions to spread posts across; worker `w` uses
    /// partition `w mod periods`. `1` (default) puts every worker on the
    /// same partition. In the default ledger workload a partition is a
    /// period *value* (same predicate - value-level partitioning); with
    /// `--disjoint` it is a distinct *predicate*. The contrast between
    /// the two sweeps is the concurrency law; the measured answer is in
    /// the bench README.
    #[arg(long, default_value_t = 1)]
    periods: usize,

    /// Switch from the ledger workload to a synthetic one whose entire
    /// footprint is a single predicate `Bench_{w mod periods}`, so
    /// `--periods >= workers` gives every worker a disjoint predicate
    /// footprint. Tests the *positive* half of the concurrency law:
    /// predicate-disjoint workloads should not contend, where
    /// value-disjoint ones (the ledger `--periods` sweep) do.
    #[arg(long, default_value_t = false)]
    disjoint: bool,

    /// Per-operation cap on SERIALIZABLE (40001) retries before the
    /// operation is recorded as failed. A real caller retries; this
    /// bounds a pathological live-lock so the bench terminates.
    #[arg(long, default_value_t = 100)]
    max_retries: usize,

    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    /// The target database is truncated before each run.
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// Required: acknowledge that this binary will TRUNCATE the
    /// entire morpholog schema before running. Same contract as the
    /// other scenarios.
    #[arg(long)]
    reset: bool,

    /// The execution configuration to measure: the interpreter, the
    /// compiled invariant route with no compiler-required index, or
    /// the compiled route with its indexes provisioned. The bench
    /// establishes the index condition itself, outside every sample.
    #[arg(long, value_enum, default_value = "interpreted")]
    implementation: Implementation,

    /// Timed repetitions; each repeat rebuilds the prepopulated fixture
    /// so every burst races over the same logical pre-state.
    #[arg(long, default_value_t = 1)]
    repeat: usize,
}

/// Arguments for the `import` scenario.
#[derive(clap::Args, Debug)]
struct ImportArgs {
    /// Number of entries to commit sequentially from an empty book.
    /// The per-commit cost grows with the book on today's interpreted
    /// runtime, so the whole journey is roughly quadratic in N - keep
    /// N modest (the canonical suite tops out at 3000).
    n: usize,

    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    /// The target database is truncated before each run.
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// Required: acknowledge the TRUNCATE, same contract as the other
    /// scenarios.
    #[arg(long)]
    reset: bool,

    /// The execution configuration to measure: the interpreter, the
    /// compiled invariant route with no compiler-required index, or
    /// the compiled route with its indexes provisioned. The bench
    /// establishes the index condition itself, outside every sample.
    #[arg(long, value_enum, default_value = "interpreted")]
    implementation: Implementation,

    /// Timed repetitions; each repeat re-truncates so every journey
    /// starts from the same empty book.
    #[arg(long, default_value_t = 1)]
    repeat: usize,
}

/// Arguments for the `wide` scenario.
#[derive(clap::Args, Debug)]
struct WideArgs {
    /// Number of pre-existing wide rows before the measured proposal
    /// and read.
    n: usize,

    /// Argument count of the synthetic predicate (minimum 3: a line
    /// key, a group, an amount; the rest is subject padding). Default
    /// 13, the widest consumer-reported claim shape.
    #[arg(long, default_value_t = 13)]
    arity: usize,

    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    /// The target database is truncated before each run.
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// Required: acknowledge the TRUNCATE, same contract as the other
    /// scenarios.
    #[arg(long)]
    reset: bool,

    /// The execution configuration to measure: the interpreter, the
    /// compiled invariant route with no compiler-required index, or
    /// the compiled route with its indexes provisioned. The bench
    /// establishes the index condition itself, outside every sample.
    #[arg(long, value_enum, default_value = "interpreted")]
    implementation: Implementation,

    /// Timed repetitions; each repeat rebuilds the fixture so the
    /// proposal always lands on the same logical pre-state.
    #[arg(long, default_value_t = 1)]
    repeat: usize,
}

/// Arguments for the `suite` runner.
#[derive(clap::Args, Debug)]
struct SuiteArgs {
    /// Ladder size: `quick` is a fast local pass and the whole-suite
    /// complement table for performance PRs; `full` is the published
    /// curve set and takes tens of minutes on the interpreted runtime.
    #[arg(long, default_value = "quick")]
    ladder: Ladder,

    /// Timed repetitions per case point (import and contend cases cap
    /// themselves lower; see docs/benchmarking.md).
    #[arg(long, default_value_t = 5)]
    repeat: usize,

    /// Output format: `markdown` is the PR-body table; `json` is the
    /// machine seam for same-host baseline/candidate comparison.
    #[arg(long, default_value = "markdown")]
    format: OutputFormat,

    /// PostgreSQL connection string. Falls back to `DATABASE_URL`.
    /// The target database is truncated repeatedly across the run.
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// Required: acknowledge the TRUNCATEs, same contract as the other
    /// scenarios.
    #[arg(long)]
    reset: bool,

    /// The execution configuration to measure: the interpreter, the
    /// compiled invariant route with no compiler-required index, or
    /// the compiled route with its indexes provisioned. The bench
    /// establishes the index condition itself, outside every sample.
    #[arg(long, value_enum, default_value = "interpreted")]
    implementation: Implementation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Ladder {
    Quick,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum OutputFormat {
    Markdown,
    Json,
}

#[derive(clap::Args, Debug)]
struct TransactArgs {
    /// Acts in the one decision. The embedder that forced the surface
    /// runs about 370 a day.
    #[arg(long, default_value_t = 370)]
    acts: usize,

    /// Journal entries already in the book before the batch runs.
    #[arg(long, default_value_t = 1000)]
    prepopulate: usize,

    /// Concurrent single proposers posting into the same period for
    /// the batch's whole duration; 0 is the no-contention baseline.
    #[arg(long, default_value_t = 0)]
    writers: usize,

    /// The batch's own retry budget on a serialization failure.
    #[arg(long, default_value_t = 20)]
    max_retries: usize,

    /// Pause between a writer's proposals, in milliseconds: a real
    /// embedder does not spin, and a spinning writer starves a batch
    /// that must re-run whole on every conflict.
    #[arg(long, default_value_t = 5)]
    writer_pause_ms: u64,

    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    #[arg(long)]
    reset: bool,

    /// The execution configuration to measure: the interpreter, the
    /// compiled invariant route with no compiler-required index, or
    /// the compiled route with its indexes provisioned. The bench
    /// establishes the index condition itself, outside every sample.
    #[arg(long, value_enum, default_value = "interpreted")]
    implementation: Implementation,

    #[arg(long, default_value_t = 3)]
    repeat: usize,
}

/// Refuses to run without `--reset`. The error echoes the target URL so
/// the operator sees what would be truncated.
fn check_reset_ack(reset: bool, database_url: &str) -> Result<()> {
    if !reset {
        return Err(anyhow!(
            "this benchmark TRUNCATES the morpholog schema in the target database. \
             Re-run with `--reset` to acknowledge. Target: {}",
            morpholog_postgres::redact_database_url(database_url)
        ));
    }
    Ok(())
}

/// The fixture spreads lines over accounts by `i mod K`, so `K = 0`
/// would divide by zero in PostgreSQL.
fn require_positive_k(args: &ScenarioArgs) -> Result<()> {
    if args.accounts == 0 {
        return Err(anyhow!(
            "--accounts must be at least 1 (got 0); the fixture distributes \
             journal lines across K accounts via modular arithmetic, so K=0 \
             has no meaning"
        ));
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Write(args) => run_write(args).await,
        Command::Read(args) => run_read(args).await,
        Command::AsOf(args) => run_as_of(args).await,
        Command::Contend(args) => run_contend(args).await,
        Command::Import(args) => run_import(args).await,
        Command::Wide(args) => run_wide(args).await,
        Command::Transact(args) => run_transact(args).await,
        Command::Suite(args) => run_suite(args).await,
        Command::Kernel(args) => run_kernel(args),
        Command::Replay(args) => run_replay(args).await,
        Command::Compare(args) => run_compare(&args),
    }
}

// ============================================================
// Measurement layer
// ============================================================

/// The execution configuration a run measures, and the implementation
/// column of every result row. Choosing one changes what is measured,
/// not how, so the suite contract does not depend on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Implementation {
    Interpreted,
    Compiled,
    CompiledIndexed,
}

impl Implementation {
    fn label(self) -> &'static str {
        match self {
            Implementation::Interpreted => "interpreted",
            Implementation::Compiled => "compiled",
            Implementation::CompiledIndexed => "compiled-indexed",
        }
    }

    fn indexed(self) -> bool {
        matches!(self, Implementation::CompiledIndexed)
    }

    /// The programme a scenario proposes through. A compiled
    /// configuration refuses a programme that would fall back to the
    /// interpreter, so a row labelled compiled really is.
    fn program(self, core: Program) -> Result<PgProgram> {
        let compiled =
            CompiledProgram::new(core).map_err(|e| anyhow!("invalid programme: {e:?}"))?;
        match self {
            Implementation::Interpreted => Ok(PgProgram::interpreted(compiled)),
            Implementation::Compiled | Implementation::CompiledIndexed => {
                let program = PgProgram::new(compiled);
                match program.plan() {
                    InvariantPlan::Compiled => Ok(program),
                    InvariantPlan::Interpreted { refusals } => Err(anyhow!(
                        "`{}` requested but the programme `{}` would be interpreted: {}",
                        self.label(),
                        program.core().program().name,
                        refusals
                            .iter()
                            .map(|r| format!("{}: {}", r.invariant, r.reason))
                            .collect::<Vec<_>>()
                            .join("; ")
                    )),
                }
            }
        }
    }
}

/// Sets up the indexes the configuration needs, after each reset and
/// outside every timed sample. Morpholog's own `morpholog_ci_` indexes
/// are dropped first so nothing from an earlier case carries over.
/// Unindexed configurations refuse any other index that would satisfy a
/// requirement; the indexed one provisions every requirement, accepts an
/// equivalent, and refuses a conflict.
async fn establish(pool: &PgPool, implementation: Implementation, cores: &[Program]) -> Result<()> {
    let owned: Vec<String> = sqlx::query_scalar(
        "SELECT indexname::text FROM pg_indexes
         WHERE schemaname = 'morpholog' AND tablename = 'claims'
           AND indexname LIKE 'morpholog_ci_%'",
    )
    .fetch_all(pool)
    .await
    .context("listing Morpholog's compiled indexes")?;
    for name in owned {
        // Quoted like the catalogue quotes them.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP INDEX morpholog.\"{}\"",
            name.replace('"', "\"\"")
        )))
        .execute(pool)
        .await
        .with_context(|| format!("dropping {name} to start from a clean catalogue"))?;
    }
    for core in cores {
        let classified = PgProgram::new(
            CompiledProgram::new(core.clone()).map_err(|e| anyhow!("invalid programme: {e:?}"))?,
        );
        if implementation.indexed() {
            let report = morpholog_postgres::provision_indexes(pool, &classified, false)
                .await
                .context("provisioning the indexed condition")?;
            if !report.applied {
                return Err(anyhow!(
                    "the indexed condition could not be established for `{}`: {:?}",
                    report.program_identity,
                    report.entries
                ));
            }
        }
        let plan = morpholog_postgres::plan_indexes(pool, &classified)
            .await
            .context("checking the index condition")?;
        let acceptable = |action: &IndexAction| {
            if implementation.indexed() {
                matches!(action, IndexAction::Keep | IndexAction::SatisfiedExternally)
            } else {
                *action == IndexAction::Create
            }
        };
        let offending: Vec<String> = plan
            .entries
            .iter()
            .filter(|e| !acceptable(&e.action))
            .map(|e| format!("{} {} ({})", e.action, e.index_name, e.detail))
            .collect();
        if !offending.is_empty() {
            return Err(anyhow!(
                "the `{}` condition is not what the database holds for `{}`: {}",
                implementation.label(),
                plan.program_identity,
                offending.join("; ")
            ));
        }
    }
    Ok(())
}

/// The bench establishes index conditions through the registry, so the
/// database must be at the migration head; say so rather than fail
/// inside provisioning.
async fn require_migration_head(pool: &PgPool) -> Result<()> {
    let status = morpholog_postgres::migration_status(pool)
        .await
        .context("reading the migration status")?;
    if !status.is_current() {
        return Err(anyhow!(
            "the database schema is behind the Morpholog migration head; run `morpholog migrate`"
        ));
    }
    Ok(())
}

/// Bumped only when what the benchmark measures changes (cases,
/// fixtures, ladders, aggregation), never for a change in the code
/// being measured. See docs/benchmarking.md.
const SUITE_CONTRACT: u32 = 2;

/// One metric of one case: named samples with a unit, in repeat order.
/// `samples[0]` is the `first` reading, not a "cold" one: building the
/// fixture has just warmed the buffers. The steady median is over the
/// rest.
#[derive(Debug, Clone, serde::Serialize)]
struct Metric {
    name: &'static str,
    unit: &'static str,
    samples: Vec<f64>,
}

impl Metric {
    fn ms(name: &'static str, samples: &[Duration]) -> Self {
        Metric {
            name,
            unit: "ms",
            samples: samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect(),
        }
    }

    fn series(name: &'static str, unit: &'static str, samples: Vec<f64>) -> Self {
        Metric {
            name,
            unit,
            samples,
        }
    }

    fn first(&self) -> Option<f64> {
        self.samples.first().copied()
    }

    fn steady_median(&self) -> Option<f64> {
        steady_median(&self.samples)
    }
}

/// The median over every sample but the first, which is the `first`
/// reading; `None` when there is nothing after it.
fn steady_median(samples: &[f64]) -> Option<f64> {
    median(samples.get(1..).unwrap_or(&[]))
}

fn median(values: &[f64]) -> Option<f64> {
    let mut sorted = values.to_vec();
    if sorted.is_empty() {
        return None;
    }
    sorted.sort_by(f64::total_cmp);
    let mid = sorted.len() / 2;
    Some(if sorted.len() % 2 == 1 {
        sorted[mid]
    } else {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    })
}

/// One canonical case at one ladder point - what the suite table
/// renders, what `--format json` serialises, and what the single
/// scenario commands print human-readably.
#[derive(Debug, Clone, serde::Serialize)]
struct CaseResult {
    case: String,
    implementation: &'static str,
    axis: &'static str,
    point: u64,
    metrics: Vec<Metric>,
}

fn print_case_human(result: &CaseResult) {
    for m in &result.metrics {
        let first = m.first().unwrap_or(f64::NAN);
        match m.steady_median() {
            Some(steady) => println!(
                "  {:<18} first {:>10.2} {:<14} steady median {:>10.2} {} (over {})",
                m.name,
                first,
                m.unit,
                steady,
                m.unit,
                m.samples.len() - 1
            ),
            None => println!("  {:<18} {:>10.2} {}", m.name, first, m.unit),
        }
    }
}

/// Refresh planner statistics after a fixture lands, so the first
/// measured query is not planned against stale stats.
async fn analyze_claims(pool: &PgPool) -> Result<()> {
    sqlx::query("ANALYZE morpholog.claims")
        .execute(pool)
        .await
        .context("ANALYZE morpholog.claims")?;
    Ok(())
}

async fn analyze_audit(pool: &PgPool) -> Result<()> {
    sqlx::query("ANALYZE morpholog.audit")
        .execute(pool)
        .await
        .context("ANALYZE morpholog.audit")?;
    Ok(())
}

fn require_positive_repeat(repeat: usize) -> Result<()> {
    if repeat == 0 {
        return Err(anyhow!("--repeat must be at least 1"));
    }
    Ok(())
}

/// The write case: every repeat rebuilds the same logical pre-state
/// (N entries, K accounts, the noise rows), then times one fresh
/// proposal on top of it.
async fn measure_write(
    implementation: Implementation,
    pool: &PgPool,
    case: &str,
    n: usize,
    accounts: usize,
    noise_claims: usize,
    repeat: usize,
) -> Result<CaseResult> {
    // The programmes whose index condition this scenario establishes.
    let cores: Vec<Program> = vec![double_entry_ledger::program()];
    let compiled = implementation.program(double_entry_ledger::program())?;
    let mut fixture = Vec::with_capacity(repeat);
    let mut propose = Vec::with_capacity(repeat);
    let mut begin = Vec::with_capacity(repeat);
    let mut load = Vec::with_capacity(repeat);
    let mut kernel = Vec::with_capacity(repeat);
    let mut finalise = Vec::with_capacity(repeat);
    for r in 0..repeat {
        let t = Instant::now();
        reset_db(pool).await?;
        let reset_took = t.elapsed();
        // The index condition is established outside the sample.
        establish(pool, implementation, &cores).await?;
        let t = Instant::now();
        insert_n_entries(pool, n, accounts).await?;
        insert_noise_claims(pool, noise_claims).await?;
        fixture.push(reset_took + t.elapsed());
        analyze_claims(pool).await?;

        let transition = ledger_posting(&format!("entry_bench_target_{r}"), "p_bench");
        let t = Instant::now();
        let timed = propose_against_pg_timed(pool, &compiled, &Proposal::gateway(&transition))
            .await
            .context("propose_against_pg_timed")?;
        propose.push(t.elapsed());
        begin.push(timed.phases.begin);
        load.push(timed.phases.load);
        kernel.push(timed.phases.kernel);
        finalise.push(timed.phases.finalise);
        let outcome = timed.outcome;
        if !matches!(outcome, PgProposalOutcome::Committed { .. }) {
            return Err(anyhow!(
                "expected the target propose to commit ({}); bench fixture or \
                 kernel behaviour has changed",
                outcome_summary(&outcome)
            ));
        }
    }
    Ok(CaseResult {
        case: case.to_string(),
        implementation: implementation.label(),
        axis: "n",
        point: n as u64,
        metrics: vec![
            Metric::ms("fixture_build", &fixture),
            Metric::ms("propose_one", &propose),
            Metric::ms("phase_begin_tx", &begin),
            Metric::ms("phase_load_state", &load),
            Metric::ms("phase_kernel", &kernel),
            Metric::ms("phase_finalise", &finalise),
        ],
    })
}

/// The ledger book the database fixtures lay out, built in memory: one
/// JournalEntry and two JournalLines per entry, across two accounts.
fn in_memory_book(n: usize) -> Vec<ClaimInstance> {
    let mut claims = Vec::with_capacity(3 * n);
    for i in 0..n {
        let entry = subj(&format!("bench_entry_{i}"));
        claims.push(ClaimInstance {
            predicate: "JournalEntry".into(),
            args: vec![entry.clone(), subj("d_2026_05_17"), subj("p_bench")],
        });
        claims.push(ClaimInstance {
            predicate: "JournalLine".into(),
            args: vec![
                entry.clone(),
                subj(&format!("account_{}", i % 2)),
                dec(100),
                dec(0),
            ],
        });
        claims.push(ClaimInstance {
            predicate: "JournalLine".into(),
            args: vec![
                entry,
                subj(&format!("account_{}", (i + 1) % 2)),
                dec(0),
                dec(100),
            ],
        });
    }
    claims
}

/// Read once from the example, so every posting names the transformation the
/// example declares and no timed loop pays for looking it up.
static POST_SIMPLE_ENTRY: LazyLock<TransformationName> =
    LazyLock::new(|| double_entry_ledger::post_simple_entry().name);

fn ledger_posting(entry: &str, period: &str) -> Transition {
    Transition {
        transformation_name: POST_SIMPLE_ENTRY.clone(),
        args: vec![
            subj(entry),
            subj("d_2026_05_17"),
            subj(period),
            subj("account_cash"),
            subj("account_revenue"),
            dec(42),
        ],
        actor: Subject::from("bench"),
    }
}

fn must_commit(outcome: Outcome, what: &str) -> Result<State> {
    match outcome {
        Outcome::Accepted {
            candidate_state, ..
        } => Ok(candidate_state),
        Outcome::Rejected { reason } => Err(anyhow!("{what} was refused: {reason}")),
    }
}

/// The kernel alone over an in-memory book of `n` entries: the state
/// build, one proposal with the ledger's invariants, the same proposal
/// with none (the transformation body and the candidate build, so the
/// difference is what invariant evaluation costs), and `acts`
/// sequential proposals each against the candidate the act before it
/// produced.
fn measure_kernel(
    implementation: Implementation,
    case: &str,
    n: usize,
    acts: usize,
    repeat: usize,
) -> Result<CaseResult> {
    let transformation = double_entry_ledger::post_simple_entry();
    let invariants = double_entry_ledger::all_invariants();
    let definitions = double_entry_ledger::definitions();
    let claims = in_memory_book(n);
    let mut build = Vec::with_capacity(repeat);
    let mut one = Vec::with_capacity(repeat);
    let mut no_invariants = Vec::with_capacity(repeat);
    let mut sequential = Vec::with_capacity(repeat);
    for _ in 0..repeat {
        let input = claims.clone();
        let t = Instant::now();
        let pre = State::from_claims(input);
        build.push(t.elapsed());

        let target = ledger_posting("bench_target", "p_bench");
        let t = Instant::now();
        must_commit(
            propose(&transformation, &target, &pre, &invariants, &definitions)?,
            "the target proposal",
        )?;
        one.push(t.elapsed());

        let t = Instant::now();
        must_commit(
            propose(&transformation, &target, &pre, &[], &definitions)?,
            "the proposal without invariants",
        )?;
        no_invariants.push(t.elapsed());

        let batch: Vec<Transition> = (0..acts)
            .map(|i| ledger_posting(&format!("bench_act_{i}"), "p_bench"))
            .collect();
        let t = Instant::now();
        let mut state = pre;
        for act in &batch {
            state = must_commit(
                propose(&transformation, act, &state, &invariants, &definitions)?,
                "a sequential act",
            )?;
        }
        sequential.push(t.elapsed());
    }
    Ok(CaseResult {
        case: case.to_string(),
        implementation: implementation.label(),
        axis: "n",
        point: n as u64,
        metrics: vec![
            Metric::ms("state_build", &build),
            Metric::ms("propose_one", &one),
            Metric::ms("propose_no_invariants", &no_invariants),
            Metric::ms("acts_sequential", &sequential),
        ],
    })
}

fn run_kernel(args: KernelArgs) -> Result<()> {
    require_positive_repeat(args.repeat)?;
    println!(
        "scenario=kernel n={} acts={} repeat={}",
        args.n, args.acts, args.repeat
    );
    let result = measure_kernel(
        args.implementation,
        "kernel",
        args.n,
        args.acts,
        args.repeat,
    )?;
    print_case_human(&result);
    Ok(())
}

/// The two replays that walk the whole audit log, over the `as-of`
/// fixture: coverage and candidate scoring of the ledger programme.
async fn measure_replay(
    implementation: Implementation,
    pool: &PgPool,
    case: &str,
    n: usize,
    retract_fraction: usize,
    repeat: usize,
) -> Result<CaseResult> {
    // The programmes whose index condition this scenario establishes.
    let cores: Vec<Program> = vec![double_entry_ledger::program()];
    let program = double_entry_ledger::program();
    let retract_stride = retract_stride_for(retract_fraction);
    let t = Instant::now();
    reset_db(pool).await?;
    let reset_took = t.elapsed();
    // The index condition is established outside the sample.
    establish(pool, implementation, &cores).await?;
    let t = Instant::now();
    fabricate_audit_rows(pool, n, retract_stride).await?;
    let fixture = reset_took + t.elapsed();
    analyze_audit(pool).await?;

    let mut coverage = Vec::with_capacity(repeat);
    let mut evaluate = Vec::with_capacity(repeat);
    for _ in 0..repeat {
        let t = Instant::now();
        coverage_replay(pool, &program)
            .await
            .context("coverage_replay")?;
        coverage.push(t.elapsed());

        let t = Instant::now();
        score_candidate(pool, &program, None)
            .await
            .context("score_candidate")?;
        evaluate.push(t.elapsed());
    }
    Ok(CaseResult {
        case: case.to_string(),
        implementation: implementation.label(),
        axis: "n",
        point: n as u64,
        metrics: vec![
            Metric::ms("fixture_build", &[fixture]),
            Metric::ms("coverage_replay", &coverage),
            Metric::ms("evaluate_replay", &evaluate),
        ],
    })
}

async fn run_replay(args: ReplayArgs) -> Result<()> {
    check_reset_ack(args.reset, &args.database_url)?;
    require_positive_repeat(args.repeat)?;
    if args.retract_fraction > 50 {
        return Err(anyhow!("--retract-fraction must be between 0 and 50"));
    }
    let pool = connect(&args.database_url).await?;
    require_migration_head(&pool).await?;
    println!(
        "scenario=replay n={} retract_fraction={} repeat={}",
        args.n, args.retract_fraction, args.repeat
    );
    let result = measure_replay(
        args.implementation,
        &pool,
        "replay",
        args.n,
        args.retract_fraction,
        args.repeat,
    )
    .await?;
    print_case_human(&result);
    Ok(())
}

/// A suite report as read back from `--format json`: the same shape
/// `SuiteReport` writes, owned, so two runs can be set side by side.
#[derive(Debug, PartialEq, serde::Deserialize)]
struct ReadReport {
    suite_contract: u32,
    implementation: String,
    ladder: String,
    cases: Vec<ReadCase>,
}

#[derive(Debug, PartialEq, serde::Deserialize)]
struct ReadCase {
    case: String,
    axis: String,
    point: u64,
    metrics: Vec<ReadMetric>,
}

#[derive(Debug, PartialEq, serde::Deserialize)]
struct ReadMetric {
    name: String,
    unit: String,
    samples: Vec<f64>,
}

fn read_report(path: &std::path::Path) -> Result<ReadReport> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing {} as a suite report", path.display()))
}

type Key = (String, String, u64, String);

/// Pairs every metric of both sides by case, axis, point, and metric. A
/// pair becomes a ratio row; a metric only one side has is listed after
/// the table, so a changed plan cannot pass as a changed number.
/// Different contracts, ladders, or units are errors.
///
/// Each side is one or more suite runs, and a run is the unit of
/// evidence: repeats inside one run share a process and a cache, so
/// they agree far more closely than two runs do.
fn render_compare(before: &[ReadReport], after: &[ReadReport]) -> Result<String> {
    let (Some(first), Some(_)) = (before.first(), after.first()) else {
        return Err(anyhow!("each side needs at least one report"));
    };
    for report in before.iter().chain(after) {
        if report.suite_contract != first.suite_contract {
            return Err(anyhow!(
                "the reports were measured with different rulers: suite_contract {} and {}",
                first.suite_contract,
                report.suite_contract
            ));
        }
        if report.ladder != first.ladder {
            return Err(anyhow!(
                "the reports ran different ladders: {} and {}",
                first.ladder,
                report.ladder
            ));
        }
    }
    let all: Vec<&ReadReport> = before.iter().chain(after).collect();
    for (i, report) in all.iter().enumerate() {
        if all[i + 1..].contains(report) {
            return Err(anyhow!(
                "the same run is given twice: one run counted twice is not two runs"
            ));
        }
    }
    let (b, a) = (
        side_readings("before", before)?,
        side_readings("after", after)?,
    );
    let mut out = String::new();
    out.push_str(&format!(
        "suite_contract={} ladder={} before={} ({} runs) after={} ({} runs)\n\n",
        first.suite_contract,
        first.ladder,
        before[0].implementation,
        before.len(),
        after[0].implementation,
        after.len()
    ));
    out.push_str(
        "| case | axis | point | metric | before | after | after/before | verdict | unit |\n",
    );
    out.push_str("|---|---|--:|---|--:|--:|--:|---|---|\n");
    let mut unmatched = Vec::new();
    for (key, (unit, before_runs)) in &b {
        let Some((after_unit, after_runs)) = a.get(key) else {
            unmatched.push(format!(
                "before only: {} {} {} {} ({unit})",
                key.0, key.1, key.2, key.3
            ));
            continue;
        };
        if after_unit != unit {
            return Err(anyhow!(
                "{} {} {} {} is measured in {unit} before and {after_unit} after: a changed unit is a changed ruler",
                key.0,
                key.1,
                key.2,
                key.3
            ));
        }
        let (before_reading, after_reading) = (median(before_runs), median(after_runs));
        let ratio = match (before_reading, after_reading) {
            (Some(b), Some(a)) if b > 0.0 => format!("{:.2}", a / b),
            _ => "-".to_string(),
        };
        out.push_str(&format!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |\n",
            key.0,
            key.1,
            key.2,
            key.3,
            format_sample(before_reading),
            format_sample(after_reading),
            ratio,
            verdict(before_runs, after_runs),
            unit
        ));
    }
    for (key, (unit, _)) in &a {
        if !b.contains_key(key) {
            unmatched.push(format!(
                "after only: {} {} {} {} ({unit})",
                key.0, key.1, key.2, key.3
            ));
        }
    }
    if !unmatched.is_empty() {
        out.push('\n');
        for line in unmatched {
            out.push_str(&format!("- {line}\n"));
        }
    }
    Ok(out)
}

/// One reading per run for every metric of one side - the run's steady
/// median, or its only sample - with the metric's unit. The runs of a
/// side must measure one implementation and carry the same rows.
fn side_readings(
    side: &str,
    runs: &[ReadReport],
) -> Result<std::collections::BTreeMap<Key, (String, Vec<f64>)>> {
    let mut out: std::collections::BTreeMap<Key, (String, Vec<f64>)> =
        std::collections::BTreeMap::new();
    let mut rows_per_run = Vec::new();
    for report in runs {
        if report.implementation != runs[0].implementation {
            return Err(anyhow!(
                "the {side} runs measured different implementations: {} and {}",
                runs[0].implementation,
                report.implementation
            ));
        }
        let mut rows = std::collections::BTreeSet::new();
        for case in &report.cases {
            for m in &case.metrics {
                let key = (
                    case.case.clone(),
                    case.axis.clone(),
                    case.point,
                    m.name.clone(),
                );
                if !rows.insert(key.clone()) {
                    return Err(anyhow!(
                        "a report carries {} {} {} {} twice",
                        key.0,
                        key.1,
                        key.2,
                        key.3
                    ));
                }
                let entry = out
                    .entry(key.clone())
                    .or_insert_with(|| (m.unit.clone(), Vec::new()));
                if entry.0 != m.unit {
                    return Err(anyhow!(
                        "{} {} {} {} is measured in two units among the {side} runs",
                        key.0,
                        key.1,
                        key.2,
                        key.3
                    ));
                }
                if let Some(reading) =
                    steady_median(&m.samples).or_else(|| m.samples.first().copied())
                {
                    entry.1.push(reading);
                }
            }
        }
        rows_per_run.push(rows);
    }
    if rows_per_run.iter().any(|rows| rows.len() != out.len()) {
        return Err(anyhow!(
            "the {side} runs do not carry the same rows: a changed plan, not a changed number"
        ));
    }
    Ok(out)
}

/// Whether one side's runs lie wholly beyond the other's. Each side
/// needs at least four runs; fewer says too little about how much runs
/// vary. With four a side, if nothing changed and runs were interleaved,
/// complete separation happens by chance at most `2 / C(8, 4)` (under
/// 3%, the exact Mann-Whitney tail). The rule is per row, with no
/// correction for the number of rows. "lower"/"higher" rather than
/// better/worse, because some metrics are throughputs.
fn verdict(before: &[f64], after: &[f64]) -> &'static str {
    if before.len() < 4 || after.len() < 4 {
        return "too few runs";
    }
    let low = |xs: &[f64]| xs.iter().copied().fold(f64::INFINITY, f64::min);
    let high = |xs: &[f64]| xs.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    if high(after) < low(before) {
        "lower"
    } else if low(after) > high(before) {
        "higher"
    } else {
        "within noise"
    }
}

fn run_compare(args: &CompareArgs) -> Result<()> {
    let read = |paths: &[PathBuf]| {
        paths
            .iter()
            .map(|p| read_report(p))
            .collect::<Result<Vec<_>>>()
    };
    print!(
        "{}",
        render_compare(&read(&args.before)?, &read(&args.after)?)?
    );
    Ok(())
}

/// A scenario's pool, on the URL's implied user like every other
/// Postgres tool.
async fn connect(url: &str) -> Result<PgPool> {
    PgPool::connect(&morpholog_postgres::with_default_user(url))
        .await
        .context("connect to PostgreSQL")
}

async fn run_write(args: ScenarioArgs) -> Result<()> {
    check_reset_ack(args.reset, &args.database_url)?;
    require_positive_k(&args)?;
    require_positive_repeat(args.repeat)?;
    let pool = connect(&args.database_url).await?;
    require_migration_head(&pool).await?;
    println!(
        "scenario=write n={} accounts={} noise_claims={} repeat={}",
        args.n, args.accounts, args.noise_claims, args.repeat
    );
    let result = measure_write(
        args.implementation,
        &pool,
        "write",
        args.n,
        args.accounts,
        args.noise_claims,
        args.repeat,
    )
    .await?;
    print_case_human(&result);
    Ok(())
}

/// The read case: one fixture, reused across repeats because reading
/// never changes it. Times the three phases of `list_derived` separately.
async fn measure_read(
    implementation: Implementation,
    pool: &PgPool,
    case: &str,
    n: usize,
    accounts: usize,
    noise_claims: usize,
    repeat: usize,
) -> Result<CaseResult> {
    // The programmes whose index condition this scenario establishes.
    let cores: Vec<Program> = vec![double_entry_ledger::program()];
    let t = Instant::now();
    reset_db(pool).await?;
    let reset_took = t.elapsed();
    // The index condition is established outside the sample.
    establish(pool, implementation, &cores).await?;
    let t = Instant::now();
    insert_n_entries(pool, n, accounts).await?;
    insert_noise_claims(pool, noise_claims).await?;
    let fixture = reset_took + t.elapsed();
    analyze_claims(pool).await?;

    let derived = double_entry_ledger::trial_balance_row();
    let footprint: Vec<String> = predicates_referenced_by_derived(&derived, &[])
        .into_iter()
        .map(|p| p.to_string())
        .collect();

    let mut list_scoped = Vec::with_capacity(repeat);
    let mut build_state = Vec::with_capacity(repeat);
    let mut enumerate = Vec::with_capacity(repeat);
    let mut n_claims = 0usize;
    let mut n_rows = 0usize;
    for _ in 0..repeat {
        let t = Instant::now();
        let claims = list_claims_for_predicates(pool, &footprint)
            .await
            .context("list_claims_for_predicates")?;
        list_scoped.push(t.elapsed());
        n_claims = claims.len();

        let t = Instant::now();
        let state = State::from_claims(claims);
        build_state.push(t.elapsed());

        let t = Instant::now();
        let rows = enumerate_derived(&derived, &state, &[]).context("enumerate_derived")?;
        enumerate.push(t.elapsed());
        n_rows = rows.len();

        // No rows iff n=0; otherwise between 1 and K, one per account.
        if n == 0 {
            if !rows.is_empty() {
                return Err(anyhow!(
                    "expected 0 derived rows for n=0, got {}",
                    rows.len()
                ));
            }
        } else if rows.is_empty() {
            return Err(anyhow!(
                "expected at least one derived row for n={n} k={accounts}, got none"
            ));
        } else if rows.len() > accounts {
            return Err(anyhow!(
                "derived rows ({}) exceeded the K-account ceiling ({accounts}); \
                 fixture distribution is broken",
                rows.len()
            ));
        }
    }
    Ok(CaseResult {
        case: case.to_string(),
        implementation: implementation.label(),
        axis: "n",
        point: n as u64,
        metrics: vec![
            Metric::ms("fixture_build", &[fixture]),
            Metric::ms("list_scoped", &list_scoped),
            Metric::ms("build_state", &build_state),
            Metric::ms("enumerate", &enumerate),
            Metric::series("scoped_claims", "count", vec![n_claims as f64]),
            Metric::series("derived_rows", "count", vec![n_rows as f64]),
        ],
    })
}

async fn run_read(args: ScenarioArgs) -> Result<()> {
    check_reset_ack(args.reset, &args.database_url)?;
    require_positive_k(&args)?;
    require_positive_repeat(args.repeat)?;
    let pool = connect(&args.database_url).await?;
    require_migration_head(&pool).await?;
    println!(
        "scenario=read n={} accounts={} noise_claims={} repeat={}",
        args.n, args.accounts, args.noise_claims, args.repeat
    );
    let result = measure_read(
        args.implementation,
        &pool,
        "read",
        args.n,
        args.accounts,
        args.noise_claims,
        args.repeat,
    )
    .await?;
    print_case_human(&result);
    Ok(())
}

/// Interleave a retract every `stride` transitions; floored at 2 so a
/// retract always targets the still-live entry asserted immediately
/// before it. `0` disables retracts.
fn retract_stride_for(retract_fraction: usize) -> i64 {
    if retract_fraction == 0 {
        0
    } else {
        ((100.0 / retract_fraction as f64).round() as i64).max(2)
    }
}

/// The as-of case: one immutable fabricated log, reused across
/// repeats; one `reconstruct_state_at` plus one `list_derived_at` per
/// repeat against the `--at`-selected target.
async fn measure_as_of(
    implementation: Implementation,
    pool: &PgPool,
    case: &str,
    n: usize,
    at: f64,
    retract_fraction: usize,
    repeat: usize,
) -> Result<CaseResult> {
    // The programmes whose index condition this scenario establishes.
    let cores: Vec<Program> = vec![double_entry_ledger::program()];
    let retract_stride = retract_stride_for(retract_fraction);
    let retract_count = if retract_stride == 0 {
        0
    } else {
        n as i64 / retract_stride
    };

    let t = Instant::now();
    reset_db(pool).await?;
    let reset_took = t.elapsed();
    // The index condition is established outside the sample.
    establish(pool, implementation, &cores).await?;
    let t = Instant::now();
    fabricate_audit_rows(pool, n, retract_stride).await?;
    let fixture = reset_took + t.elapsed();
    analyze_audit(pool).await?;

    // Pick the target transition by causal offset; clamp so
    // floating-point edges do not push past the end.
    let offset: i64 = {
        let raw = ((n as f64) * at).floor() as i64;
        raw.clamp(0, (n as i64) - 1)
    };
    let (target_tid,): (Uuid,) = sqlx::query_as(
        "SELECT transition_id FROM morpholog.audit
         ORDER BY committed_at, transition_id LIMIT 1 OFFSET $1",
    )
    .bind(offset)
    .fetch_one(pool)
    .await
    .context("resolve target transition_id")?;

    let mut reconstruct = Vec::with_capacity(repeat);
    let mut list_at = Vec::with_capacity(repeat);
    let mut state_claims = 0usize;
    let mut derived_rows = 0usize;
    for _ in 0..repeat {
        let t = Instant::now();
        let state = reconstruct_state_at(pool, target_tid)
            .await
            .context("reconstruct_state_at")?;
        reconstruct.push(t.elapsed());
        state_claims = state.len();

        let t = Instant::now();
        let rows = list_derived_at(
            pool,
            &double_entry_ledger::trial_balance_row(),
            &double_entry_ledger::definitions(),
            target_tid,
        )
        .await
        .context("list_derived_at")?;
        list_at.push(t.elapsed());
        derived_rows = rows.len();
    }
    Ok(CaseResult {
        case: case.to_string(),
        implementation: implementation.label(),
        axis: "n",
        point: n as u64,
        metrics: vec![
            Metric::ms("fixture_build", &[fixture]),
            Metric::ms("reconstruct", &reconstruct),
            Metric::ms("list_derived_at", &list_at),
            Metric::series("state_claims", "count", vec![state_claims as f64]),
            Metric::series("derived_rows", "count", vec![derived_rows as f64]),
            Metric::series("retracts", "count", vec![retract_count as f64]),
        ],
    })
}

async fn run_as_of(args: AsOfArgs) -> Result<()> {
    check_reset_ack(args.reset, &args.database_url)?;
    require_positive_repeat(args.repeat)?;
    if args.n == 0 {
        return Err(anyhow!(
            "as-of bench requires n >= 1; there must be at least one fabricated \
             transition to target"
        ));
    }
    if !(0.0..=1.0).contains(&args.at) {
        return Err(anyhow!(
            "--at must be between 0.0 and 1.0 inclusive (got {})",
            args.at
        ));
    }
    if args.retract_fraction > 50 {
        return Err(anyhow!(
            "--retract-fraction must be between 0 and 50 (got {}); above 50% a \
             retract would have to target a transition that itself only retracts",
            args.retract_fraction
        ));
    }
    let pool = connect(&args.database_url).await?;
    require_migration_head(&pool).await?;
    println!(
        "scenario=as-of n={} at={} retract_fraction={} repeat={}",
        args.n, args.at, args.retract_fraction, args.repeat
    );
    let result = measure_as_of(
        args.implementation,
        &pool,
        "asof",
        args.n,
        args.at,
        args.retract_fraction,
        args.repeat,
    )
    .await?;
    print_case_human(&result);
    Ok(())
}

/// One concurrent burst: W workers, `ops` operations each, against a
/// freshly-built pre-state. Returns the summed tally and the elapsed
/// wall time of the concurrent phase.
#[allow(clippy::too_many_arguments)]
async fn contend_burst(
    implementation: Implementation,
    pool: &PgPool,
    workers: usize,
    ops_per_worker: usize,
    max_retries: usize,
    periods: usize,
    disjoint: bool,
    round: usize,
) -> Result<(Tally, Duration)> {
    let t = Instant::now();
    let mut handles = Vec::with_capacity(workers);
    for w in 0..workers {
        let pool = pool.clone();
        handles.push(tokio::spawn(async move {
            contend_worker(
                implementation,
                pool,
                w,
                ops_per_worker,
                max_retries,
                periods,
                disjoint,
                round,
            )
            .await
        }));
    }
    let mut total = Tally::default();
    for h in handles {
        // First `?`: the task panicked / was cancelled. Second `?`: the
        // worker hit an unexpected (non-40001) adapter error.
        let tally = h.await.context("join contend worker")??;
        total.committed += tally.committed;
        total.rejected += tally.rejected;
        total.retries += tally.retries;
        total.failed += tally.failed;
    }
    Ok((total, t.elapsed()))
}

/// The contend case: every repeat rebuilds the pre-state and races the
/// same burst over it. With `require_clean`, any failed or rejected
/// operation is an error: a change must not look faster because work
/// stopped succeeding.
#[allow(clippy::too_many_arguments)]
async fn measure_contend(
    implementation: Implementation,
    pool: &PgPool,
    case: &str,
    workers: usize,
    ops_per_worker: usize,
    prepopulate: usize,
    periods: usize,
    disjoint: bool,
    max_retries: usize,
    repeat: usize,
    require_clean: bool,
) -> Result<CaseResult> {
    // The programmes whose index condition this scenario establishes.
    let cores: Vec<Program> = if disjoint {
        (0..periods)
            .map(|p| synthetic_program(&format!("Bench_{p}")))
            .collect()
    } else {
        vec![double_entry_ledger::program()]
    };
    let total_ops = (workers * ops_per_worker) as u64;
    let mut fixture = Vec::with_capacity(repeat);
    let mut elapsed_s = Vec::with_capacity(repeat);
    let mut throughput_s = Vec::with_capacity(repeat);
    let mut retry_rate_s = Vec::with_capacity(repeat);
    let mut committed_s = Vec::with_capacity(repeat);
    let mut rejected_s = Vec::with_capacity(repeat);
    let mut failed_s = Vec::with_capacity(repeat);
    for round in 0..repeat {
        let t = Instant::now();
        reset_db(pool).await?;
        let reset_took = t.elapsed();
        // The index condition is established outside the sample.
        establish(pool, implementation, &cores).await?;
        let t = Instant::now();
        insert_n_entries(pool, prepopulate, 2).await?;
        fixture.push(reset_took + t.elapsed());
        analyze_claims(pool).await?;

        let (total, elapsed) = contend_burst(
            implementation,
            pool,
            workers,
            ops_per_worker,
            max_retries,
            periods,
            disjoint,
            round,
        )
        .await?;

        // Every op terminates as exactly one of committed / rejected /
        // failed; a drift here means a worker leaked an outcome.
        let accounted = total.committed + total.rejected + total.failed;
        if accounted != total_ops {
            return Err(anyhow!(
                "accounting mismatch: committed+rejected+failed ({accounted}) != total_ops ({total_ops})"
            ));
        }
        // A contention bench that commits nothing is degenerate: either
        // the scenario is broken (every op rejected) or it is
        // mis-parameterised (every op exhausted its retries).
        if total.committed == 0 {
            return Err(anyhow!(
                "contend committed nothing (rejected={} failed={} of {total_ops}); \
                 scenario is broken or mis-parameterised",
                total.rejected,
                total.failed
            ));
        }
        if require_clean && (total.failed > 0 || total.rejected > 0) {
            return Err(anyhow!(
                "canonical contend case is not clean (committed={} rejected={} \
                 failed={} of {total_ops}); a row with lost work is not a \
                 comparable measurement",
                total.committed,
                total.rejected,
                total.failed
            ));
        }

        let secs = elapsed.as_secs_f64();
        elapsed_s.push(elapsed);
        throughput_s.push(if secs > 0.0 {
            total.committed as f64 / secs
        } else {
            0.0
        });
        retry_rate_s.push(if total.committed > 0 {
            total.retries as f64 / total.committed as f64
        } else {
            0.0
        });
        committed_s.push(total.committed as f64);
        rejected_s.push(total.rejected as f64);
        failed_s.push(total.failed as f64);
    }
    Ok(CaseResult {
        case: case.to_string(),
        implementation: implementation.label(),
        axis: "workers",
        point: workers as u64,
        metrics: vec![
            Metric::ms("fixture_build", &fixture),
            Metric::ms("concurrent", &elapsed_s),
            Metric::series("throughput", "commits/s", throughput_s),
            Metric::series("retry_rate", "retries/commit", retry_rate_s),
            Metric::series("committed", "count", committed_s),
            Metric::series("rejected", "count", rejected_s),
            Metric::series("failed", "count", failed_s),
        ],
    })
}

async fn run_contend(args: ContendArgs) -> Result<()> {
    check_reset_ack(args.reset, &args.database_url)?;
    require_positive_repeat(args.repeat)?;
    if args.workers == 0 {
        return Err(anyhow!("--workers must be at least 1"));
    }
    if args.ops_per_worker == 0 {
        return Err(anyhow!("--ops-per-worker must be at least 1"));
    }
    if args.periods == 0 {
        return Err(anyhow!("--periods must be at least 1"));
    }

    // One connection per worker: a smaller pool would queue workers on
    // connections and hide the contention being measured.
    let pool = PgPoolOptions::new()
        .max_connections(args.workers as u32 + 2)
        .connect(&morpholog_postgres::with_default_user(&args.database_url))
        .await
        .context("connect to PostgreSQL")?;
    require_migration_head(&pool).await?;
    println!(
        "scenario=contend workers={} ops_per_worker={} prepopulate={} periods={} disjoint={} max_retries={} repeat={}",
        args.workers,
        args.ops_per_worker,
        args.prepopulate,
        args.periods,
        args.disjoint,
        args.max_retries,
        args.repeat
    );
    let result = measure_contend(
        args.implementation,
        &pool,
        "contend",
        args.workers,
        args.ops_per_worker,
        args.prepopulate,
        args.periods,
        args.disjoint,
        args.max_retries,
        args.repeat,
        false,
    )
    .await?;
    print_case_human(&result);
    Ok(())
}

/// Per-worker (and summed) outcome counts for the `contend` scenario.
/// `retries` counts 40001 occurrences, not operations - one operation
/// can contribute several retries before it commits.
#[derive(Default)]
struct Tally {
    committed: u64,
    rejected: u64,
    retries: u64,
    failed: u64,
}

/// One worker: `ops` sequential proposals, each on a fresh item. In the
/// ledger workload workers split by period but share predicates, so
/// more periods do not relieve contention. With `--disjoint` each
/// worker's footprint is its own predicate. Each proposal retries on
/// serialization failure (see [`one_op`]).
#[allow(clippy::too_many_arguments)]
async fn contend_worker(
    implementation: Implementation,
    pool: PgPool,
    worker_id: usize,
    ops: usize,
    max_retries: usize,
    periods: usize,
    disjoint: bool,
    round: usize,
) -> Result<Tally> {
    let mut tally = Tally::default();
    if disjoint {
        // Here `--periods` splits workers by predicate, not by value.
        let predicate = format!("Bench_{}", worker_id % periods);
        let transformation = synthetic_bump(&predicate);
        let compiled = implementation.program(synthetic_program(&predicate))?;
        for op in 0..ops {
            let transition = Transition {
                transformation_name: transformation.name.clone(),
                args: vec![subj(&format!("item_r{round}_w{worker_id}_op{op}"))],
                actor: Subject::from("bench"),
            };
            let label = format!("disjoint worker {worker_id} op {op}");
            one_op(
                &pool,
                &compiled,
                &transition,
                max_retries,
                &label,
                &mut tally,
            )
            .await?;
        }
    } else {
        let compiled = implementation.program(double_entry_ledger::program())?;
        let period = format!("p_contend_{}", worker_id % periods);
        for op in 0..ops {
            let transition =
                ledger_posting(&format!("entry_r{round}_w{worker_id}_op{op}"), &period);
            let label = format!("ledger worker {worker_id} op {op}");
            one_op(
                &pool,
                &compiled,
                &transition,
                max_retries,
                &label,
                &mut tally,
            )
            .await?;
        }
    }
    Ok(tally)
}

/// Propose one transition, retrying on serialization failure, and count
/// the outcome in `tally`. After `max_retries` it counts as `failed`.
/// Any other error is a bug, not contention, so it propagates.
async fn one_op(
    pool: &PgPool,
    compiled: &PgProgram,
    transition: &Transition,
    max_retries: usize,
    label: &str,
    tally: &mut Tally,
) -> Result<()> {
    let mut attempt: u64 = 0;
    loop {
        match propose_against_pg(pool, compiled, &Proposal::gateway(transition)).await {
            Ok(PgProposalOutcome::Committed { .. }) => {
                tally.committed += 1;
                return Ok(());
            }
            Ok(PgProposalOutcome::Rejected { .. }) => {
                tally.rejected += 1;
                return Ok(());
            }
            Err(PgError::SerializationFailure) => {
                tally.retries += 1;
                attempt += 1;
                if attempt as usize > max_retries {
                    tally.failed += 1;
                    return Ok(());
                }
                // Linear backoff (no jitter, dependency-free) to damp
                // live-lock; a production caller would jitter.
                tokio::time::sleep(Duration::from_micros(100 * attempt)).await;
            }
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!("contend {label}")));
            }
        }
    }
}

/// A transformation that reads and writes only one predicate, so
/// workers on distinct predicates do not contend and workers sharing one
/// do. Hand-built because the ledger's predicates cannot vary per worker.
fn synthetic_bump(predicate: &str) -> Transformation {
    use morpholog_core::ir_builder as b;
    b::transformation(
        &format!("bump_{predicate}"),
        vec!["item".into()],
        vec![
            b::require(b::not(b::claim(predicate, vec![b::var("item")]))),
            b::assert_(predicate, vec![b::var("item")]),
        ],
    )
}

/// The smallest valid programme around [`synthetic_bump`].
fn synthetic_program(predicate: &str) -> morpholog_core::Program {
    use morpholog_core::ir_builder as b;
    b::program(&format!("synthetic_{predicate}"))
        .predicates(vec![b::predicate(predicate).subject("item").build()])
        .transformations(vec![synthetic_bump(predicate)])
        .build()
}

// ============================================================
// import: the cumulative core-import curve
// ============================================================

/// The import case: N sequential commits from an empty book, the
/// in-process core of an embedder's bulk import (`propose --batch` adds
/// parsing and receipts around each commit). Per-commit cost grows with
/// the book, so the first and last tenth of commits show the growth.
/// Each repeat re-truncates; the whole journey is one sample.
async fn measure_import(
    implementation: Implementation,
    pool: &PgPool,
    case: &str,
    n: usize,
    repeat: usize,
) -> Result<CaseResult> {
    // The programmes whose index condition this scenario establishes.
    let cores: Vec<Program> = vec![double_entry_ledger::program()];
    if n == 0 {
        return Err(anyhow!("import requires n >= 1"));
    }
    let compiled = implementation.program(double_entry_ledger::program())?;
    let mut total_s = Vec::with_capacity(repeat);
    let mut rows_per_s = Vec::with_capacity(repeat);
    let mut first_decile = Vec::with_capacity(repeat);
    let mut last_decile = Vec::with_capacity(repeat);
    for r in 0..repeat {
        reset_db(pool).await?;
        establish(pool, implementation, &cores).await?;
        // Refresh stats so the first commits are not planned against
        // the previous case's leftovers.
        analyze_claims(pool).await?;
        analyze_audit(pool).await?;
        let mut per_commit = Vec::with_capacity(n);
        let journey = Instant::now();
        for i in 0..n {
            let transition = ledger_posting(&format!("entry_import_r{r}_{i}"), "p_import");
            let t = Instant::now();
            let outcome = propose_against_pg(pool, &compiled, &Proposal::gateway(&transition))
                .await
                .context("import propose")?;
            per_commit.push(t.elapsed());
            if !matches!(outcome, PgProposalOutcome::Committed { .. }) {
                return Err(anyhow!(
                    "import commit {i} did not commit ({}); fixture or kernel \
                     behaviour has changed",
                    outcome_summary(&outcome)
                ));
            }
        }
        let total = journey.elapsed();
        let decile = (n / 10).max(1);
        let mean_ms = |window: &[Duration]| {
            window.iter().map(|d| d.as_secs_f64() * 1000.0).sum::<f64>() / window.len() as f64
        };
        total_s.push(total);
        rows_per_s.push(n as f64 / total.as_secs_f64().max(f64::EPSILON));
        first_decile.push(mean_ms(&per_commit[..decile]));
        last_decile.push(mean_ms(&per_commit[n - decile..]));
    }
    Ok(CaseResult {
        case: case.to_string(),
        implementation: implementation.label(),
        axis: "n",
        point: n as u64,
        metrics: vec![
            Metric::ms("journey", &total_s),
            Metric::series("throughput", "rows/s", rows_per_s),
            Metric::series("first_decile_commit", "ms", first_decile),
            Metric::series("last_decile_commit", "ms", last_decile),
        ],
    })
}

async fn run_import(args: ImportArgs) -> Result<()> {
    check_reset_ack(args.reset, &args.database_url)?;
    require_positive_repeat(args.repeat)?;
    let pool = connect(&args.database_url).await?;
    require_migration_head(&pool).await?;
    println!("scenario=import n={} repeat={}", args.n, args.repeat);
    let result = measure_import(args.implementation, &pool, "import", args.n, args.repeat).await?;
    print_case_human(&result);
    Ok(())
}

// ============================================================
// wide: the argument-count axis
// ============================================================

/// The wide-predicate programme: a line key, a group, an amount, and
/// subject padding up to `arity`. Real users have 13-argument claims;
/// the worked examples stop at 7. A grouped-sum invariant gives the
/// write path realistic work over the wide rows.
fn wide_program(arity: usize) -> morpholog_core::Program {
    use morpholog_core::ir_builder as b;
    let mut decl = b::predicate("WideLine")
        .subject("line")
        .subject("grp")
        .decimal("amount");
    for i in 3..arity {
        decl = decl.subject(&format!("pad_{i}"));
    }
    // `unique by (line)` compares whole claims, so more arguments also
    // widen the invariant being checked, not just the stored row.
    decl = decl.disciplines(vec![morpholog_core::Discipline::UniqueBy {
        fields: vec!["line".to_string()],
    }]);

    let param_names: Vec<String> = wide_field_names(arity);
    let param_refs: Vec<&str> = param_names.iter().map(String::as_str).collect();
    let all_vars: Vec<morpholog_core::Term> = param_names.iter().map(|p| b::var(p)).collect();
    let mut require_pattern: Vec<morpholog_core::Term> = vec![b::var("line")];
    require_pattern.extend((1..arity).map(|_| b::wildcard()));

    // sum(amount | WideLine(_, grp, amount, _...)) per group.
    let mut sum_pattern: Vec<morpholog_core::Term> =
        vec![b::wildcard(), b::var("grp"), b::var("amount")];
    sum_pattern.extend((3..arity).map(|_| b::wildcard()));
    let mut head_pattern: Vec<morpholog_core::Term> = vec![b::wildcard(), b::var("grp")];
    head_pattern.extend((2..arity).map(|_| b::wildcard()));

    let mut program = b::program("wide_bench")
        .predicates(vec![decl.build()])
        .invariants(vec![b::invariant(
            "grouped_total_capped",
            b::implies(
                b::claim("WideLine", head_pattern),
                b::le(
                    b::sum(b::var("amount"), b::claim("WideLine", sum_pattern)),
                    b::term(b::dec("1000000000000")),
                ),
            ),
        )])
        .transformations(vec![b::transformation(
            "add_wide",
            b::params(&param_refs),
            vec![
                b::require(b::not(b::claim("WideLine", require_pattern))),
                b::assert_("WideLine", all_vars),
            ],
        )])
        .build();
    // The parser does this for `.morph` sources; hand-built IR must do
    // it itself or validation refuses the programme.
    morpholog_core::lower_disciplines(&mut program);
    program
}

fn wide_field_names(arity: usize) -> Vec<String> {
    let mut names = vec!["line".to_string(), "grp".to_string(), "amount".to_string()];
    names.extend((3..arity).map(|i| format!("pad_{i}")));
    names
}

/// Insert `n` wide rows directly. The SQL is built from the arity, an
/// internal integer, hence `AssertSqlSafe`.
async fn insert_wide_rows(pool: &PgPool, n: usize, arity: usize) -> Result<()> {
    if n == 0 {
        return Ok(());
    }
    let n_i: i64 = n
        .try_into()
        .map_err(|_| anyhow!("n={n} too large for i64"))?;
    let mut elements = vec![
        "jsonb_build_object('type','subject','value','wide_' || i)".to_string(),
        "jsonb_build_object('type','subject','value','g_' || (i % 16))".to_string(),
        "jsonb_build_object('type','decimal','value','1')".to_string(),
    ];
    elements.extend(
        (3..arity).map(|p| format!("jsonb_build_object('type','subject','value','pad_{p}')")),
    );
    let sql = format!(
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         SELECT 'WideLine', jsonb_build_array({}), $1
         FROM generate_series(1, $2) AS i",
        elements.join(", ")
    );
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(Uuid::nil())
        .bind(n_i)
        .execute(pool)
        .await
        .context("insert wide fixture rows")?;
    Ok(())
}

/// The wide case: every repeat rebuilds N wide rows, times reading them
/// back, then one proposal through the grouped-sum invariant.
async fn measure_wide(
    implementation: Implementation,
    pool: &PgPool,
    case: &str,
    axis: &'static str,
    n: usize,
    arity: usize,
    repeat: usize,
) -> Result<CaseResult> {
    if arity < 3 {
        return Err(anyhow!(
            "--arity must be at least 3 (a line key, a group, an amount); got {arity}"
        ));
    }
    let program = wide_program(arity);
    let compiled = implementation.program(program.clone())?;
    // The programmes whose index condition this scenario establishes.
    let cores: Vec<Program> = vec![program];
    let footprint = vec!["WideLine".to_string()];

    let mut fixture = Vec::with_capacity(repeat);
    let mut list_scoped = Vec::with_capacity(repeat);
    let mut build_state = Vec::with_capacity(repeat);
    let mut propose = Vec::with_capacity(repeat);
    for r in 0..repeat {
        let t = Instant::now();
        reset_db(pool).await?;
        let reset_took = t.elapsed();
        // The index condition is established outside the sample.
        establish(pool, implementation, &cores).await?;
        let t = Instant::now();
        insert_wide_rows(pool, n, arity).await?;
        fixture.push(reset_took + t.elapsed());
        analyze_claims(pool).await?;

        let t = Instant::now();
        let claims = list_claims_for_predicates(pool, &footprint)
            .await
            .context("list wide claims")?;
        list_scoped.push(t.elapsed());
        if claims.len() != n {
            return Err(anyhow!(
                "expected {n} wide claims, found {}; fixture is broken",
                claims.len()
            ));
        }
        let t = Instant::now();
        let _state = State::from_claims(claims);
        build_state.push(t.elapsed());

        let mut args: Vec<EvalValue> = vec![subj(&format!("wide_target_{r}")), subj("g_0"), dec(1)];
        args.extend((3..arity).map(|p| subj(&format!("pad_{p}"))));
        let transition = Transition {
            transformation_name: "add_wide".into(),
            args,
            actor: Subject::from("bench"),
        };
        let t = Instant::now();
        let outcome = propose_against_pg(pool, &compiled, &Proposal::gateway(&transition))
            .await
            .context("wide propose")?;
        propose.push(t.elapsed());
        if !matches!(outcome, PgProposalOutcome::Committed { .. }) {
            return Err(anyhow!(
                "wide propose did not commit ({}); fixture or kernel behaviour \
                 has changed",
                outcome_summary(&outcome)
            ));
        }
    }
    Ok(CaseResult {
        case: case.to_string(),
        implementation: implementation.label(),
        axis,
        point: if axis == "arity" {
            arity as u64
        } else {
            n as u64
        },
        metrics: vec![
            Metric::ms("fixture_build", &fixture),
            Metric::ms("list_scoped", &list_scoped),
            Metric::ms("build_state", &build_state),
            Metric::ms("propose_one", &propose),
        ],
    })
}

async fn run_wide(args: WideArgs) -> Result<()> {
    check_reset_ack(args.reset, &args.database_url)?;
    require_positive_repeat(args.repeat)?;
    let pool = connect(&args.database_url).await?;
    require_migration_head(&pool).await?;
    println!(
        "scenario=wide n={} arity={} repeat={}",
        args.n, args.arity, args.repeat
    );
    let result = measure_wide(
        args.implementation,
        &pool,
        "wide",
        "n",
        args.n,
        args.arity,
        args.repeat,
    )
    .await?;
    print_case_human(&result);
    Ok(())
}

/// Fabricate `n` audit rows in one SQL statement, each shaped like a
/// `post_simple_entry` commit (one JournalEntry, two JournalLines).
/// Going through `propose_against_pg` would make building the fixture
/// cost more than the replay being measured.
///
/// `transition_id` is a random UUIDv4: replay orders by
/// `(committed_at, transition_id)`, and `committed_at` rises by one
/// microsecond per row, so the order is deterministic.
///
/// With `retract_stride > 0`, every `stride`-th row retracts the entry
/// asserted by the row before it. The stride is at least 2, so that
/// entry is always still live. A stride of 0 means no retracts.
async fn fabricate_audit_rows(pool: &PgPool, n: usize, retract_stride: i64) -> Result<()> {
    let n_i: i64 = n
        .try_into()
        .map_err(|_| anyhow!("n={n} too large for i64"))?;

    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents,
            committed_at, attestation, parameters
        )
        SELECT
            gen_random_uuid(),
            'bench_as_of_post',
            '[]'::jsonb,
            '{\"type\":\"subject\",\"value\":\"bench\"}'::jsonb,
            1,
            '[]'::jsonb,
            CASE WHEN is_retract THEN '[]'::jsonb ELSE payload END,
            CASE WHEN is_retract THEN payload ELSE '[]'::jsonb END,
            '[]'::jsonb,
            now() + (i * interval '1 microsecond'),
            '{\"mode\":\"gateway\",\"authenticated_by\":\"bench-fixture\"}'::jsonb,
            '[]'::jsonb
        FROM (
            SELECT
                i,
                is_retract,
                jsonb_build_array(
                    jsonb_build_object(
                        'predicate', 'JournalEntry',
                        'args', jsonb_build_array(
                            jsonb_build_object('type','subject','value','bench_entry_' || target),
                            jsonb_build_object('type','subject','value','d_2026'),
                            jsonb_build_object('type','subject','value','p_bench')
                        )
                    ),
                    jsonb_build_object(
                        'predicate', 'JournalLine',
                        'args', jsonb_build_array(
                            jsonb_build_object('type','subject','value','bench_entry_' || target),
                            jsonb_build_object('type','subject','value','account_cash'),
                            jsonb_build_object('type','decimal','value','100'),
                            jsonb_build_object('type','decimal','value','0')
                        )
                    ),
                    jsonb_build_object(
                        'predicate', 'JournalLine',
                        'args', jsonb_build_array(
                            jsonb_build_object('type','subject','value','bench_entry_' || target),
                            jsonb_build_object('type','subject','value','account_revenue'),
                            jsonb_build_object('type','decimal','value','0'),
                            jsonb_build_object('type','decimal','value','100')
                        )
                    )
                ) AS payload
            FROM (
                SELECT
                    i,
                    ($2 > 0 AND i % $2 = 0) AS is_retract,
                    CASE WHEN ($2 > 0 AND i % $2 = 0) THEN i - 1 ELSE i END AS target
                FROM generate_series(1, $1) AS i
            ) base
        ) rows",
    )
    .bind(n_i)
    .bind(retract_stride)
    .execute(pool)
    .await
    .context("fabricate audit rows")?;

    Ok(())
}

async fn reset_db(pool: &PgPool) -> Result<()> {
    sqlx::query(morpholog_postgres::testing::RESET_SQL)
        .execute(pool)
        .await
        .context("TRUNCATE morpholog tables")?;
    Ok(())
}

/// Insert `n` journal entries (one JournalEntry, two JournalLines each)
/// in three SQL statements, whatever `n` is.
///
/// Entry `i` debits `account_{i mod k}` and credits
/// `account_{(i + 1) mod k}` by the same amount, so every entry
/// balances. The period `p_bench` is never closed, so a later `write`
/// proposal passes its gate.
///
/// `asserted_in` is `Uuid::nil()`, which has no audit row; the schema
/// does not require one.
async fn insert_n_entries(pool: &PgPool, n: usize, k: usize) -> Result<()> {
    let n_i: i64 = n
        .try_into()
        .map_err(|_| anyhow!("n={n} too large for i64"))?;
    let k_i: i64 = k
        .try_into()
        .map_err(|_| anyhow!("accounts={k} too large for i64"))?;
    let fixture_id = Uuid::nil();

    // JournalEntry(entry_id, posting_date, period). One per entry,
    // independent of K.
    sqlx::query(
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         SELECT 'JournalEntry',
                jsonb_build_array(
                    jsonb_build_object('type','subject','value','entry_bench_' || i),
                    jsonb_build_object('type','subject','value','d_2026_05_17'),
                    jsonb_build_object('type','subject','value','p_bench')
                ),
                $1
         FROM generate_series(1, $2) AS i",
    )
    .bind(fixture_id)
    .bind(n_i)
    .execute(pool)
    .await
    .context("insert JournalEntry fixture rows")?;

    // JournalLine(entry_id, account_{i % k}, 100, 0)  - debit side
    sqlx::query(
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         SELECT 'JournalLine',
                jsonb_build_array(
                    jsonb_build_object('type','subject','value','entry_bench_' || i),
                    jsonb_build_object('type','subject','value','account_' || (i % $3)),
                    jsonb_build_object('type','decimal','value','100'),
                    jsonb_build_object('type','decimal','value','0')
                ),
                $1
         FROM generate_series(1, $2) AS i",
    )
    .bind(fixture_id)
    .bind(n_i)
    .bind(k_i)
    .execute(pool)
    .await
    .context("insert JournalLine debit-side fixture rows")?;

    // JournalLine(entry_id, account_{(i+1) % k}, 0, 100)  - credit side
    sqlx::query(
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         SELECT 'JournalLine',
                jsonb_build_array(
                    jsonb_build_object('type','subject','value','entry_bench_' || i),
                    jsonb_build_object('type','subject','value','account_' || ((i + 1) % $3)),
                    jsonb_build_object('type','decimal','value','0'),
                    jsonb_build_object('type','decimal','value','100')
                ),
                $1
         FROM generate_series(1, $2) AS i",
    )
    .bind(fixture_id)
    .bind(n_i)
    .bind(k_i)
    .execute(pool)
    .await
    .context("insert JournalLine credit-side fixture rows")?;

    Ok(())
}

/// Insert `count` rows of `UnrelatedNoise(noise_i, i)`. The ledger
/// programme never reads this predicate, so loading state for a
/// proposal should skip these rows entirely.
async fn insert_noise_claims(pool: &PgPool, count: usize) -> Result<()> {
    if count == 0 {
        return Ok(());
    }
    let count_i: i64 = count
        .try_into()
        .map_err(|_| anyhow!("noise-claims={count} too large for i64"))?;
    let fixture_id = Uuid::nil();
    sqlx::query(
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         SELECT 'UnrelatedNoise',
                jsonb_build_array(
                    jsonb_build_object('type','subject','value','noise_' || i),
                    jsonb_build_object('type','decimal','value', i::text)
                ),
                $1
         FROM generate_series(1, $2) AS i",
    )
    .bind(fixture_id)
    .bind(count_i)
    .execute(pool)
    .await
    .context("insert noise fixture rows")?;
    Ok(())
}

fn subj(s: &str) -> EvalValue {
    EvalValue::Subject(s.into())
}

fn dec(n: i64) -> EvalValue {
    EvalValue::Decimal(Decimal::new(n, 0))
}

fn outcome_summary(outcome: &PgProposalOutcome) -> String {
    match outcome {
        PgProposalOutcome::Committed {
            asserted_claims,
            retracted_claims,
            emitted_intents,
            ..
        } => format!(
            "Committed (asserts={} retracts={} intents={})",
            asserted_claims.len(),
            retracted_claims.len(),
            emitted_intents.len()
        ),
        PgProposalOutcome::Rejected { reason, .. } => format!("Rejected: {reason}"),
    }
}

// ============================================================
// suite: the frozen canonical case matrix
// ============================================================

/// One canonical case family's provenance - the consumer workload or
/// mechanism that forced it. Rendered as a mapping block above the
/// table so the table's anchor travels with it.
const CASE_PROVENANCE: &[(&str, &str)] = &[
    (
        "write",
        "single-proposal latency as governed state grows; /noise is the \
         predicate-scoping control (forced predicate-scoped load_state)",
    ),
    (
        "read",
        "the read path's three phases; /grouped stresses derived grouping \
         across 100 accounts",
    ),
    (
        "asof",
        "audit-log replay; /retract is the replay's retract-path control \
         (the asserts-only default is best-case)",
    ),
    (
        "contend",
        "the SSI concurrency law, as same-workload A/Bs: /shared vs \
         /value-partitioned (ledger) = value sharding does not relieve 40001 \
         pressure; /predicate-shared vs /disjoint (synthetic) = footprint \
         partitioning does. The two scaling curves use different workloads \
         and never compare to each other; workers=1 is the non-contention \
         baseline",
    ),
    (
        "import",
        "the in-process CORE of the embedder import/replay path that forced \
         propose --batch (Redline's 130-act WAN seed; grid-mysteries' CI \
         replay); the real batch adds NDJSON/decode/receipt cost per row",
    ),
    (
        "wide",
        "the billing embedder's 13-ary InvoiceLine shape (the gallery tops \
         out at 7-ary); /size sweeps rows at arity 13, /arity sweeps the \
         argument count itself",
    ),
    (
        "kernel",
        "the kernel alone, no database: the interpreted core of transact \
         (an embedder's ~370-act day proposed act by act against the \
         candidate before it) and the same proposal without invariants, \
         so the difference is what invariant evaluation costs",
    ),
    (
        "replay",
        "the audit-analysis arc: coverage and candidate scoring walk the \
         whole audit log, once per row, over the as-of fixture",
    ),
];

#[derive(Debug, Clone)]
enum CaseKind {
    Write {
        n: usize,
        accounts: usize,
        noise: usize,
    },
    Read {
        n: usize,
        accounts: usize,
        noise: usize,
    },
    AsOf {
        n: usize,
        retract_fraction: usize,
    },
    Contend {
        workers: usize,
        ops: usize,
        prepopulate: usize,
        periods: usize,
        disjoint: bool,
    },
    Import {
        n: usize,
    },
    Wide {
        axis: &'static str,
        n: usize,
        arity: usize,
    },
    Kernel {
        n: usize,
        acts: usize,
    },
    Replay {
        n: usize,
        retract_fraction: usize,
    },
}

#[derive(Debug, Clone)]
struct CaseSpec {
    case: &'static str,
    kind: CaseKind,
}

/// The frozen matrix. Complements are deliberate anti-overfit
/// controls, not defaults: changing anything here (cases, sizes,
/// parameters) is changing the ruler and bumps [`SUITE_CONTRACT`] in
/// its own reviewed commit - never inside a performance PR.
fn suite_plan(ladder: Ladder) -> Vec<CaseSpec> {
    let n_ladder: &[usize] = match ladder {
        Ladder::Quick => &[100, 1_000],
        Ladder::Full => &[1_000, 10_000, 100_000],
    };
    // Import's per-commit cost grows with the book, so the journey is
    // roughly quadratic in N on the interpreted runtime - its ladder
    // is deliberately small (0->100k would be ~22h per repeat).
    let import_ladder: &[usize] = match ladder {
        Ladder::Quick => &[100, 500],
        Ladder::Full => &[100, 1_000, 3_000],
    };
    let workers_ladder: &[usize] = match ladder {
        Ladder::Quick => &[1, 4],
        Ladder::Full => &[1, 2, 4, 8, 16],
    };
    let contend_ops = match ladder {
        Ladder::Quick => 10,
        Ladder::Full => 25,
    };
    let contend_prepopulate = match ladder {
        Ladder::Quick => 100,
        Ladder::Full => 2_000,
    };
    let wide_arities: &[usize] = &[4, 7, 13];
    let wide_fixed_n = match ladder {
        Ladder::Quick => 1_000,
        Ladder::Full => 10_000,
    };

    let mut plan = Vec::new();
    for &n in n_ladder {
        plan.push(CaseSpec {
            case: "write/base",
            kind: CaseKind::Write {
                n,
                accounts: 2,
                noise: 0,
            },
        });
        plan.push(CaseSpec {
            case: "write/noise",
            kind: CaseKind::Write {
                n,
                accounts: 2,
                noise: 3 * n,
            },
        });
        plan.push(CaseSpec {
            case: "read/base",
            kind: CaseKind::Read {
                n,
                accounts: 2,
                noise: 0,
            },
        });
        plan.push(CaseSpec {
            case: "read/grouped",
            kind: CaseKind::Read {
                n,
                accounts: 100,
                noise: 0,
            },
        });
        plan.push(CaseSpec {
            case: "asof/assert",
            kind: CaseKind::AsOf {
                n,
                retract_fraction: 0,
            },
        });
        plan.push(CaseSpec {
            case: "asof/retract",
            kind: CaseKind::AsOf {
                n,
                retract_fraction: 50,
            },
        });
        plan.push(CaseSpec {
            case: "wide/size",
            kind: CaseKind::Wide {
                axis: "n",
                n,
                arity: 13,
            },
        });
        plan.push(CaseSpec {
            case: "kernel/acts",
            kind: CaseKind::Kernel { n, acts: 370 },
        });
        plan.push(CaseSpec {
            case: "replay/retract",
            kind: CaseKind::Replay {
                n,
                retract_fraction: 50,
            },
        });
    }
    for &workers in workers_ladder {
        plan.push(CaseSpec {
            case: "contend/shared",
            kind: CaseKind::Contend {
                workers,
                ops: contend_ops,
                prepopulate: contend_prepopulate,
                periods: 1,
                disjoint: false,
            },
        });
        plan.push(CaseSpec {
            case: "contend/disjoint",
            kind: CaseKind::Contend {
                workers,
                ops: contend_ops,
                prepopulate: 0,
                periods: workers,
                disjoint: true,
            },
        });
    }
    // Each curve above gets one control row on the same workload, at
    // the top worker count only. The two curves use different
    // workloads and are not comparable to each other. The ladder is
    // never empty; the fallback only avoids a panic path.
    let max_workers = workers_ladder.last().copied().unwrap_or(1);
    plan.push(CaseSpec {
        // Same workload as /shared, one period per worker. Expected
        // NOT to improve on /shared.
        case: "contend/value-partitioned",
        kind: CaseKind::Contend {
            workers: max_workers,
            ops: contend_ops,
            prepopulate: contend_prepopulate,
            periods: max_workers,
            disjoint: false,
        },
    });
    plan.push(CaseSpec {
        // Same workload as /disjoint, all workers on one predicate.
        // /disjoint is expected to improve on this row.
        case: "contend/predicate-shared",
        kind: CaseKind::Contend {
            workers: max_workers,
            ops: contend_ops,
            prepopulate: 0,
            periods: 1,
            disjoint: true,
        },
    });
    for &n in import_ladder {
        plan.push(CaseSpec {
            case: "import/core",
            kind: CaseKind::Import { n },
        });
    }
    for &arity in wide_arities {
        plan.push(CaseSpec {
            case: "wide/arity",
            kind: CaseKind::Wide {
                axis: "arity",
                n: wide_fixed_n,
                arity,
            },
        });
    }
    plan
}

/// Run one canonical case. Import and contend cap their repeats (the
/// journeys are long and each rebuilds its pre-state); the canonical
/// contend rows require a clean burst.
async fn run_case(
    implementation: Implementation,
    pool: &PgPool,
    spec: &CaseSpec,
    repeat: usize,
) -> Result<CaseResult> {
    match &spec.kind {
        CaseKind::Write { n, accounts, noise } => {
            measure_write(
                implementation,
                pool,
                spec.case,
                *n,
                *accounts,
                *noise,
                repeat,
            )
            .await
        }
        CaseKind::Read { n, accounts, noise } => {
            measure_read(
                implementation,
                pool,
                spec.case,
                *n,
                *accounts,
                *noise,
                repeat,
            )
            .await
        }
        CaseKind::AsOf {
            n,
            retract_fraction,
        } => {
            measure_as_of(
                implementation,
                pool,
                spec.case,
                *n,
                1.0,
                *retract_fraction,
                repeat,
            )
            .await
        }
        CaseKind::Contend {
            workers,
            ops,
            prepopulate,
            periods,
            disjoint,
        } => {
            // A higher retry cap than the standalone default: these rows
            // must be clean, and running out of retries at an arbitrary
            // cap would be an artifact, not a measurement.
            measure_contend(
                implementation,
                pool,
                spec.case,
                *workers,
                *ops,
                *prepopulate,
                *periods,
                *disjoint,
                1000,
                repeat.min(3),
                true,
            )
            .await
        }
        CaseKind::Import { n } => {
            measure_import(implementation, pool, spec.case, *n, repeat.min(3)).await
        }
        CaseKind::Wide { axis, n, arity } => {
            measure_wide(implementation, pool, spec.case, axis, *n, *arity, repeat).await
        }
        CaseKind::Kernel { n, acts } => {
            measure_kernel(implementation, spec.case, *n, *acts, repeat)
        }
        CaseKind::Replay {
            n,
            retract_fraction,
        } => {
            measure_replay(
                implementation,
                pool,
                spec.case,
                *n,
                *retract_fraction,
                repeat,
            )
            .await
        }
    }
}

async fn run_suite_specs(
    implementation: Implementation,
    pool: &PgPool,
    specs: &[CaseSpec],
    repeat: usize,
) -> Result<Vec<CaseResult>> {
    let mut results = Vec::with_capacity(specs.len());
    for (i, spec) in specs.iter().enumerate() {
        eprintln!("[{}/{}] {} ...", i + 1, specs.len(), spec.case);
        let result = run_case(implementation, pool, spec, repeat)
            .await
            .with_context(|| format!("suite case {}", spec.case))?;
        results.push(result);
    }
    Ok(results)
}

/// One planned case with all its parameters, so a table explains itself
/// without reading `suite_plan`.
#[derive(Debug, serde::Serialize)]
struct PlanEntry {
    case: &'static str,
    params: String,
}

#[derive(Debug, serde::Serialize)]
struct SuiteReport {
    suite_contract: u32,
    implementation: &'static str,
    ladder: &'static str,
    /// What the caller asked for; per-family caps mean the effective
    /// count per row is the row's own `samples` column, never this.
    requested_repeat: usize,
    pg_version: String,
    debug_assertions: bool,
    plan: Vec<PlanEntry>,
    cases: Vec<CaseResult>,
}

fn format_sample(value: Option<f64>) -> String {
    match value {
        Some(v) => format!("{v:.2}"),
        None => "-".to_string(),
    }
}

fn render_markdown(report: &SuiteReport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "suite_contract={} implementation={} ladder={} requested_repeat={}\n",
        report.suite_contract, report.implementation, report.ladder, report.requested_repeat
    ));
    out.push_str(&format!("pg=\"{}\"\n", report.pg_version));
    if report.debug_assertions {
        out.push_str("benchmark-grade=false: debug assertions enabled\n");
    }
    out.push('\n');
    out.push_str("Provenance (what forced each case family):\n");
    for (family, provenance) in CASE_PROVENANCE {
        out.push_str(&format!("- `{family}`: {provenance}\n"));
    }
    out.push('\n');
    out.push_str("Case definitions (the flags travel with the number):\n");
    for entry in &report.plan {
        out.push_str(&format!("- `{}` {}\n", entry.case, entry.params));
    }
    out.push('\n');
    out.push_str("| case | axis | point | metric | first | steady median | samples | unit |\n");
    out.push_str("|---|---|--:|---|--:|--:|--:|---|\n");
    for case in &report.cases {
        for m in &case.metrics {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} | {} |\n",
                case.case,
                case.axis,
                case.point,
                m.name,
                format_sample(m.first()),
                format_sample(m.steady_median()),
                m.samples.len(),
                m.unit
            ));
        }
    }
    out
}

async fn run_suite(args: SuiteArgs) -> Result<()> {
    check_reset_ack(args.reset, &args.database_url)?;
    require_positive_repeat(args.repeat)?;
    // Sized for the widest contend case; every other case uses a
    // handful of connections.
    let pool = PgPoolOptions::new()
        .max_connections(18)
        .connect(&morpholog_postgres::with_default_user(&args.database_url))
        .await
        .context("connect to PostgreSQL")?;
    require_migration_head(&pool).await?;
    let pg_version: String = sqlx::query_scalar("SELECT version()")
        .fetch_one(&pool)
        .await
        .context("SELECT version()")?;

    let specs = suite_plan(args.ladder);
    let plan = specs
        .iter()
        .map(|spec| PlanEntry {
            case: spec.case,
            params: format!("{:?}", spec.kind),
        })
        .collect();
    let cases = run_suite_specs(args.implementation, &pool, &specs, args.repeat).await?;
    let report = SuiteReport {
        suite_contract: SUITE_CONTRACT,
        implementation: args.implementation.label(),
        ladder: match args.ladder {
            Ladder::Quick => "quick",
            Ladder::Full => "full",
        },
        requested_repeat: args.repeat,
        pg_version,
        debug_assertions: cfg!(debug_assertions),
        plan,
        cases,
    };
    match args.format {
        OutputFormat::Markdown => print!("{}", render_markdown(&report)),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&report).context("serialise suite report")?
        ),
    }
    Ok(())
}

fn posting(i: usize, tag: &str) -> Transition {
    Transition {
        transformation_name: double_entry_ledger::post_simple_entry().name.clone(),
        args: vec![
            subj(&format!("entry_{tag}_{i}")),
            subj("d_2026_05_17"),
            subj("p_bench"),
            subj(&format!("account_{}", i % 2)),
            subj(&format!("account_{}", (i + 1) % 2)),
            dec(42),
        ],
        actor: Subject::from("bench"),
    }
}

/// The batch as the caller experiences it: the whole wait including
/// every retry and backoff, the retries spent, and whether it ever
/// committed within the budget. Exhausting the budget is a reading,
/// not an error: it is what a batch on a contended footprint does.
async fn transact_once(
    pool: &PgPool,
    compiled: &PgProgram,
    proposals: &[Proposal],
    max_retries: usize,
) -> Result<(Duration, u64, bool)> {
    let mut retries = 0u64;
    let t = Instant::now();
    loop {
        match propose_all_against_pg(pool, compiled, proposals).await {
            Ok(PgAtomicOutcome::Committed { acts }) => {
                if acts.len() != proposals.len() {
                    return Err(anyhow!(
                        "the batch committed {} of {} acts",
                        acts.len(),
                        proposals.len()
                    ));
                }
                return Ok((t.elapsed(), retries, true));
            }
            Ok(PgAtomicOutcome::Rejected { act, reason, .. }) => {
                return Err(anyhow!("the batch was refused at act {act}: {reason}"));
            }
            Err(PgError::SerializationFailure) => {
                retries += 1;
                if retries as usize > max_retries {
                    return Ok((t.elapsed(), retries, false));
                }
                tokio::time::sleep(Duration::from_micros(100 * retries)).await;
            }
            Err(e) => return Err(anyhow::Error::new(e).context("transact")),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn measure_transact(
    implementation: Implementation,
    pool: &PgPool,
    acts: usize,
    prepopulate: usize,
    writers: usize,
    max_retries: usize,
    writer_pause: Duration,
    repeat: usize,
) -> Result<CaseResult> {
    // The programmes whose index condition this scenario establishes.
    let cores: Vec<Program> = vec![double_entry_ledger::program()];
    let compiled = std::sync::Arc::new(implementation.program(double_entry_ledger::program())?);
    let mut atomic = Vec::with_capacity(repeat);
    let mut sequential = Vec::with_capacity(repeat);
    let mut batch_retries = Vec::with_capacity(repeat);
    let mut batch_committed = Vec::with_capacity(repeat);
    let mut writer_commits = Vec::with_capacity(repeat);
    let mut writer_retries = Vec::with_capacity(repeat);
    for round in 0..repeat {
        // The batch, with the writers racing it for as long as it runs.
        reset_db(pool).await?;
        establish(pool, implementation, &cores).await?;
        insert_n_entries(pool, prepopulate, 2).await?;
        analyze_claims(pool).await?;
        let proposals: Vec<Proposal> = (0..acts)
            .map(|i| Proposal::gateway(&posting(i, &format!("atomic_r{round}"))))
            .collect();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut handles = Vec::with_capacity(writers);
        for w in 0..writers {
            let pool = pool.clone();
            let compiled = compiled.clone();
            let stop = stop.clone();
            handles.push(tokio::spawn(async move {
                let mut tally = Tally::default();
                let mut op = 0usize;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let transition = posting(op, &format!("writer_r{round}_w{w}"));
                    one_op(
                        &pool,
                        &compiled,
                        &transition,
                        max_retries,
                        "transact writer",
                        &mut tally,
                    )
                    .await?;
                    op += 1;
                    tokio::time::sleep(writer_pause).await;
                }
                Ok::<Tally, anyhow::Error>(tally)
            }));
        }
        let (elapsed, retries, committed) =
            transact_once(pool, &compiled, &proposals, max_retries).await?;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut commits = 0u64;
        let mut retried = 0u64;
        for h in handles {
            let tally = h.await.context("join transact writer")??;
            commits += tally.committed;
            retried += tally.retries;
        }
        atomic.push(elapsed);
        batch_retries.push(retries as f64);
        batch_committed.push(if committed { 1.0 } else { 0.0 });
        writer_commits.push(commits as f64);
        writer_retries.push(retried as f64);

        // The same acts one by one, from the same book, uncontended.
        reset_db(pool).await?;
        establish(pool, implementation, &cores).await?;
        insert_n_entries(pool, prepopulate, 2).await?;
        analyze_claims(pool).await?;
        let t = Instant::now();
        for i in 0..acts {
            let outcome = propose_against_pg(
                pool,
                &compiled,
                &Proposal::gateway(&posting(i, &format!("single_r{round}"))),
            )
            .await?;
            if !matches!(outcome, PgProposalOutcome::Committed { .. }) {
                return Err(anyhow!("single act {i} did not commit"));
            }
        }
        sequential.push(t.elapsed());
    }
    Ok(CaseResult {
        case: if writers > 0 {
            "transact/contend"
        } else {
            "transact"
        }
        .to_string(),
        implementation: implementation.label(),
        axis: "acts",
        point: acts as u64,
        metrics: vec![
            Metric::ms("transact_all", &atomic),
            Metric::ms("single_each", &sequential),
            Metric::series("batch_retries", "count", batch_retries),
            Metric::series("batch_committed", "count", batch_committed),
            Metric::series("writer_commits", "count", writer_commits),
            Metric::series("writer_retries", "count", writer_retries),
        ],
    })
}

async fn run_transact(args: TransactArgs) -> Result<()> {
    check_reset_ack(args.reset, &args.database_url)?;
    require_positive_repeat(args.repeat)?;
    if args.acts == 0 {
        return Err(anyhow!("--acts must be at least 1"));
    }
    let pool = connect(&args.database_url).await?;
    require_migration_head(&pool).await?;
    println!(
        "scenario=transact acts={} prepopulate={} writers={} repeat={}",
        args.acts, args.prepopulate, args.writers, args.repeat
    );
    let result = measure_transact(
        args.implementation,
        &pool,
        args.acts,
        args.prepopulate,
        args.writers,
        args.max_retries,
        Duration::from_millis(args.writer_pause_ms),
        args.repeat,
    )
    .await?;
    print_case_human(&result);
    Ok(())
}

#[cfg(test)]
mod smoke {
    //! Runs every scenario once at minimal size and checks only that it
    //! completes, never a timing. This catches schema drift in the
    //! bench's own hand-written SQL on every database-backed test run,
    //! not just when someone runs the bench by hand.
    //!
    //! Skips when `DATABASE_URL` is unset. One test runs the scenarios in
    //! sequence because each truncates the schema; the database suites
    //! run with `--test-threads=1` for the same reason.
    use super::*;

    fn db_url() -> Option<String> {
        std::env::var("DATABASE_URL").ok().filter(|s| !s.is_empty())
    }

    fn one_case_report(implementation: &str, samples: Vec<f64>) -> ReadReport {
        ReadReport {
            suite_contract: SUITE_CONTRACT,
            implementation: implementation.to_string(),
            ladder: "quick".to_string(),
            cases: vec![ReadCase {
                case: "write/base".to_string(),
                axis: "n".to_string(),
                point: 100,
                metrics: vec![ReadMetric {
                    name: "propose_one".to_string(),
                    unit: "ms".to_string(),
                    samples,
                }],
            }],
        }
    }

    /// The compare table pairs rows by case, axis, point, and metric,
    /// reads each run's steady median (the first sample when there is
    /// only one), and refuses two rulers.
    #[test]
    fn compare_pairs_rows_and_refuses_different_rulers() {
        let report = |contract: u32, ladder: &str, unit: &str, samples: Vec<f64>| {
            let mut report = one_case_report("interpreted", samples);
            report.suite_contract = contract;
            report.ladder = ladder.to_string();
            report.cases[0].metrics[0].unit = unit.to_string();
            report
        };
        let before = report(SUITE_CONTRACT, "quick", "ms", vec![9.0, 4.0, 2.0, 3.0]);
        let mut after = report(SUITE_CONTRACT, "quick", "ms", vec![5.0, 1.5]);
        after.cases[0].metrics.push(ReadMetric {
            name: "phase_kernel".to_string(),
            unit: "ms".to_string(),
            samples: vec![1.0],
        });
        after.cases.push(ReadCase {
            case: "kernel/acts".to_string(),
            axis: "n".to_string(),
            point: 100,
            metrics: vec![ReadMetric {
                name: "acts_sequential".to_string(),
                unit: "ms".to_string(),
                samples: vec![2.0],
            }],
        });
        let before = [before];
        let table = render_compare(&before, &[after]).unwrap();
        assert!(
            table.contains(
                "| write/base | n | 100 | propose_one | 3.00 | 1.50 | 0.50 | too few runs | ms |"
            ),
            "{table}"
        );
        // A metric only the candidate has is named, inside a shared case
        // and in a case of its own, with its axis.
        assert!(
            table.contains("- after only: write/base n 100 phase_kernel (ms)"),
            "{table}"
        );
        assert!(
            table.contains("- after only: kernel/acts n 100 acts_sequential (ms)"),
            "{table}"
        );

        let other_ruler = report(SUITE_CONTRACT + 1, "quick", "ms", vec![1.0]);
        assert!(render_compare(&before, &[other_ruler]).is_err());
        let other_ladder = report(SUITE_CONTRACT, "full", "ms", vec![1.0]);
        assert!(render_compare(&before, &[other_ladder]).is_err());
        let other_unit = report(SUITE_CONTRACT, "quick", "s", vec![1.0]);
        assert!(render_compare(&before, &[other_unit]).is_err());
    }

    /// Two runs of one binary disagree far more than the repeats inside
    /// a run do. So one run a side proves nothing however cleanly its
    /// repeats separate, and four runs a side that separate do.
    #[test]
    fn compare_judges_runs_not_the_repeats_inside_them() {
        let run = |steady: f64| {
            one_case_report(
                "interpreted",
                vec![99.0, steady, steady + 0.1, steady, steady + 0.1],
            )
        };
        let row = |table: String| {
            table
                .lines()
                .find(|l| l.starts_with("| write/base"))
                .unwrap()
                .to_string()
        };
        assert!(
            row(render_compare(&[run(10.0)], &[run(12.0)]).unwrap()).contains("| too few runs |")
        );
        let before = [run(10.0), run(10.4), run(10.2), run(10.6)];
        let slower = [run(12.0), run(12.6), run(12.2), run(12.4)];
        let overlapping = [run(10.1), run(10.5), run(9.9), run(10.3)];
        assert!(row(render_compare(&before, &slower).unwrap()).contains("| higher |"));
        assert!(row(render_compare(&slower, &before).unwrap()).contains("| lower |"));
        assert!(row(render_compare(&before, &overlapping).unwrap()).contains("| within noise |"));
        let mut changed_plan = run(10.0);
        changed_plan.cases[0].point = 1000;
        assert!(render_compare(&[run(10.0), changed_plan], &slower).is_err());
        assert!(
            render_compare(
                &[run(10.0), one_case_report("compiled", vec![1.0])],
                &slower
            )
            .is_err()
        );
        // One run named four times is one run.
        let err = render_compare(&[run(10.0), run(10.0), run(10.0), run(10.0)], &slower)
            .unwrap_err()
            .to_string();
        assert!(err.contains("the same run is given twice"), "{err}");
    }

    /// The separation test itself: four runs a side, and complete
    /// separation, nothing less.
    #[test]
    fn the_verdict_calls_a_change_only_when_chance_cannot_explain_it() {
        let before = [10.0, 11.0, 10.5, 10.2];
        assert_eq!(verdict(&before, &[8.0, 8.4, 8.1, 8.9]), "lower");
        assert_eq!(verdict(&before, &[12.0, 11.5, 13.0, 12.2]), "higher");
        assert_eq!(verdict(&before, &[9.0, 10.3, 8.8, 9.1]), "within noise");
        // Four runs a side, whatever the other side has: many candidate
        // runs say nothing about how much the baseline varies.
        assert_eq!(
            verdict(&[10.0, 11.0, 10.5], &[1.0, 1.1, 1.2]),
            "too few runs"
        );
        assert_eq!(
            verdict(&[10.0, 11.0, 10.5], &[1.0, 1.1, 1.2, 1.3, 1.4, 1.5, 1.6]),
            "too few runs"
        );
        assert_eq!(verdict(&[], &[1.0]), "too few runs");
        // Equal readings on both sides are not a separation.
        assert_eq!(verdict(&before, &[10.0, 9.0, 9.5, 9.7]), "within noise");
    }

    #[tokio::test]
    async fn scenarios_smoke() {
        let Some(url) = db_url() else {
            eprintln!("DATABASE_URL unset; skipping bench smoke test");
            return;
        };
        // A database behind the migration head is refused with the
        // remedy named; afterwards the test migrates it back.
        let pool = connect(&url).await.expect("connect");
        sqlx::query("DELETE FROM morpholog.schema_migrations WHERE version = $1")
            .bind(morpholog_postgres::head_version())
            .execute(&pool)
            .await
            .expect("remove the head's record");
        let behind = run_write(ScenarioArgs {
            n: 1,
            accounts: 2,
            noise_claims: 0,
            database_url: url.clone(),
            reset: true,
            implementation: Implementation::Interpreted,
            repeat: 1,
        })
        .await;
        let message = format!(
            "{:?}",
            behind.expect_err("a database behind the head is refused")
        );
        assert!(
            message.contains("behind the Morpholog migration head"),
            "the refusal names the remedy: {message}"
        );
        morpholog_postgres::apply_migrations(&pool)
            .await
            .expect("bring the test database to the migration head");
        drop(pool);

        run_write(ScenarioArgs {
            n: 1,
            accounts: 2,
            noise_claims: 1,
            database_url: url.clone(),
            reset: true,
            implementation: Implementation::Interpreted,
            repeat: 2,
        })
        .await
        .expect("write scenario smoke");

        // Every scenario under both compiled configurations, so SQL
        // drift on that route and in index setup is caught too.
        for implementation in [Implementation::Compiled, Implementation::CompiledIndexed] {
            run_write(ScenarioArgs {
                n: 1,
                accounts: 2,
                noise_claims: 1,
                database_url: url.clone(),
                reset: true,
                implementation,
                repeat: 2,
            })
            .await
            .expect("write scenario smoke, compiled");
            run_read(ScenarioArgs {
                n: 1,
                accounts: 2,
                noise_claims: 1,
                database_url: url.clone(),
                reset: true,
                implementation,
                repeat: 2,
            })
            .await
            .expect("read scenario smoke, compiled");
            run_import(ImportArgs {
                n: 2,
                database_url: url.clone(),
                reset: true,
                implementation,
                repeat: 1,
            })
            .await
            .expect("import scenario smoke, compiled");
            run_transact(TransactArgs {
                acts: 2,
                prepopulate: 1,
                writers: 0,
                max_retries: 20,
                writer_pause_ms: 5,
                database_url: url.clone(),
                reset: true,
                implementation,
                repeat: 2,
            })
            .await
            .expect("transact scenario smoke, compiled");
            run_contend(ContendArgs {
                workers: 2,
                ops_per_worker: 2,
                prepopulate: 1,
                periods: 1,
                disjoint: false,
                max_retries: 20,
                database_url: url.clone(),
                reset: true,
                implementation,
                repeat: 1,
            })
            .await
            .expect("contend scenario smoke, compiled");
            run_contend(ContendArgs {
                workers: 2,
                ops_per_worker: 2,
                prepopulate: 0,
                periods: 2,
                disjoint: true,
                max_retries: 20,
                database_url: url.clone(),
                reset: true,
                implementation,
                repeat: 1,
            })
            .await
            .expect("predicate-disjoint contend scenario smoke, compiled");
            run_as_of(AsOfArgs {
                n: 4,
                at: 1.0,
                retract_fraction: 50,
                database_url: url.clone(),
                reset: true,
                implementation,
                repeat: 1,
            })
            .await
            .expect("as-of scenario smoke, compiled");
            run_wide(WideArgs {
                n: 2,
                arity: 4,
                database_url: url.clone(),
                reset: true,
                implementation,
                repeat: 1,
            })
            .await
            .expect("wide scenario smoke, compiled");
            run_replay(ReplayArgs {
                n: 2,
                retract_fraction: 50,
                repeat: 1,
                database_url: url.clone(),
                reset: true,
                implementation,
            })
            .await
            .expect("replay scenario smoke, compiled");
        }

        // Back to an unindexed configuration: the indexes the last run
        // provisioned must be gone before its fixture is built.
        run_write(ScenarioArgs {
            n: 1,
            accounts: 2,
            noise_claims: 0,
            database_url: url.clone(),
            reset: true,
            implementation: Implementation::Compiled,
            repeat: 1,
        })
        .await
        .expect("write scenario smoke, unindexed after indexed");

        // An operator's equivalent index contaminates the unindexed
        // condition; the bench refuses rather than measure it.
        let pool = connect(&url).await.expect("connect");
        sqlx::raw_sql(
            "CREATE INDEX bench_smoke_external ON morpholog.claims \
             USING btree ((morpholog.claim_digest(morpholog.value_key_v1(arguments -> 0)))) \
             WHERE predicate_name = 'JournalEntry'",
        )
        .execute(&pool)
        .await
        .expect("external index");
        let refused = run_write(ScenarioArgs {
            n: 1,
            accounts: 2,
            noise_claims: 0,
            database_url: url.clone(),
            reset: true,
            implementation: Implementation::Compiled,
            repeat: 1,
        })
        .await;
        sqlx::raw_sql("DROP INDEX morpholog.bench_smoke_external")
            .execute(&pool)
            .await
            .expect("drop external index");
        let message = format!(
            "{:?}",
            refused.expect_err("an external equivalent is refused")
        );
        assert!(
            message.contains("not what the database holds"),
            "the refusal names the condition: {message}"
        );

        run_read(ScenarioArgs {
            n: 1,
            accounts: 2,
            noise_claims: 1,
            database_url: url.clone(),
            reset: true,
            implementation: Implementation::Interpreted,
            repeat: 2,
        })
        .await
        .expect("read scenario smoke");

        run_transact(TransactArgs {
            acts: 2,
            prepopulate: 1,
            writers: 0,
            max_retries: 20,
            writer_pause_ms: 5,
            database_url: url.clone(),
            reset: true,
            implementation: Implementation::Interpreted,
            repeat: 2,
        })
        .await
        .expect("transact scenario smoke");

        run_as_of(AsOfArgs {
            n: 4,
            at: 1.0,
            retract_fraction: 0,
            database_url: url.clone(),
            reset: true,
            implementation: Implementation::Interpreted,
            repeat: 2,
        })
        .await
        .expect("as-of scenario smoke (asserts only)");

        // Retract-heavy: exercises the `actor` column and the retract
        // branch of the fabricator and the replay path.
        run_as_of(AsOfArgs {
            n: 10,
            at: 1.0,
            retract_fraction: 50,
            database_url: url.clone(),
            reset: true,
            implementation: Implementation::Interpreted,
            repeat: 1,
        })
        .await
        .expect("as-of scenario smoke (retract-heavy)");

        run_contend(ContendArgs {
            workers: 2,
            ops_per_worker: 2,
            prepopulate: 2,
            periods: 2,
            disjoint: false,
            max_retries: 20,
            database_url: url.clone(),
            reset: true,
            implementation: Implementation::Interpreted,
            repeat: 2,
        })
        .await
        .expect("contend scenario smoke (ledger)");

        // The synthetic disjoint-predicate workload uses a different
        // (ir_builder-built) transformation, so smoke it too.
        run_contend(ContendArgs {
            workers: 2,
            ops_per_worker: 2,
            prepopulate: 0,
            periods: 2,
            disjoint: true,
            max_retries: 20,
            database_url: url.clone(),
            reset: true,
            implementation: Implementation::Interpreted,
            repeat: 1,
        })
        .await
        .expect("contend scenario smoke (disjoint)");

        run_import(ImportArgs {
            n: 2,
            database_url: url.clone(),
            reset: true,
            implementation: Implementation::Interpreted,
            repeat: 2,
        })
        .await
        .expect("import scenario smoke");

        run_wide(WideArgs {
            n: 1,
            arity: 13,
            database_url: url.clone(),
            reset: true,
            implementation: Implementation::Interpreted,
            repeat: 2,
        })
        .await
        .expect("wide scenario smoke");

        // The suite plumbing over a tiny private plan, one case per
        // family. The quick ladder is a real measurement run, too slow
        // for CI.
        let pool = PgPool::connect(&morpholog_postgres::with_default_user(&url))
            .await
            .expect("suite smoke pool");
        let plan = vec![
            CaseSpec {
                case: "write/base",
                kind: CaseKind::Write {
                    n: 1,
                    accounts: 2,
                    noise: 0,
                },
            },
            CaseSpec {
                case: "asof/assert",
                kind: CaseKind::AsOf {
                    n: 2,
                    retract_fraction: 0,
                },
            },
            CaseSpec {
                case: "contend/disjoint",
                kind: CaseKind::Contend {
                    workers: 1,
                    ops: 1,
                    prepopulate: 0,
                    periods: 1,
                    disjoint: true,
                },
            },
            CaseSpec {
                case: "import/core",
                kind: CaseKind::Import { n: 2 },
            },
            CaseSpec {
                case: "wide/size",
                kind: CaseKind::Wide {
                    axis: "n",
                    n: 1,
                    arity: 4,
                },
            },
            CaseSpec {
                case: "kernel/acts",
                kind: CaseKind::Kernel { n: 2, acts: 2 },
            },
            CaseSpec {
                case: "replay/retract",
                kind: CaseKind::Replay {
                    n: 2,
                    retract_fraction: 50,
                },
            },
        ];
        for implementation in [Implementation::Compiled, Implementation::CompiledIndexed] {
            let cases = run_suite_specs(implementation, &pool, &plan, 1)
                .await
                .expect("suite smoke plan, compiled");
            assert!(
                cases
                    .iter()
                    .all(|c| c.implementation == implementation.label()),
                "every row carries the configuration's label"
            );
        }
        let cases = run_suite_specs(Implementation::Interpreted, &pool, &plan, 2)
            .await
            .expect("suite smoke plan");
        assert_eq!(cases.len(), plan.len(), "every smoke case reports");
        let report = SuiteReport {
            suite_contract: SUITE_CONTRACT,
            implementation: "interpreted",
            ladder: "smoke",
            requested_repeat: 2,
            pg_version: "smoke".to_string(),
            debug_assertions: cfg!(debug_assertions),
            plan: plan
                .iter()
                .map(|spec| PlanEntry {
                    case: spec.case,
                    params: format!("{:?}", spec.kind),
                })
                .collect(),
            cases,
        };
        let rendered = render_markdown(&report);
        for spec in &plan {
            assert!(
                rendered.contains(spec.case),
                "the rendered table names every case; missing {}:\n{rendered}",
                spec.case
            );
        }
    }

    /// The renderer is pure, so its shape is pinned without a database:
    /// header, provenance mapping, and one row per metric.
    #[test]
    fn markdown_renderer_shape() {
        let report = SuiteReport {
            suite_contract: SUITE_CONTRACT,
            implementation: "interpreted",
            ladder: "unit",
            requested_repeat: 3,
            pg_version: "PostgreSQL test".to_string(),
            debug_assertions: false,
            plan: vec![PlanEntry {
                case: "write/base",
                params: "Write { n: 100, accounts: 2, noise: 0 }".to_string(),
            }],
            cases: vec![CaseResult {
                case: "write/base".to_string(),
                implementation: "interpreted",
                axis: "n",
                point: 100,
                metrics: vec![
                    Metric::series("propose_one", "ms", vec![5.0, 2.0, 3.0]),
                    Metric::series("scoped_claims", "count", vec![300.0]),
                ],
            }],
        };
        let rendered = render_markdown(&report);
        assert!(rendered.contains(&format!("suite_contract={SUITE_CONTRACT}")));
        assert!(rendered.contains("requested_repeat=3"));
        // The flags travel with the number.
        assert!(rendered.contains("- `write/base` Write { n: 100, accounts: 2, noise: 0 }"));
        assert!(
            rendered.contains(
                "| case | axis | point | metric | first | steady median | samples | unit |"
            )
        );
        // first = samples[0]; the steady median of [2.0, 3.0] averages
        // the two middles; the samples column is the row's own count,
        // since some families cap their repeats.
        assert!(rendered.contains("| write/base | n | 100 | propose_one | 5.00 | 2.50 | 3 | ms |"));
        // A single-sample metric has no steady median.
        assert!(
            rendered.contains("| write/base | n | 100 | scoped_claims | 300.00 | - | 1 | count |")
        );
        assert!(!rendered.contains("benchmark-grade=false"));
    }

    /// Pins fingerprints of the canonical plans under the current suite
    /// contract, so `suite_plan` cannot change without a contract bump.
    #[test]
    fn the_canonical_matrix_is_frozen_under_contract_2() {
        use sha2::{Digest, Sha256};
        let fingerprint = |ladder: Ladder| {
            let canonical: String = suite_plan(ladder)
                .iter()
                .map(|spec| format!("{}|{:?}\n", spec.case, spec.kind))
                .collect();
            let digest = Sha256::digest(canonical.as_bytes());
            digest.iter().fold(String::new(), |mut out, b| {
                use std::fmt::Write;
                let _ = write!(out, "{b:02x}");
                out
            })
        };
        assert_eq!(SUITE_CONTRACT, 2, "bumping the contract re-pins these");
        assert_eq!(
            fingerprint(Ladder::Quick),
            "ffd974c7fe2fa3624d92e3a94b2b8df3d43f3dd3a2e197483e66edbef369cb37",
            "the quick matrix changed without a contract bump"
        );
        assert_eq!(
            fingerprint(Ladder::Full),
            "d2fecec898de1d87f0ff08817f92da0867b552d78e229425bc56b1a3671a7a24",
            "the full matrix changed without a contract bump"
        );
    }
}
