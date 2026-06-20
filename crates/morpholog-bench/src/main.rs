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
    PgError, PgPool, PgProposalOutcome, list_claims_for_predicates, list_derived_at,
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
             Re-run with `--reset` to acknowledge. Target: {database_url}"
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
    }
}

async fn run_write(args: ScenarioArgs) -> Result<()> {
    require_reset_ack(&args)?;
    require_positive_k(&args)?;
    let pool = PgPool::connect(&args.database_url)
        .await
        .context("connect to PostgreSQL")?;
    println!(
        "scenario=write n={} accounts={} noise_claims={}",
        args.n, args.accounts, args.noise_claims
    );

    let t = Instant::now();
    reset_db(&pool).await?;
    insert_n_entries(&pool, args.n, args.accounts).await?;
    insert_noise_claims(&pool, args.noise_claims).await?;
    println!("  fixture_build:  {:>8} ms", t.elapsed().as_millis());

    let t = Instant::now();
    let transformation = double_entry_ledger::post_simple_entry();
    let transition = Transition {
        transformation_name: transformation.name.clone(),
        args: vec![
            subj("entry_bench_target"),
            subj("d_2026_05_17"),
            subj("p_bench"),
            subj("account_cash"),
            subj("account_revenue"),
            dec(42),
        ],
        actor: Subject::from("bench"),
    };
    let compiled = CompiledProgram::new(double_entry_ledger::program())
        .map_err(|e| anyhow!("invalid programme: {e:?}"))?;
    let outcome = propose_against_pg(&pool, &compiled, &transition)
        .await
        .context("propose_against_pg")?;
    println!("  propose_one:    {:>8} ms", t.elapsed().as_millis());
    println!("  outcome:        {}", outcome_summary(&outcome));

    if !matches!(outcome, PgProposalOutcome::Committed { .. }) {
        return Err(anyhow!(
            "expected the target propose to commit; bench fixture or kernel \
             behaviour has changed"
        ));
    }
    Ok(())
}

async fn run_read(args: ScenarioArgs) -> Result<()> {
    require_reset_ack(&args)?;
    require_positive_k(&args)?;
    let pool = PgPool::connect(&args.database_url)
        .await
        .context("connect to PostgreSQL")?;
    println!(
        "scenario=read n={} accounts={} noise_claims={}",
        args.n, args.accounts, args.noise_claims
    );

    let t = Instant::now();
    reset_db(&pool).await?;
    insert_n_entries(&pool, args.n, args.accounts).await?;
    insert_noise_claims(&pool, args.noise_claims).await?;
    println!("  fixture_build:  {:>8} ms", t.elapsed().as_millis());

    // The read scenario bypasses `list_derived` and runs the three
    // phases inline so each can be timed separately. The kernel
    // semantics are identical to what `list_derived` does:
    // compute the derived claim's predicate footprint, load only
    // claims of those predicates via `list_claims_for_predicates`,
    // wrap in `State`, call `enumerate_derived`. The split is
    // purely diagnostic and lets us see which layer dominates as N
    // and K grow.
    let derived = double_entry_ledger::trial_balance_row();
    let footprint: Vec<String> = predicates_referenced_by_derived(&derived, &[])
        .into_iter()
        .map(|p| p.to_string())
        .collect();

    let t = Instant::now();
    let claims = list_claims_for_predicates(&pool, &footprint)
        .await
        .context("list_claims_for_predicates")?;
    let n_claims = claims.len();
    println!(
        "  list_scoped:    {:>8} ms  ({} claims, predicates={:?})",
        t.elapsed().as_millis(),
        n_claims,
        footprint
    );

    let t = Instant::now();
    let state = State::from_claims(claims);
    println!("  build_state:    {:>8} ms", t.elapsed().as_millis());

    let t = Instant::now();
    let rows = enumerate_derived(&derived, &state, &[]).context("enumerate_derived")?;
    println!(
        "  enumerate:      {:>8} ms  ({} derived rows)",
        t.elapsed().as_millis(),
        rows.len()
    );

    // Bounds on the derived-row count given the fixture shape:
    // - n=0: no JournalLine claims, so 0 rows.
    // - n>0: each entry contributes lines on two distinct accounts.
    //   The exact count is `min(k, distinct accounts actually
    //   touched)`, which is `k` when `n` is large enough to wrap
    //   around the K-account modular cycle and a bit less when not.
    //   Assert the loose `0 < rows <= k` bound rather than pin an
    //   exact count, so the bench accepts any well-formed (n, k).
    if args.n == 0 {
        if !rows.is_empty() {
            return Err(anyhow!(
                "expected 0 derived rows for n=0, got {}",
                rows.len()
            ));
        }
        return Ok(());
    }
    if rows.is_empty() {
        return Err(anyhow!(
            "expected at least one derived row for n={} k={}, got none",
            args.n,
            args.accounts
        ));
    }
    if rows.len() > args.accounts {
        return Err(anyhow!(
            "derived rows ({}) exceeded the K-account ceiling ({}); \
             fixture distribution is broken",
            rows.len(),
            args.accounts
        ));
    }
    Ok(())
}

