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
use morpholog_core::{EvalValue, examples::double_entry_ledger};
use morpholog_postgres::{PgPool, PgProposalOutcome, list_derived, propose_against_pg};
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
    let pool = PgPool::connect(&args.database_url)
        .await
        .context("connect to PostgreSQL")?;
    println!("scenario=write n={}", args.n);

    let t = Instant::now();
    reset_db(&pool).await?;
    insert_n_entries(&pool, args.n).await?;
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
    let pool = PgPool::connect(&args.database_url)
        .await
        .context("connect to PostgreSQL")?;
    println!("scenario=read n={}", args.n);

    let t = Instant::now();
    reset_db(&pool).await?;
    insert_n_entries(&pool, args.n).await?;
    println!("  fixture_build:  {:>8} ms", t.elapsed().as_millis());

    let t = Instant::now();
    let rows = list_derived(&pool, &double_entry_ledger::trial_balance_row())
        .await
        .context("list_derived")?;
    println!("  list_derived:   {:>8} ms", t.elapsed().as_millis());
    println!("  derived_rows:   {}", rows.len());

    // n=0 is a legitimate baseline (measures load_state + enumerate
    // cost against an empty state); the fixture inserts no
    // JournalLine claims so the domain enumerates to zero key
    // bindings and the derived extension is empty. For n>0 every
    // entry hits the same two accounts (cash, revenue), so the
    // expected row count is exactly two regardless of n.
    let expected = if args.n == 0 { 0 } else { 2 };
    if rows.len() != expected {
        return Err(anyhow!(
            "expected {expected} derived row(s) for n={}, got {}",
            args.n,
            rows.len()
        ));
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
/// All entries debit `account_cash` for 100 and credit `account_revenue`
/// for 100. Period is `p_bench`. No `PeriodClosed` claim is inserted, so
/// the require in `post_simple_entry` passes.
///
/// `asserted_in` uses `Uuid::nil()` as a synthetic fixture transition id
/// (the same pattern the integration-test fixtures use). This row does
/// not appear in the `audit` table; the schema does not enforce
/// referential integrity from `claims.asserted_in` to `audit.transition_id`.
async fn insert_n_entries(pool: &PgPool, n: usize) -> Result<()> {
    let n_i: i64 = n
        .try_into()
        .map_err(|_| anyhow!("n={n} too large for i64"))?;
    let fixture_id = Uuid::nil();

    // JournalEntry(entry_id, posting_date, period)
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

    // JournalLine(entry_id, account_cash, 100, 0)  - debit side
    sqlx::query(
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         SELECT 'JournalLine',
                jsonb_build_array(
                    jsonb_build_object('type','subject','value','entry_bench_' || i),
                    jsonb_build_object('type','subject','value','account_cash'),
                    jsonb_build_object('type','decimal','value','100'),
                    jsonb_build_object('type','decimal','value','0')
                ),
                $1
         FROM generate_series(1, $2) AS i",
    )
    .bind(fixture_id)
    .bind(n_i)
    .execute(pool)
    .await
    .context("insert JournalLine debit-side fixture rows")?;

    // JournalLine(entry_id, account_revenue, 0, 100)  - credit side
    sqlx::query(
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         SELECT 'JournalLine',
                jsonb_build_array(
                    jsonb_build_object('type','subject','value','entry_bench_' || i),
                    jsonb_build_object('type','subject','value','account_revenue'),
                    jsonb_build_object('type','decimal','value','0'),
                    jsonb_build_object('type','decimal','value','100')
                ),
                $1
         FROM generate_series(1, $2) AS i",
    )
    .bind(fixture_id)
    .bind(n_i)
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
