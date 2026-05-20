//! Morpholog scale-pressure benchmark.
//!
//! Synthetic benchmark for understanding how the runtime behaves as
//! state grows. Three scenarios:
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
//!   Measures audit-log replay cost as a function of N and the
//!   `--at <fraction>` axis.
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
    EvalValue, State, Transition, enumerate_derived, examples::double_entry_ledger,
    predicates_referenced_by_derived,
};
use morpholog_postgres::{
    PgPool, PgProposalOutcome, list_claims_for_predicates, list_derived_at, propose_against_pg,
    reconstruct_state_at,
};
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

    /// Fabricate N audit transitions, then time one
    /// `reconstruct_state_at` and one `list_derived_at` against a
    /// target transition. Measures audit-log replay cost as a
    /// function of N (number of transitions to walk through) and
    /// `--at <fraction>` (how far through the log the target sits).
    AsOf(AsOfArgs),
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
             Re-run with `--reset` to acknowledge. Target: {}",
            database_url
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
        actor: EvalValue::Subject("bench".to_string()),
    };
    let outcome = propose_against_pg(
        &pool,
        &transformation,
        &transition,
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
    // semantics are identical to what `list_derived` does:
    // compute the derived claim's predicate footprint, load only
    // claims of those predicates via `list_claims_for_predicates`,
    // wrap in `State`, call `enumerate_derived`. The split is
    // purely diagnostic and lets us see which layer dominates as N
    // and K grow.
    let derived = double_entry_ledger::trial_balance_row();
    let footprint: Vec<String> = predicates_referenced_by_derived(&derived)
        .into_iter()
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
    let rows = enumerate_derived(&derived, &state).context("enumerate_derived")?;
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

    let pool = PgPool::connect(&args.database_url)
        .await
        .context("connect to PostgreSQL")?;
    println!("scenario=as-of n={} at={}", args.n, args.at);

    let t = Instant::now();
    reset_db(&pool).await?;
    fabricate_audit_rows(&pool, args.n).await?;
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
    let rows = list_derived_at(&pool, &double_entry_ledger::trial_balance_row(), target_tid)
        .await
        .context("list_derived_at")?;
    println!(
        "  list_derived_at:{:>8} ms  ({} derived rows)",
        t.elapsed().as_millis(),
        rows.len()
    );
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
/// **`retracted_claims` is always `[]`.** This bench measures replay
/// in the asserts-only regime. A future scenario could exercise the
/// retraction branch by interleaving fabricated transitions that
/// retract earlier claims, but the integration test
/// `reconstruct_state_at_applies_cross_transition_retractions`
/// already pins that branch's correctness; bench coverage of
/// retraction cost is a future enhancement if forced.
async fn fabricate_audit_rows(pool: &PgPool, n: usize) -> Result<()> {
    let n_i: i64 = n
        .try_into()
        .map_err(|_| anyhow!("n={n} too large for i64"))?;

    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents,
            committed_at
        )
        SELECT
            gen_random_uuid(),
            'bench_as_of_post',
            '[]'::jsonb,
            1,
            '[]'::jsonb,
            jsonb_build_array(
                jsonb_build_object(
                    'predicate', 'JournalEntry',
                    'args', jsonb_build_array(
                        jsonb_build_object('type','subject','value','bench_entry_' || i),
                        jsonb_build_object('type','subject','value','d_2026'),
                        jsonb_build_object('type','subject','value','p_bench')
                    )
                ),
                jsonb_build_object(
                    'predicate', 'JournalLine',
                    'args', jsonb_build_array(
                        jsonb_build_object('type','subject','value','bench_entry_' || i),
                        jsonb_build_object('type','subject','value','account_cash'),
                        jsonb_build_object('type','decimal','value','100'),
                        jsonb_build_object('type','decimal','value','0')
                    )
                ),
                jsonb_build_object(
                    'predicate', 'JournalLine',
                    'args', jsonb_build_array(
                        jsonb_build_object('type','subject','value','bench_entry_' || i),
                        jsonb_build_object('type','subject','value','account_revenue'),
                        jsonb_build_object('type','decimal','value','0'),
                        jsonb_build_object('type','decimal','value','100')
                    )
                )
            ),
            '[]'::jsonb,
            '[]'::jsonb,
            now() + (i * interval '1 microsecond')
        FROM generate_series(1, $1) AS i",
    )
    .bind(n_i)
    .execute(pool)
    .await
    .context("fabricate audit rows")?;

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
