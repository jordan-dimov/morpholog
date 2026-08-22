//! Morpholog scale-pressure benchmark.
//!
//! Synthetic benchmark for understanding how the runtime behaves as
//! state grows and as proposals contend. The scenarios:
//!
//! - `write` populates the claims table with N pre-existing journal
//!   entries via direct SQL, then times one `propose_against_pg`
//!   call. Measures load_state + invariant evaluation + commit as a
//!   function of pre-state size.
//! - `read` uses the same fixture and times the three phases of the
//!   read path separately (`list_claims_for_predicates`,
//!   `State::from_claims`, `enumerate_derived`). Measures where the
//!   read time goes as N and the `--accounts K` axis grow.
//! - `as-of` fabricates N audit transitions directly via SQL
//!   (bypassing the kernel) and times one `reconstruct_state_at`
//!   plus one `list_derived_at` against a target transition.
//!   Measures audit-log replay cost as a function of N, the
//!   `--at <fraction>` axis, and `--retract-fraction K` (what share
//!   of the log retracts prior claims instead of asserting fresh
//!   ones - the purely-additive default is best-case for replay).
//! - `contend` runs W concurrent workers issuing `propose_against_pg`
//!   into one shared period, each with the SERIALIZABLE 40001 retry
//!   loop a real embedder owns. Measures throughput and the
//!   serialization-conflict retry rate as `--workers` grows - the
//!   axis the single-propose scenarios cannot see.
//! - `import` commits N entries sequentially from an empty book - the
//!   cumulative core-import curve `write` cannot see, and the
//!   in-process core of the embedder import/replay workload.
//! - `wide` proposes against, and reads back, a synthetic wide
//!   predicate (default arity 13, the widest consumer-reported claim
//!   shape) - the argument-count axis.
//! - `suite` runs the frozen canonical case matrix across per-case
//!   ladders and prints one table with provenance - the whole-suite
//!   evidence a performance PR carries (docs/benchmarking.md owns the
//!   discipline).
//!
//! Every scenario takes `--repeat`: repeats start from the same
//! logical pre-state (mutating scenarios rebuild their fixture), the
//! first sample reports as `first`, the median over the rest as
//! `steady median`.
//!
//! The `write` / `read` fixture distributes lines across `K`
//! accounts via modular arithmetic; the `as-of` fixture is uniform
//! (every fabricated transition uses the same two accounts) because
//! grouping cost is not what the as-of scenario measures.
//!
//! Numbers printed here are NOT regression assertions. They are
//! exploratory measurements meant to surface bottlenecks. Do not
//! treat them as CI gates; do not check captured numbers into the
//! repo as expected values.
//!
//! Truncates the entire morpholog schema before each run so prior
//! state cannot contaminate measurement. This binary is destructive
//! against whatever database it is pointed at; do not run it against
//! a database with anything you want to keep.

use anyhow::{Context, Result, anyhow};
use clap::{Parser, Subcommand};
use morpholog_core::{
    CompiledProgram, EvalValue, State, Subject, Transformation, Transition, enumerate_derived,
    predicates_referenced_by_derived,
};
use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{
    PgError, PgPool, PgProposalOutcome, Proposal, list_claims_for_predicates, list_derived_at,
    propose_against_pg, reconstruct_state_at,
};
use rust_decimal::Decimal;
use sqlx::postgres::PgPoolOptions;
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

    /// Propose against, and read back, a synthetic WIDE predicate
    /// (default arity 13 - the widest consumer-reported claim shape).
    /// The gallery's widest predicate is 7-ary, so this is the carrier
    /// for how argument count moves write and read cost.
    Wide(WideArgs),

    /// Run the frozen canonical case matrix across per-case ladders and
    /// print one table (markdown by default; `--format json` for
    /// machine comparison). This is the whole-suite evidence a
    /// performance PR carries; see docs/benchmarking.md for the
    /// discipline. Sequential and destructive; the full ladder takes
    /// tens of minutes on today's interpreted runtime.
    Suite(SuiteArgs),
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
    /// non-linearity in the `ReplaySet` retract path. Capped at 50
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