async fn run_as_of(args: AsOfArgs) -> Result<()> {
    require_reset_ack_as_of(&args)?;
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

    // Interleave a retract every `stride` transitions. Floored at 2 so
    // the retract at position `i` always targets the live entry
    // asserted at `i - 1` (a stride of 1 would make every transition a
    // retract, with nothing to remove). `0` disables retracts.
    let retract_stride: i64 = if args.retract_fraction == 0 {
        0
    } else {
        ((100.0 / args.retract_fraction as f64).round() as i64).max(2)
    };
    let retract_count = if retract_stride == 0 {
        0
    } else {
        args.n as i64 / retract_stride
    };

    let pool = PgPool::connect(&args.database_url)
        .await
        .context("connect to PostgreSQL")?;
    println!(
        "scenario=as-of n={} at={} retract_fraction={} (retracts={})",
        args.n, args.at, args.retract_fraction, retract_count
    );

    let t = Instant::now();
    reset_db(&pool).await?;
    fabricate_audit_rows(&pool, args.n, retract_stride).await?;
    println!("  fixture_build:  {:>8} ms", t.elapsed().as_millis());

    // Pick the target transition by causal offset. `at = 1.0` lands
    // on the last fabricated row (full replay); `at = 0.0` on the
    // first; `at = 0.5` on the middle. Clamp the resulting offset to
    // `[0, n-1]` so floating-point edges do not push us off the end.
    let offset: i64 = {
        let raw = ((args.n as f64) * args.at).floor() as i64;
        raw.clamp(0, (args.n as i64) - 1)
    };
    let (target_tid,): (Uuid,) = sqlx::query_as(
        "SELECT transition_id FROM morpholog.audit
         ORDER BY committed_at, transition_id LIMIT 1 OFFSET $1",
    )
    .bind(offset)
    .fetch_one(&pool)
    .await
    .context("resolve target transition_id")?;
    println!("  target_tid:     {target_tid}");

    let t = Instant::now();
    let state = reconstruct_state_at(&pool, target_tid)
        .await
        .context("reconstruct_state_at")?;
    println!(
        "  reconstruct:    {:>8} ms  ({} claims)",
        t.elapsed().as_millis(),
        state.len()
    );

    let t = Instant::now();
    let rows = list_derived_at(
        &pool,
        &double_entry_ledger::trial_balance_row(),
        &double_entry_ledger::definitions(),
        target_tid,
    )
    .await
    .context("list_derived_at")?;
    println!(
        "  list_derived_at:{:>8} ms  ({} derived rows)",
        t.elapsed().as_millis(),
        rows.len()
    );
    Ok(())
}

