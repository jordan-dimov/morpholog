//! Morpholog scale-pressure benchmark.
//!
//! Synthetic benchmark for understanding how the runtime behaves as
//! state grows. Two scenarios that share the same fixture builder:
//!
//! - `write` populates the claims table with N pre-existing journal
//!   entries via direct SQL, then times one `propose_against_pg`
//!   call. Measures load_state + invariant evaluation + commit as a
//!   function of pre-state size.
//! - `read` uses the same fixture and times one `list_derived` call
//!   against the trial-balance derived claim. Measures load_state +
//!   `enumerate_derived` as a function of pre-state size.
//!
//! The fixture is deliberately uniform (every entry debits the same
//! cash account and credits the same revenue account for the same
//! amount). Trial balance produces exactly two rows regardless of N.
//! That keeps the read-path dominant cost in the load + sum sweeps
//! rather than in `enumerate_derived`'s grouping. A future scenario
//! that distributes lines across many accounts would stress grouping.
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
use morpholog_core::{EvalValue, State, enumerate_derived, examples::double_entry_ledger};
use morpholog_postgres::{PgPool, PgProposalOutcome, list_claims, propose_against_pg};
use rust_decimal::Decimal;
use std::time::Instant;
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
    /// preserves the original K=2 baseline (entries alternate cash
    /// and revenue) so older numbers remain comparable.
    ///
    /// The trial-balance derived claim produces one row per distinct
    /// account, so K is the number of derived rows expected on the
    /// read scenario. Larger K stresses `enumerate_derived`'s grouping
    /// and the per-account `Sum` lookups.
    #[arg(long, default_value_t = 2)]
    accounts: usize,

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

/// Refuses to proceed unless `--reset` was explicitly passed. The
/// target URL is echoed so the operator can see what is about to be
/// truncated; this is the closest the binary gets to a "are you sure"
/// dialog without making scripted use awkward.
fn require_reset_ack(args: &ScenarioArgs) -> Result<()> {
    if !args.reset {
        return Err(anyhow!(
            "this benchmark TRUNCATES the morpholog schema in the target database. \
             Re-run with `--reset` to acknowledge. Target: {}",
            args.database_url
        ));
    }
    Ok(())
}

/// `--accounts 0` would break the fixture's modulo-K distribution
/// (`i % 0` is undefined in PostgreSQL and meaningless conceptually -
/// there must be at least one account for any journal line to land on).
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
    }
}

async fn run_write(args: ScenarioArgs) -> Result<()> {
    require_reset_ack(&args)?;
    require_positive_k(&args)?;
    let pool = PgPool::connect(&args.database_url)
        .await
        .context("connect to PostgreSQL")?;
    println!("scenario=write n={} accounts={}", args.n, args.accounts);

    let t = Instant::now();
    reset_db(&pool).await?;
    insert_n_entries(&pool, args.n, args.accounts).await?;
    println!("  fixture_build:  {:>8} ms", t.elapsed().as_millis());

    let t = Instant::now();
    let outcome = propose_against_pg(
        &pool,
        &double_entry_ledger::post_simple_entry(),
        vec![
            subj("entry_bench_target"),
            subj("d_2026_05_17"),
            subj("p_bench"),
            subj("account_cash"),
            subj("account_revenue"),
            dec(42),
        ],
        &double_entry_ledger::all_invariants(),
    )
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
    println!("scenario=read n={} accounts={}", args.n, args.accounts);

    let t = Instant::now();
    reset_db(&pool).await?;
    insert_n_entries(&pool, args.n, args.accounts).await?;
    println!("  fixture_build:  {:>8} ms", t.elapsed().as_millis());

    // The read scenario bypasses `list_derived` and runs the three
    // phases inline so each can be timed separately. The kernel
    // semantics are identical to what `list_derived` does (load all
    // current claims via the PG read helper, wrap in `State`, call
    // `enumerate_derived`); the split is purely diagnostic and lets
    // us see which layer dominates as N and K grow.
    let t = Instant::now();
    let claims = list_claims(&pool).await.context("list_claims")?;
    let n_claims = claims.len();
    println!(
        "  list_claims:    {:>8} ms  ({} claims)",
        t.elapsed().as_millis(),
        n_claims
    );

    let t = Instant::now();
    let state = State::from_claims(claims);
    println!("  build_state:    {:>8} ms", t.elapsed().as_millis());

    let t = Instant::now();
    let rows = enumerate_derived(&double_entry_ledger::trial_balance_row(), &state)
        .context("enumerate_derived")?;
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
    } else {
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
    }
    Ok(())
}

async fn reset_db(pool: &PgPool) -> Result<()> {
    sqlx::query("TRUNCATE morpholog.outbox, morpholog.claims, morpholog.audit CASCADE")
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

fn subj(s: &str) -> EvalValue {
    EvalValue::Subject(s.to_string())
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