/// Refuses to proceed unless `--reset` was explicitly passed. The
/// target URL is echoed so the operator can see what is about to be
/// truncated; this is the closest the binary gets to a "are you sure"
/// dialog without making scripted use awkward.
fn require_reset_ack(args: &ScenarioArgs) -> Result<()> {
    check_reset_ack(args.reset, &args.database_url)
}

fn require_reset_ack_as_of(args: &AsOfArgs) -> Result<()> {
    check_reset_ack(args.reset, &args.database_url)
}

/// Shared body for `--reset` acknowledgement, called from each
/// scenario-specific guard.
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

/// `--accounts 0` would break the fixture's modulo-K distribution.
/// PostgreSQL raises `SQLSTATE 22012 (division_by_zero)` on integer
/// modulo by zero, and conceptually there must be at least one
/// account for any journal line to land on.
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
        Command::Suite(args) => run_suite(args).await,
    }
}

// ============================================================
// Measurement layer
// ============================================================

/// The implementation column of every result row. #277's compiled
/// engine will add its own value; the table contract does not change.
const IMPLEMENTATION: &str = "interpreted";

/// Bumped only when benchmark semantics change (cases, fixtures,
/// ladders, aggregation) - never by an implementation being measured.
/// The distinction between changing the machine and changing the
/// ruler; see docs/benchmarking.md.
const SUITE_CONTRACT: u32 = 1;

/// One measured metric of one case: named, unit-tagged samples in
/// repeat order. `samples[0]` is the `first` reading - deliberately
/// not called "cold": the fixture insert has just warmed the buffers,
/// so all the instrument can defend is "first timed invocation after
/// fixture construction". The steady median is over the rest.
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
        let mut rest: Vec<f64> = self.samples.get(1..).unwrap_or(&[]).to_vec();
        if rest.is_empty() {
            return None;
        }
        rest.sort_by(f64::total_cmp);
        Some(rest[rest.len() / 2])
    }
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
/// measured query is not also the query that pays for stale stats -
/// the spike's lesson, carried over. A closed table set, so the SQL
/// stays static.
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
    pool: &PgPool,
    case: &str,
    n: usize,
    accounts: usize,
    noise_claims: usize,
    repeat: usize,
) -> Result<CaseResult> {
    let transformation = double_entry_ledger::post_simple_entry();
    let compiled = CompiledProgram::new(double_entry_ledger::program())
        .map_err(|e| anyhow!("invalid programme: {e:?}"))?;
    let mut fixture = Vec::with_capacity(repeat);
    let mut propose = Vec::with_capacity(repeat);
    for r in 0..repeat {
        let t = Instant::now();
        reset_db(pool).await?;
        insert_n_entries(pool, n, accounts).await?;
        insert_noise_claims(pool, noise_claims).await?;
        fixture.push(t.elapsed());
        analyze_claims(pool).await?;

        let transition = Transition {
            transformation_name: transformation.name.clone(),
            args: vec![
                subj(&format!("entry_bench_target_{r}")),
                subj("d_2026_05_17"),
                subj("p_bench"),
                subj("account_cash"),
                subj("account_revenue"),
                dec(42),
            ],
            actor: Subject::from("bench"),
        };
        let t = Instant::now();
        let outcome = propose_against_pg(pool, &compiled, &Proposal::gateway(&transition))
            .await
            .context("propose_against_pg")?;
        propose.push(t.elapsed());
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
        implementation: IMPLEMENTATION,
        axis: "n",
        point: n as u64,
        metrics: vec![
            Metric::ms("fixture_build", &fixture),
            Metric::ms("propose_one", &propose),
        ],
    })
}