async fn run_contend(args: ContendArgs) -> Result<()> {
    check_reset_ack(args.reset, &args.database_url)?;
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
        .connect(&args.database_url)
        .await
        .context("connect to PostgreSQL")?;
    println!(
        "scenario=contend workers={} ops_per_worker={} prepopulate={} periods={} disjoint={} max_retries={}",
        args.workers,
        args.ops_per_worker,
        args.prepopulate,
        args.periods,
        args.disjoint,
        args.max_retries
    );

    let t = Instant::now();
    reset_db(&pool).await?;
    insert_n_entries(&pool, args.prepopulate, 2).await?;
    println!("  fixture_build:  {:>8} ms", t.elapsed().as_millis());

    // Fan out: every worker shares the pool and posts uniquely-keyed
    // entries concurrently, into period `w mod periods`.
    let t = Instant::now();
    let mut handles = Vec::with_capacity(args.workers);
    for w in 0..args.workers {
        let pool = pool.clone();
        let ops = args.ops_per_worker;
        let max_retries = args.max_retries;
        let periods = args.periods;
        let disjoint = args.disjoint;
        handles.push(tokio::spawn(async move {
            contend_worker(pool, w, ops, max_retries, periods, disjoint).await
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
    let elapsed = t.elapsed();

    let total_ops = (args.workers * args.ops_per_worker) as u64;
    let secs = elapsed.as_secs_f64();
    let throughput = if secs > 0.0 {
        total.committed as f64 / secs
    } else {
        0.0
    };
    let retry_rate = if total.committed > 0 {
        total.retries as f64 / total.committed as f64
    } else {
        0.0
    };
    println!("  concurrent:     {:>8} ms", elapsed.as_millis());
    println!("  total_ops:      {total_ops}");
    println!("  committed:      {}", total.committed);
    println!("  rejected:       {}", total.rejected);
    println!("  retries(40001): {}", total.retries);
    println!("  failed:         {}", total.failed);
    println!("  throughput:     {throughput:>8.1} commits/s");
    println!("  retry_rate:     {retry_rate:>8.3} retries/commit");

    // Every op terminates as exactly one of committed / rejected /
    // failed; a drift here means a worker leaked an outcome.
    let accounted = total.committed + total.rejected + total.failed;
    if accounted != total_ops {
        return Err(anyhow!(
            "accounting mismatch: committed+rejected+failed ({accounted}) != total_ops ({total_ops})"
        ));
    }
    // A contention bench that commits nothing is degenerate: either the
    // scenario is broken (every op rejected) or it is mis-parameterised
    // (every op exhausted its retries). Either way it is not a usable
    // measurement, and it is the signal the smoke test relies on.
    if total.committed == 0 {
        return Err(anyhow!(
            "contend committed nothing (rejected={} failed={} of {total_ops}); \
             scenario is broken or mis-parameterised",
            total.rejected,
            total.failed
        ));
    }
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
                args: vec![subj(&format!("item_w{worker_id}_op{op}"))],
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
                    subj(&format!("entry_w{worker_id}_op{op}")),
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
        match propose_against_pg(pool, compiled, transition).await {
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
            committed_at
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
            now() + (i * interval '1 microsecond')
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
    sqlx::query("TRUNCATE morpholog.outbox, morpholog.claims, morpholog.audit, morpholog.rejections CASCADE")
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
        PgProposalOutcome::Rejected { reason } => format!("Rejected: {reason}"),
    }
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
        })
        .await
        .expect("write scenario smoke");

        run_read(ScenarioArgs {
            n: 1,
            accounts: 2,
            noise_claims: 1,
            database_url: url.clone(),
            reset: true,
        })
        .await
        .expect("read scenario smoke");

        run_as_of(AsOfArgs {
            n: 4,
            at: 1.0,
            retract_fraction: 0,
            database_url: url.clone(),
            reset: true,
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
            database_url: url,
            reset: true,
        })
        .await
        .expect("contend scenario smoke (disjoint)");
    }
}