async fn run_write(args: ScenarioArgs) -> Result<()> {
    require_reset_ack(&args)?;
    require_positive_k(&args)?;
    require_positive_repeat(args.repeat)?;
    let pool = PgPool::connect(&morpholog_postgres::with_default_user(&args.database_url))
        .await
        .context("connect to PostgreSQL")?;
    println!(
        "scenario=write n={} accounts={} noise_claims={} repeat={}",
        args.n, args.accounts, args.noise_claims, args.repeat
    );
    let result = measure_write(
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

/// The read case: one immutable fixture, reused across repeats (the
/// pre-state never mutates, so re-reading it IS the same workload);
/// the three phases of the read path timed separately, semantics
/// identical to `list_derived` (the split is diagnostic).
async fn measure_read(
    pool: &PgPool,
    case: &str,
    n: usize,
    accounts: usize,
    noise_claims: usize,
    repeat: usize,
) -> Result<CaseResult> {
    let t = Instant::now();
    reset_db(pool).await?;
    insert_n_entries(pool, n, accounts).await?;
    insert_noise_claims(pool, noise_claims).await?;
    let fixture = t.elapsed();
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

        // Bounds on the derived-row count given the fixture shape:
        // 0 rows iff n=0, otherwise `0 < rows <= k` (the K-account
        // modular cycle caps the distinct accounts touched).
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
        implementation: IMPLEMENTATION,
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
    require_reset_ack(&args)?;
    require_positive_k(&args)?;
    require_positive_repeat(args.repeat)?;
    let pool = PgPool::connect(&morpholog_postgres::with_default_user(&args.database_url))
        .await
        .context("connect to PostgreSQL")?;
    println!(
        "scenario=read n={} accounts={} noise_claims={} repeat={}",
        args.n, args.accounts, args.noise_claims, args.repeat
    );
    let result = measure_read(
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
    pool: &PgPool,
    case: &str,
    n: usize,
    at: f64,
    retract_fraction: usize,
    repeat: usize,
) -> Result<CaseResult> {
    let retract_stride = retract_stride_for(retract_fraction);
    let retract_count = if retract_stride == 0 {
        0
    } else {
        n as i64 / retract_stride
    };

    let t = Instant::now();
    reset_db(pool).await?;
    fabricate_audit_rows(pool, n, retract_stride).await?;
    let fixture = t.elapsed();
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
        implementation: IMPLEMENTATION,
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
    require_reset_ack_as_of(&args)?;
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
    let pool = PgPool::connect(&morpholog_postgres::with_default_user(&args.database_url))
        .await
        .context("connect to PostgreSQL")?;
    println!(
        "scenario=as-of n={} at={} retract_fraction={} repeat={}",
        args.n, args.at, args.retract_fraction, args.repeat
    );
    let result = measure_as_of(
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
async fn contend_burst(
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

/// The contend case: every repeat rebuilds the prepopulated pre-state
/// and races the same burst over it. `require_clean` is the canonical
/// suite's validity rule: a row with failed (or rejected) operations
/// is not a comparable measurement - an optimiser must not look
/// faster because work stopped succeeding - so the case errors
/// instead of reporting.
#[allow(clippy::too_many_arguments)]
async fn measure_contend(
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
        insert_n_entries(pool, prepopulate, 2).await?;
        fixture.push(t.elapsed());
        analyze_claims(pool).await?;

        let (total, elapsed) = contend_burst(
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
        implementation: IMPLEMENTATION,
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

    // Pool sized to the worker count: each in-flight propose holds one
    // connection for the life of its SERIALIZABLE transaction, so a
    // smaller pool would serialise the workers at the connection layer
    // and hide the very contention this scenario means to measure.
    let pool = PgPoolOptions::new()
        .max_connections(args.workers as u32 + 2)
        .connect(&morpholog_postgres::with_default_user(&args.database_url))
        .await
        .context("connect to PostgreSQL")?;
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

/// One worker's slice of the contended workload: `ops` sequential
/// proposals against a uniquely-keyed item. In the default ledger
/// workload each worker posts into period `worker_id mod periods` but
/// shares the journal-line *predicate* footprint, so raising `--periods`
/// partitions by value and does not relieve contention. With
/// `--disjoint`, each worker's footprint is its own predicate
/// (`Bench_{worker_id mod periods}`), so `--periods >= workers` makes the
/// workers genuinely disjoint. Either workload carries the SERIALIZABLE
/// retry loop a real embedder owns (see [`one_op`]).
async fn contend_worker(
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
        // The whole footprint is one predicate, `Bench_{w mod periods}`:
        // with `periods >= workers` every worker is on its own predicate
        // (disjoint footprints); with `periods == 1` they all share
        // `Bench_0`. The same `--periods` knob that partitions by *value*
        // in the ledger workload here partitions by *predicate*.
        let predicate = format!("Bench_{}", worker_id % periods);
        let transformation = synthetic_bump(&predicate);
        let compiled = CompiledProgram::new(synthetic_program(&predicate))
            .map_err(|e| anyhow!("invalid programme: {e:?}"))?;
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
        let transformation = double_entry_ledger::post_simple_entry();
        let compiled = CompiledProgram::new(double_entry_ledger::program())
            .map_err(|e| anyhow!("invalid programme: {e:?}"))?;
        let period = format!("p_contend_{}", worker_id % periods);
        for op in 0..ops {
            let transition = Transition {
                transformation_name: transformation.name.clone(),
                args: vec![
                    subj(&format!("entry_r{round}_w{worker_id}_op{op}")),
                    subj("d_2026_05_17"),
                    subj(&period),
                    subj("account_cash"),
                    subj("account_revenue"),
                    dec(42),
                ],
                actor: Subject::from("bench"),
            };
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

/// Propose one transition with the caller-owned SERIALIZABLE retry loop,
/// folding the outcome into `tally`. A 40001 backs off and retries up to
/// `max_retries` (then counts as `failed`); any other error is drift,
/// not contention, and propagates as `Err` so a real run and the smoke
/// test fail loudly instead of banking it as an expected outcome.
async fn one_op(
    pool: &PgPool,
    compiled: &CompiledProgram,
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

/// A minimal synthetic transformation whose entire read/write footprint
/// is one predicate: `require not <predicate>(item)` then
/// `admit <predicate>(item)`. Workers given distinct predicates have
/// disjoint footprints and do not contend under SSI; workers sharing one
/// predicate do. Built via `ir_builder` because the ledger example's
/// predicates are fixed and cannot be made per-worker.
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

/// A minimal valid programme wrapping [`synthetic_bump`] so it can be
/// proposed through the `CompiledProgram` facade: the bumped predicate
/// declared, plus the bump transformation.
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

/// The import case. Provenance: the external embedder's import path
/// that forced `propose --batch` (design-history: "the first
/// throughput lever the bench's contend axis names"), Redline's
/// 130-act WAN seed, and grid-mysteries' CI replay. This is the
/// in-process CORE of that workload - each commit here is what the
/// real batch wraps in NDJSON parsing, argument decoding, and receipt
/// serialisation.
///
/// From an empty book, N sequential kernel commits. Per-commit cost
/// grows with the book on the interpreted runtime, so the journey is
/// roughly quadratic in N - the decile split (mean of the first vs
/// last tenth of per-commit latencies) is the growth signal. Every
/// repeat re-truncates: the whole 0->N journey IS the sample.
async fn measure_import(pool: &PgPool, case: &str, n: usize, repeat: usize) -> Result<CaseResult> {
    if n == 0 {
        return Err(anyhow!("import requires n >= 1"));
    }
    let transformation = double_entry_ledger::post_simple_entry();
    let compiled = CompiledProgram::new(double_entry_ledger::program())
        .map_err(|e| anyhow!("invalid programme: {e:?}"))?;
    let mut total_s = Vec::with_capacity(repeat);
    let mut rows_per_s = Vec::with_capacity(repeat);
    let mut first_decile = Vec::with_capacity(repeat);
    let mut last_decile = Vec::with_capacity(repeat);
    for r in 0..repeat {
        reset_db(pool).await?;
        let mut per_commit = Vec::with_capacity(n);
        let journey = Instant::now();
        for i in 0..n {
            let transition = Transition {
                transformation_name: transformation.name.clone(),
                args: vec![
                    subj(&format!("entry_import_r{r}_{i}")),
                    subj("d_2026_05_17"),
                    subj("p_import"),
                    subj("account_cash"),
                    subj("account_revenue"),
                    dec(42),
                ],
                actor: Subject::from("bench"),
            };
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
        implementation: IMPLEMENTATION,
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
    let pool = PgPool::connect(&morpholog_postgres::with_default_user(&args.database_url))
        .await
        .context("connect to PostgreSQL")?;
    println!("scenario=import n={} repeat={}", args.n, args.repeat);
    let result = measure_import(&pool, "import", args.n, args.repeat).await?;
    print_case_human(&result);
    Ok(())
}

// ============================================================
// wide: the argument-count axis
// ============================================================

/// The wide-predicate programme. Provenance: the billing embedder's
/// 13-ary `InvoiceLine` (whose positional `rate_uni` typo was the
/// live near-miss that forced named-field patterns) and
/// grid-mysteries' `EvidenceMetric`; the gallery tops out at 7-ary,
/// so this synthetic carrier owns the axis. Shape: a line key, a
/// group, an amount, and subject padding to `arity`; a grouped-sum
/// invariant so the write path pays realistic invariant work over
/// the wide rows.
fn wide_program(arity: usize) -> morpholog_core::Program {
    use morpholog_core::ir_builder as b;
    let mut decl = b::predicate("WideLine")
        .subject("line")
        .subject("grp")
        .decimal("amount");
    for i in 3..arity {
        decl = decl.subject(&format!("pad_{i}"));
    }

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

    b::program("wide_bench")
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
        .build()
}

fn wide_field_names(arity: usize) -> Vec<String> {
    let mut names = vec!["line".to_string(), "grp".to_string(), "amount".to_string()];
    names.extend((3..arity).map(|i| format!("pad_{i}")));
    names
}

/// Insert `n` wide rows directly. The SQL is assembled from the arity
/// (a bench-internal integer, never external input), hence the
/// explicit `AssertSqlSafe`.
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

/// The wide case: every repeat rebuilds N wide rows, times the scoped
/// read (list + build_state - no derived claim; the predicate itself
/// is the payload), then one proposal through the grouped-sum
/// invariant.
async fn measure_wide(
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
    let compiled =
        CompiledProgram::new(program).map_err(|e| anyhow!("invalid wide programme: {e:?}"))?;
    let footprint = vec!["WideLine".to_string()];

    let mut fixture = Vec::with_capacity(repeat);
    let mut list_scoped = Vec::with_capacity(repeat);
    let mut build_state = Vec::with_capacity(repeat);
    let mut propose = Vec::with_capacity(repeat);
    for r in 0..repeat {
        let t = Instant::now();
        reset_db(pool).await?;
        insert_wide_rows(pool, n, arity).await?;
        fixture.push(t.elapsed());
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
        implementation: IMPLEMENTATION,
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
    let pool = PgPool::connect(&morpholog_postgres::with_default_user(&args.database_url))
        .await
        .context("connect to PostgreSQL")?;
    println!(
        "scenario=wide n={} arity={} repeat={}",
        args.n, args.arity, args.repeat
    );
    let result = measure_wide(&pool, "wide", "n", args.n, args.arity, args.repeat).await?;
    print_case_human(&result);
    Ok(())
}

/// Fabricate `n` audit rows via direct SQL. Each row carries a
/// 3-claim assertion payload (one JournalEntry + two JournalLines
/// against the fixed `account_cash` / `account_revenue` pair),
/// matching the shape `post_simple_entry` would produce.
///
/// All rows land in a single `INSERT ... SELECT ... FROM generate_series`
/// so the fixture builds in `O(1)` round-trips regardless of `n`.
/// The cost is in PG planning, JSONB construction, and the audit
/// table's primary-key index maintenance.
///
/// **Why direct SQL, not chained `propose_against_pg` calls.** The
/// bench measures replay cost, not write cost. Going through
/// `propose_against_pg` would pay the per-transition kernel evaluation
/// and SERIALIZABLE-transaction overhead for every row, making fixture
/// build the dominant cost at large N. Direct SQL bypasses that and
/// produces audit rows whose shape (asserted_claims, retracted_claims,
/// committed_at) is exactly what the replay reads.
///
/// **`transition_id` uses `gen_random_uuid()` (UUIDv4), not UUIDv7.**
/// Replay correctness depends on the `(committed_at, transition_id)`
/// row ordering, not on UUID byte order, so UUIDv4 is fine. The
/// fabricated rows do use a strictly monotone `committed_at = now()
/// + i microseconds` to keep replay order deterministic.
///
/// **Retracts.** With `retract_stride > 0`, every `stride`-th
/// transition retracts the payload asserted by the immediately prior
/// transition (`target = i - 1`) instead of asserting a fresh entry.
/// Because the stride is at least 2, `i - 1` is always an assert and
/// is still live when the retract replays, so the asserts-only
/// invariant (every retracted claim was asserted earlier in causal
/// order) holds. `stride = 0` reproduces the original asserts-only
/// log. The payload is built once from `target` and routed to either
/// `asserted_claims` or `retracted_claims` by `is_retract`.
async fn fabricate_audit_rows(pool: &PgPool, n: usize, retract_stride: i64) -> Result<()> {
    let n_i: i64 = n
        .try_into()
        .map_err(|_| anyhow!("n={n} too large for i64"))?;

    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents,
            committed_at, attestation
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
            '{\"mode\":\"gateway\",\"authenticated_by\":\"bench-fixture\"}'::jsonb
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

/// Insert `n` synthetic journal entries (one JournalEntry plus two
/// JournalLines per entry) into `morpholog.claims` via three SQL
/// statements. Each statement uses `generate_series` so the entire
/// fixture lands in `O(1)` round-trips regardless of `n`; the cost is
/// in the planner and the JSONB construction, not in the wire.
///
/// Lines are distributed across `k` accounts via modular arithmetic:
/// entry `i` debits `account_{i mod k}` and credits
/// `account_{(i + 1) mod k}` for the same amount, so every entry is
/// self-balancing (`balanced_posted_entry` invariant holds). Period
/// is `p_bench`; no `PeriodClosed` claim is inserted, so the require
/// in `post_simple_entry` passes for any follow-up `write` propose.
///
/// `asserted_in` uses `Uuid::nil()` as a synthetic fixture transition id
/// (the same pattern the integration-test fixtures use). This row does
/// not appear in the `audit` table; the schema does not enforce
/// referential integrity from `claims.asserted_in` to `audit.transition_id`.
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

/// Insert `count` rows of `UnrelatedNoise(noise_i, i)` directly into
/// `morpholog.claims`. The predicate is never referenced by the
/// double-entry-ledger programme; a correct scoped `load_state`
/// skips these entirely. With the older unscoped loader, they show
/// up as linear fetch + decode cost in `propose_one`.
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
        "audit-log replay; /retract is the ReplaySet retract-path control \
         (the asserts-only default is best-case)",
    ),
    (
        "contend",
        "the SSI concurrency law: /shared shows value partitioning does not \
         relieve 40001 pressure, /disjoint shows predicate partitioning does; \
         workers=1 is the non-contention baseline",
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
async fn run_case(pool: &PgPool, spec: &CaseSpec, repeat: usize) -> Result<CaseResult> {
    match &spec.kind {
        CaseKind::Write { n, accounts, noise } => {
            measure_write(pool, spec.case, *n, *accounts, *noise, repeat).await
        }
        CaseKind::Read { n, accounts, noise } => {
            measure_read(pool, spec.case, *n, *accounts, *noise, repeat).await
        }
        CaseKind::AsOf {
            n,
            retract_fraction,
        } => measure_as_of(pool, spec.case, *n, 1.0, *retract_fraction, repeat).await,
        CaseKind::Contend {
            workers,
            ops,
            prepopulate,
            periods,
            disjoint,
        } => {
            measure_contend(
                pool,
                spec.case,
                *workers,
                *ops,
                *prepopulate,
                *periods,
                *disjoint,
                100,
                repeat.min(3),
                true,
            )
            .await
        }
        CaseKind::Import { n } => measure_import(pool, spec.case, *n, repeat.min(3)).await,
        CaseKind::Wide { axis, n, arity } => {
            measure_wide(pool, spec.case, axis, *n, *arity, repeat).await
        }
    }
}

async fn run_suite_specs(
    pool: &PgPool,
    specs: &[CaseSpec],
    repeat: usize,
) -> Result<Vec<CaseResult>> {
    let mut results = Vec::with_capacity(specs.len());
    for (i, spec) in specs.iter().enumerate() {
        eprintln!("[{}/{}] {} ...", i + 1, specs.len(), spec.case);
        let result = run_case(pool, spec, repeat)
            .await
            .with_context(|| format!("suite case {}", spec.case))?;
        results.push(result);
    }
    Ok(results)
}

#[derive(Debug, serde::Serialize)]
struct SuiteReport {
    suite_contract: u32,
    implementation: &'static str,
    ladder: &'static str,
    repeat: usize,
    pg_version: String,
    debug_assertions: bool,
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
        "suite_contract={} implementation={} ladder={} repeat={}\n",
        report.suite_contract, report.implementation, report.ladder, report.repeat
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
    out.push_str("| case | axis | point | metric | first | steady median | unit |\n");
    out.push_str("|---|---|--:|---|--:|--:|---|\n");
    for case in &report.cases {
        for m in &case.metrics {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} | {} |\n",
                case.case,
                case.axis,
                case.point,
                m.name,
                format_sample(m.first()),
                format_sample(m.steady_median()),
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
    let pg_version: String = sqlx::query_scalar("SELECT version()")
        .fetch_one(&pool)
        .await
        .context("SELECT version()")?;

    let specs = suite_plan(args.ladder);
    let cases = run_suite_specs(&pool, &specs, args.repeat).await?;
    let report = SuiteReport {
        suite_contract: SUITE_CONTRACT,
        implementation: IMPLEMENTATION,
        ladder: match args.ladder {
            Ladder::Quick => "quick",
            Ladder::Full => "full",
        },
        repeat: args.repeat,
        pg_version,
        debug_assertions: cfg!(debug_assertions),
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

#[cfg(test)]
mod smoke {
    //! Minimal-size compatibility smoke test: runs every scenario once
    //! against the configured database, asserting only that each
    //! completes - never a timing. The persistence adapter's own
    //! query/schema drift is now a compile error (its queries are
    //! `sqlx::query!` macros checked against the committed `.sqlx/`
    //! cache); this catches behavioural drift in the scenarios and drift
    //! in the bench's *own* hand-written SQL (the kind that silently
    //! broke the as-of fixture when `morpholog.audit` gained its NOT
    //! NULL `actor` column) on the next PG-backed test run, rather than
    //! the next time someone runs the scale bench by hand.
    //!
    //! Gated on `DATABASE_URL`: skips (passes) when unset, so the pure
    //! workspace stays green without a database. A single test runs
    //! the scenarios sequentially because each truncates the schema -
    //! it must not race other PG-backed tests, which is why the PG
    //! suites run under `--test-threads=1`.
    use super::*;

    fn db_url() -> Option<String> {
        std::env::var("DATABASE_URL").ok().filter(|s| !s.is_empty())
    }

    #[tokio::test]
    async fn scenarios_smoke() {
        let Some(url) = db_url() else {
            eprintln!("DATABASE_URL unset; skipping bench smoke test");
            return;
        };

        run_write(ScenarioArgs {
            n: 1,
            accounts: 2,
            noise_claims: 1,
            database_url: url.clone(),
            reset: true,
            repeat: 2,
        })
        .await
        .expect("write scenario smoke");

        run_read(ScenarioArgs {
            n: 1,
            accounts: 2,
            noise_claims: 1,
            database_url: url.clone(),
            reset: true,
            repeat: 2,
        })
        .await
        .expect("read scenario smoke");

        run_as_of(AsOfArgs {
            n: 4,
            at: 1.0,
            retract_fraction: 0,
            database_url: url.clone(),
            reset: true,
            repeat: 2,
        })
        .await
        .expect("as-of scenario smoke (asserts only)");

        // Retract-heavy: exercises the `actor` column and the retract
        // branch of the fabricator and the ReplaySet replay path.
        run_as_of(AsOfArgs {
            n: 10,
            at: 1.0,
            retract_fraction: 50,
            database_url: url.clone(),
            reset: true,
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
            repeat: 1,
        })
        .await
        .expect("contend scenario smoke (disjoint)");

        run_import(ImportArgs {
            n: 2,
            database_url: url.clone(),
            reset: true,
            repeat: 2,
        })
        .await
        .expect("import scenario smoke");

        run_wide(WideArgs {
            n: 1,
            arity: 13,
            database_url: url.clone(),
            reset: true,
            repeat: 2,
        })
        .await
        .expect("wide scenario smoke");

        // Suite plumbing over a PRIVATE tiny plan - never the public
        // quick ladder, which is a real measurement run and does not
        // belong in CI. One case per scenario family keeps the
        // dispatch, the collection, and the renderer covered.
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
        ];
        let cases = run_suite_specs(&pool, &plan, 2)
            .await
            .expect("suite smoke plan");
        assert_eq!(cases.len(), plan.len(), "every smoke case reports");
        let report = SuiteReport {
            suite_contract: SUITE_CONTRACT,
            implementation: IMPLEMENTATION,
            ladder: "smoke",
            repeat: 2,
            pg_version: "smoke".to_string(),
            debug_assertions: cfg!(debug_assertions),
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
            implementation: IMPLEMENTATION,
            ladder: "unit",
            repeat: 3,
            pg_version: "PostgreSQL test".to_string(),
            debug_assertions: false,
            cases: vec![CaseResult {
                case: "write/base".to_string(),
                implementation: IMPLEMENTATION,
                axis: "n",
                point: 100,
                metrics: vec![
                    Metric::series("propose_one", "ms", vec![5.0, 2.0, 3.0]),
                    Metric::series("scoped_claims", "count", vec![300.0]),
                ],
            }],
        };
        let rendered = render_markdown(&report);
        assert!(rendered.contains("suite_contract=1"));
        assert!(
            rendered.contains("| case | axis | point | metric | first | steady median | unit |")
        );
        // first = samples[0]; steady median over the rest of [2.0, 3.0]
        // is 3.0 (upper median).
        assert!(rendered.contains("| write/base | n | 100 | propose_one | 5.00 | 3.00 | ms |"));
        // A single-sample metric has no steady median.
        assert!(rendered.contains("| write/base | n | 100 | scoped_claims | 300.00 | - | count |"));
        assert!(!rendered.contains("benchmark-grade=false"));
    }
}
