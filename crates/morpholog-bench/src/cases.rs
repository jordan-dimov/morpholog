//! Independent cases on one record (#906).
//!
//! Each case is a contract with its own book. Case-local admission
//! checks an order only against its own contract, so a rule never makes
//! two contracts' orders conflict. The question is what PostgreSQL's
//! physical locking costs them anyway: the committed rate when the cases
//! share one predicate, when each has its own predicate, and when each
//! has its own database, which is the physical-independence control.
//!
//! Every point ends with a reconciliation: each submitted order's
//! outcome against the history and the final book. A point that loses or
//! repeats an order reports no numbers.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use morpholog_core::{EvalValue, Program, Subject, TransformationName, Transition};
use morpholog_postgres::{
    PgAtomicOutcome, PgError, PgPool, PgProgram, PgProposalOutcome, Proposal, VerifyOutcome,
    list_audit_rows, list_claims_for_predicates, propose_against_pg, propose_all_against_pg,
    verify_replay,
};
use rust_decimal::Decimal;
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

use crate::{
    Implementation, OutputFormat, analyze_claims, check_reset_ack, establish,
    require_migration_head, reset_db,
};

#[derive(clap::Args, Debug)]
pub(crate) struct CasesArgs {
    /// The case counts to sweep.
    #[arg(long, value_delimiter = ',', default_value = "1,2,4,8,16")]
    cases: Vec<usize>,

    /// A fixed offered load: this many workers in all, divided evenly
    /// among the cases, so only the cases' independence varies.
    #[arg(long, conflicts_with = "workers_per_case")]
    workers: Option<usize>,

    /// Scaling: this many workers on each case (the default, 1).
    #[arg(long)]
    workers_per_case: Option<usize>,

    /// The layouts to run at each point, in this order.
    #[arg(
        long,
        value_enum,
        value_delimiter = ',',
        default_value = "shared,databases"
    )]
    layout: Vec<Layout>,

    /// Resting orders on each case's book before the run.
    #[arg(long, default_value_t = 300)]
    book: usize,

    /// How the orders' prices are spread.
    #[arg(long, value_enum, default_value = "distinct")]
    prices: Prices,

    /// How long each point runs.
    #[arg(long, default_value_t = 20)]
    seconds: u64,

    /// Retries of a serialization failure before an order is abandoned.
    #[arg(long, default_value_t = 100)]
    max_retries: usize,

    #[arg(long, value_enum, default_value = "compiled-indexed")]
    implementation: Implementation,

    /// The main database. The `databases` layout also creates
    /// `<name>_case_<i>` on the same server and drops them at the end.
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// Acknowledge that every database the run uses is truncated.
    #[arg(long)]
    reset: bool,

    #[arg(long, value_enum, default_value = "markdown")]
    format: OutputFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Layout {
    /// One `Resting` predicate for every case: today's shape.
    Shared,
    /// One predicate per case. Diagnostic only: it changes the
    /// programme, and the cases still share the claims table and the log.
    Apart,
    /// One database per case: the physical-independence control.
    Databases,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Prices {
    /// Every order at its own price.
    Distinct,
    /// Every bid at one price, every ask at another.
    Uniform,
    /// Nine orders in ten at the uniform prices, the rest distinct.
    Skewed,
}

fn book_programme(suffix: &str) -> Result<Program> {
    let source = format!(
        "program book{suffix}

predicate Contract{suffix}(contract: Subject)
predicate Resting{suffix}(order: Subject, contract: Subject, side: Subject, price: Decimal, qty: Decimal)
    unique by (order)

invariant never_crossed:
    Resting{suffix}(_, c, #buy, bid, _) and Resting{suffix}(_, c, #sell, ask, _) implies bid < ask

transformation list_contract(contract):
    admit Contract{suffix}(contract)

transformation rest(order, contract, side, price: Decimal, qty: Decimal):
    require listed: Contract{suffix}(contract)
    admit Resting{suffix}(order, contract, side, price, qty)
"
    );
    morpholog_surface::parse_program(&source).map_err(|e| anyhow!("the book does not parse: {e:?}"))
}

/// Even positions are bids below 100, odd ones asks above it, so no
/// order can cross the book whatever runs alongside it.
fn order(n: usize, prices: Prices) -> (Subject, Decimal) {
    let buy = n.is_multiple_of(2);
    let spread = match prices {
        Prices::Distinct => true,
        Prices::Uniform => false,
        Prices::Skewed => n.is_multiple_of(10),
    };
    let step = if spread {
        Decimal::new(i64::try_from(n).unwrap_or(i64::MAX) % 900_000 + 1, 6)
    } else {
        Decimal::ZERO
    };
    if buy {
        (Subject::from("buy"), Decimal::from(99) - step)
    } else {
        (Subject::from("sell"), Decimal::from(101) + step)
    }
}

fn order_id(case: usize, n: usize) -> String {
    format!("c{case}_o{n}")
}

fn contract(case: usize) -> String {
    format!("c{case}")
}

fn rest(case: usize, n: usize, prices: Prices) -> Proposal {
    let (side, price) = order(n, prices);
    Proposal::gateway(&Transition {
        transformation_name: TransformationName::from("rest"),
        args: vec![
            EvalValue::Subject(Subject::from(order_id(case, n).as_str())),
            EvalValue::Subject(Subject::from(contract(case).as_str())),
            EvalValue::Subject(side),
            EvalValue::Decimal(price),
            EvalValue::Decimal(Decimal::ONE),
        ],
        actor: Subject::from("bench"),
        decided_at: None,
    })
}

/// Where one or more cases live: a database and the programme their
/// orders go through.
struct Target {
    pool: PgPool,
    program: PgProgram,
    core: Program,
    resting: String,
    cases: Vec<usize>,
}

#[derive(Default)]
struct Tally {
    committed: Vec<String>,
    exhausted: Vec<String>,
    /// Orders whose act failed because the server ran out of a resource,
    /// as an embedder would see it, with the first such message.
    failed: Vec<String>,
    first_error: Option<String>,
    latencies: Vec<Duration>,
    retries: u64,
    wasted: Duration,
}

#[derive(Serialize)]
struct Point {
    layout: Layout,
    cases: usize,
    workers_per_case: usize,
    workers: usize,
    seconds: f64,
    committed: usize,
    exhausted: usize,
    failed: usize,
    first_error: Option<String>,
    retries: u64,
    retries_per_commit: f64,
    rate: f64,
    rate_per_case: f64,
    p50_ms: f64,
    p95_ms: f64,
    p99_ms: f64,
    wasted_share: f64,
    /// This layout's rate over the `databases` rate at the same point.
    independence_efficiency: Option<f64>,
    programme_hashes: Vec<String>,
}

#[derive(Serialize)]
struct Identity {
    binary_sha256: String,
    started_at: String,
    server_version: String,
    lock_settings: BTreeMap<String, String>,
    load_average: Option<String>,
    governor: Option<String>,
    implementation: &'static str,
    book: usize,
    prices: Prices,
    seconds: u64,
    max_retries: usize,
    debug_assertions: bool,
}

#[derive(Serialize)]
struct Report {
    identity: Identity,
    points: Vec<Point>,
}

pub(crate) async fn run_cases(args: CasesArgs) -> Result<()> {
    check_reset_ack(args.reset, &args.database_url)?;
    if args.cases.is_empty() || args.cases.contains(&0) || args.layout.is_empty() {
        bail!("--cases needs counts above zero, and --layout at least one layout");
    }
    if let Some(w) = args.workers
        && let Some(c) = args.cases.iter().find(|c| w % **c != 0)
    {
        bail!("--workers {w} does not divide evenly over {c} cases");
    }
    let options = PgConnectOptions::from_str(&morpholog_postgres::with_connection_defaults(
        &args.database_url,
    ))
    .context("parse the database URL")?;
    let main = connect(&options, 4).await?;
    require_migration_head(&main).await?;
    let identity = identity(&main, &args).await?;

    let mut points = Vec::new();
    for &cases in &args.cases {
        let per_case = args
            .workers
            .map_or(args.workers_per_case.unwrap_or(1), |w| w / cases);
        for &layout in &args.layout {
            let point = run_point(&args, &options, &main, layout, cases, per_case).await?;
            points.push(point);
        }
    }
    for i in 0..points.len() {
        let control = points
            .iter()
            .find(|p| {
                p.layout == Layout::Databases
                    && p.cases == points[i].cases
                    && p.workers_per_case == points[i].workers_per_case
            })
            .map(|p| p.rate);
        points[i].independence_efficiency =
            control.filter(|r| *r > 0.0).map(|r| points[i].rate / r);
    }
    if args.layout.contains(&Layout::Databases) {
        drop_case_databases(
            &main,
            &options,
            args.cases.iter().max().copied().unwrap_or(0),
        )
        .await?;
    }

    let report = Report { identity, points };
    match args.format {
        OutputFormat::Markdown => print!("{}", render(&report)),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&report).context("serialise the report")?
        ),
    }
    Ok(())
}

async fn connect(options: &PgConnectOptions, connections: usize) -> Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(u32::try_from(connections.max(1)).unwrap_or(u32::MAX))
        .connect_with(options.clone())
        .await
        .with_context(|| {
            format!(
                "connect to {}",
                options.get_database().unwrap_or("the default database")
            )
        })
}

fn case_database(options: &PgConnectOptions, case: usize) -> Result<String> {
    let base = options
        .get_database()
        .ok_or_else(|| anyhow!("the database URL names no database"))?;
    Ok(format!("{base}_case_{case}"))
}

/// Creates the case's database if it is missing and brings it to the
/// schema this binary expects.
async fn case_pool(
    main: &PgPool,
    options: &PgConnectOptions,
    case: usize,
    connections: usize,
) -> Result<PgPool> {
    let name = case_database(options, case)?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(&name)
            .fetch_one(main)
            .await
            .context("look up a case database")?;
    if !exists {
        // Safe: the name is quoted, and built from the main database's.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE DATABASE {}",
            quote_ident(&name)
        )))
        .execute(main)
        .await
        .with_context(|| format!("create {name}"))?;
    }
    let pool = connect(&options.clone().database(&name), connections).await?;
    morpholog_postgres::initialise_schema(&pool)
        .await
        .with_context(|| format!("initialise {name}"))?;
    Ok(pool)
}

async fn drop_case_databases(
    main: &PgPool,
    options: &PgConnectOptions,
    cases: usize,
) -> Result<()> {
    for case in 0..cases {
        let name = case_database(options, case)?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            quote_ident(&name)
        )))
        .execute(main)
        .await
        .with_context(|| format!("drop {name}"))?;
    }
    Ok(())
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

async fn targets(
    args: &CasesArgs,
    options: &PgConnectOptions,
    main: &PgPool,
    layout: Layout,
    cases: usize,
    per_case: usize,
) -> Result<Vec<Target>> {
    let target = |pool: PgPool, suffix: &str, cases: Vec<usize>| -> Result<Target> {
        let core = book_programme(suffix)?;
        Ok(Target {
            pool,
            program: args.implementation.program(core.clone())?,
            core,
            resting: format!("Resting{suffix}"),
            cases,
        })
    };
    Ok(match layout {
        Layout::Shared => vec![target(main.clone(), "", (0..cases).collect())?],
        Layout::Apart => (0..cases)
            .map(|c| target(main.clone(), &format!("_{c}"), vec![c]))
            .collect::<Result<_>>()?,
        Layout::Databases => {
            let mut out = Vec::with_capacity(cases);
            for c in 0..cases {
                let pool = case_pool(main, options, c, per_case + 1).await?;
                out.push(target(pool, "", vec![c])?);
            }
            out
        }
    })
}

async fn run_point(
    args: &CasesArgs,
    options: &PgConnectOptions,
    main: &PgPool,
    layout: Layout,
    cases: usize,
    per_case: usize,
) -> Result<Point> {
    let workers = cases * per_case;
    // Shared and apart workers draw on the main pool.
    let main = if layout == Layout::Databases {
        main.clone()
    } else {
        connect(&main.connect_options(), workers + 2).await?
    };
    let targets = targets(args, options, &main, layout, cases, per_case).await?;

    // The fixture, outside the timing: each database emptied and its
    // indexes established, then each case listed and its book rested in
    // one decision, so the history holds every order the book does.
    for group in by_database(targets.iter()) {
        let pool = &group[0].pool;
        reset_db(pool).await?;
        let cores: Vec<Program> = group.iter().map(|t| t.core.clone()).collect();
        establish(pool, args.implementation, &cores).await?;
    }
    for t in &targets {
        for &case in &t.cases {
            let mut proposals = vec![Proposal::gateway(&Transition {
                transformation_name: TransformationName::from("list_contract"),
                args: vec![EvalValue::Subject(Subject::from(contract(case).as_str()))],
                actor: Subject::from("bench"),
                decided_at: None,
            })];
            proposals.extend((0..args.book).map(|n| rest(case, n, args.prices)));
            match propose_all_against_pg(&t.pool, &t.program, &proposals).await {
                Ok(PgAtomicOutcome::Committed { .. }) => {}
                Ok(PgAtomicOutcome::Rejected { act, reason, .. }) => {
                    bail!("case {case}'s book was refused at act {act}: {reason}")
                }
                Err(e) => return Err(anyhow::Error::new(e).context("rest a case's book")),
            }
        }
    }
    for group in by_database(targets.iter()) {
        analyze_claims(&group[0].pool).await?;
    }

    let targets: Vec<Arc<Target>> = targets.into_iter().map(Arc::new).collect();
    let started = Instant::now();
    let load = Load {
        per_case,
        book: args.book,
        prices: args.prices,
        max_retries: args.max_retries,
        deadline: started + Duration::from_secs(args.seconds),
    };
    let mut handles = Vec::with_capacity(workers);
    for t in &targets {
        for &case in &t.cases {
            for w in 0..per_case {
                let t = Arc::clone(t);
                handles.push(tokio::spawn(async move { worker(&t, case, w, load).await }));
            }
        }
    }
    let mut tally = Tally::default();
    for h in handles {
        let t = h.await.context("join a worker")??;
        tally.committed.extend(t.committed);
        tally.exhausted.extend(t.exhausted);
        tally.failed.extend(t.failed);
        if tally.first_error.is_none() {
            tally.first_error = t.first_error;
        }
        tally.latencies.extend(t.latencies);
        tally.retries += t.retries;
        tally.wasted += t.wasted;
    }
    let elapsed = started.elapsed();

    for group in by_database(targets.iter().map(|t| &**t)) {
        reconcile(&group, args.book, &tally).await?;
    }
    for t in &targets {
        t.pool.close().await;
    }

    let mut latencies = tally.latencies.clone();
    latencies.sort();
    let committed = tally.committed.len();
    let seconds = elapsed.as_secs_f64();
    let rate = committed as f64 / seconds;
    let busy = seconds * workers as f64;
    Ok(Point {
        layout,
        cases,
        workers_per_case: per_case,
        workers,
        seconds,
        committed,
        exhausted: tally.exhausted.len(),
        failed: tally.failed.len(),
        first_error: tally.first_error.clone(),
        retries: tally.retries,
        retries_per_commit: tally.retries as f64 / committed.max(1) as f64,
        rate,
        rate_per_case: rate / cases as f64,
        p50_ms: percentile_ms(&latencies, 50),
        p95_ms: percentile_ms(&latencies, 95),
        p99_ms: percentile_ms(&latencies, 99),
        wasted_share: tally.wasted.as_secs_f64() / busy,
        independence_efficiency: None,
        programme_hashes: targets
            .iter()
            .map(|t| morpholog_core::format::canonical_hash(&t.core))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    })
}

/// The targets grouped by the database they live in, in first-seen
/// order.
fn by_database<'a>(targets: impl IntoIterator<Item = &'a Target>) -> Vec<Vec<&'a Target>> {
    let mut groups: Vec<Vec<&Target>> = Vec::new();
    for t in targets {
        let name = t.pool.connect_options().get_database().map(str::to_string);
        match groups.iter_mut().find(|g| {
            g[0].pool
                .connect_options()
                .get_database()
                .map(str::to_string)
                == name
        }) {
            Some(group) => group.push(t),
            None => groups.push(vec![t]),
        }
    }
    groups
}

/// What every worker at a point shares.
#[derive(Clone, Copy)]
struct Load {
    per_case: usize,
    book: usize,
    prices: Prices,
    max_retries: usize,
    deadline: Instant,
}

async fn worker(target: &Target, case: usize, w: usize, load: Load) -> Result<Tally> {
    let Load {
        per_case,
        book,
        prices,
        max_retries,
        deadline,
    } = load;
    let mut tally = Tally::default();
    let mut op = 0usize;
    while Instant::now() < deadline {
        let n = book + op * per_case + w;
        op += 1;
        let proposal = rest(case, n, prices);
        let started = Instant::now();
        let mut attempt: u64 = 0;
        loop {
            let tried = Instant::now();
            match propose_against_pg(&target.pool, &target.program, &proposal).await {
                Ok(PgProposalOutcome::Committed { .. }) => {
                    tally.latencies.push(started.elapsed());
                    tally.committed.push(order_id(case, n));
                    break;
                }
                Ok(PgProposalOutcome::Rejected { reason, .. }) => {
                    bail!("order {} was refused: {reason}", order_id(case, n));
                }
                Err(PgError::SerializationFailure | PgError::DeadlockDetected) => {
                    tally.retries += 1;
                    tally.wasted += tried.elapsed();
                    attempt += 1;
                    if attempt as usize > max_retries {
                        tally.exhausted.push(order_id(case, n));
                        break;
                    }
                    // The contend scenario's backoff: linear, no jitter.
                    tokio::time::sleep(Duration::from_micros(100 * attempt)).await;
                }
                Err(PgError::Database(e)) if out_of_resources(&e) => {
                    tally.wasted += tried.elapsed();
                    tally.failed.push(order_id(case, n));
                    tally.first_error.get_or_insert_with(|| e.to_string());
                    break;
                }
                Err(e) => return Err(anyhow::Error::new(e).context("rest an order")),
            }
        }
    }
    Ok(tally)
}

/// SQLSTATE class 53: the server ran out of memory, disk or
/// connections. Under serializable isolation that includes a predicate
/// lock table too small for the transactions in flight.
fn out_of_resources(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(sqlx::error::DatabaseError::code)
        .is_some_and(|code| code.starts_with("53"))
}

/// Every order submitted to one database against what it holds: on
/// the book and in the history exactly once if committed, nowhere if
/// abandoned, and the history replaying to the claims.
async fn reconcile(group: &[&Target], book: usize, tally: &Tally) -> Result<()> {
    let pool = &group[0].pool;
    let order_of = |args: &[EvalValue]| match args.first() {
        Some(EvalValue::Subject(s)) => Ok(s.to_string()),
        other => Err(anyhow!("a resting order without a subject id: {other:?}")),
    };
    let mut asserted: BTreeMap<(String, String), usize> = BTreeMap::new();
    for row in list_audit_rows(pool).await.context("read the history")? {
        for claim in &row.asserted_claims {
            let predicate = claim.predicate.as_str();
            if group.iter().any(|t| t.resting == predicate) {
                *asserted
                    .entry((predicate.to_string(), order_of(&claim.args)?))
                    .or_default() += 1;
            }
        }
    }
    for t in group {
        let ours = |id: &&String| t.cases.iter().any(|c| id.starts_with(&format!("c{c}_o")));
        let mut expected: BTreeSet<String> = t
            .cases
            .iter()
            .flat_map(|&c| (0..book).map(move |n| order_id(c, n)))
            .collect();
        expected.extend(tally.committed.iter().filter(ours).cloned());

        let claims = list_claims_for_predicates(pool, std::slice::from_ref(&t.resting))
            .await
            .context("read the book")?;
        let mut held = BTreeSet::new();
        for claim in &claims {
            let id = order_of(&claim.args)?;
            if t.cases.iter().any(|c| id.starts_with(&format!("c{c}_o")))
                && !held.insert(id.clone())
            {
                bail!("order {id} is on the book twice");
            }
        }
        if held != expected {
            bail!(
                "the book is not the committed orders: {} missing (first {:?}), {} unexpected (first {:?})",
                expected.difference(&held).count(),
                expected.difference(&held).take(5).collect::<Vec<_>>(),
                held.difference(&expected).count(),
                held.difference(&expected).take(5).collect::<Vec<_>>()
            );
        }
        for id in &expected {
            match asserted.get(&(t.resting.clone(), id.clone())) {
                Some(1) => {}
                Some(n) => bail!("order {id} is asserted {n} times in the history"),
                None => bail!("order {id} is on the book but not in the history"),
            }
        }
        if let Some(id) = tally
            .exhausted
            .iter()
            .chain(&tally.failed)
            .filter(ours)
            .find(|id| asserted.contains_key(&(t.resting.clone(), (*id).clone())))
        {
            bail!("order {id} was abandoned or failed but the history asserts it");
        }
    }
    match verify_replay(pool, &mut |_| {})
        .await
        .context("replay the history")?
    {
        VerifyOutcome::Consistent { .. } => Ok(()),
        other => Err(anyhow!(
            "the history does not replay to the claims: {other:?}"
        )),
    }
}

fn percentile_ms(sorted: &[Duration], p: usize) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let i = (sorted.len() * p).div_ceil(100).saturating_sub(1);
    sorted[i.min(sorted.len() - 1)].as_secs_f64() * 1000.0
}

async fn identity(pool: &PgPool, args: &CasesArgs) -> Result<Identity> {
    let binary = std::fs::read(std::env::current_exe().context("find this binary")?)
        .context("read this binary")?;
    let server_version: String = sqlx::query_scalar("SELECT version()")
        .fetch_one(pool)
        .await
        .context("SELECT version()")?;
    let mut lock_settings = BTreeMap::new();
    for name in [
        "max_pred_locks_per_transaction",
        "max_pred_locks_per_relation",
        "max_pred_locks_per_page",
        "max_connections",
    ] {
        let value: String = sqlx::query_scalar("SELECT setting FROM pg_settings WHERE name = $1")
            .bind(name)
            .fetch_one(pool)
            .await
            .with_context(|| format!("read {name}"))?;
        lock_settings.insert(name.to_string(), value);
    }
    let started_at: String = sqlx::query_scalar("SELECT now()::text")
        .fetch_one(pool)
        .await
        .context("SELECT now()")?;
    let read = |path: &str| {
        std::fs::read_to_string(path)
            .ok()
            .map(|s| s.trim().to_string())
    };
    Ok(Identity {
        binary_sha256: Sha256::digest(&binary)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        started_at,
        server_version,
        lock_settings,
        load_average: read("/proc/loadavg"),
        governor: read("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
        implementation: args.implementation.label(),
        book: args.book,
        prices: args.prices,
        seconds: args.seconds,
        max_retries: args.max_retries,
        debug_assertions: cfg!(debug_assertions),
    })
}

fn render(report: &Report) -> String {
    let id = &report.identity;
    let mut out = format!(
        "{}\nbinary sha256 {}, started {}, {} implementation, book {}, prices {:?}, {} s a point, load {}, governor {}\n\n",
        id.server_version,
        id.binary_sha256,
        id.started_at,
        id.implementation,
        id.book,
        id.prices,
        id.seconds,
        id.load_average.as_deref().unwrap_or("unknown"),
        id.governor.as_deref().unwrap_or("unknown"),
    );
    out.push_str(
        "| Layout | Cases | Workers/case | Committed/s | Per case | Efficiency | Retries/commit | Exhausted | Failed | p50 ms | p95 ms | p99 ms | Wasted |\n\
         |---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n",
    );
    for p in &report.points {
        out.push_str(&format!(
            "| {:?} | {} | {} | {:.1} | {:.1} | {} | {:.2} | {} | {} | {:.1} | {:.1} | {:.1} | {:.0}% |\n",
            p.layout,
            p.cases,
            p.workers_per_case,
            p.rate,
            p.rate_per_case,
            p.independence_efficiency
                .map_or_else(|| "-".to_string(), |e| format!("{:.0}%", e * 100.0)),
            p.retries_per_commit,
            p.exhausted,
            p.failed,
            p.p50_ms,
            p.p95_ms,
            p.p99_ms,
            p.wasted_share * 100.0,
        ));
    }
    for p in report.points.iter().filter(|p| p.first_error.is_some()) {
        out.push_str(&format!(
            "\n{:?} at {} cases, {} a case: {} acts failed, first with: {}\n",
            p.layout,
            p.cases,
            p.workers_per_case,
            p.failed,
            p.first_error.as_deref().unwrap_or_default()
        ));
    }
    out
}

/// The scenario at its smallest, for the bench smoke: every layout once,
/// the reconciliation included. No timing is asserted.
#[cfg(test)]
pub(crate) fn smoke_args(database_url: String) -> CasesArgs {
    CasesArgs {
        cases: vec![1, 2],
        workers: None,
        workers_per_case: Some(1),
        layout: vec![Layout::Shared, Layout::Apart, Layout::Databases],
        book: 4,
        prices: Prices::Skewed,
        seconds: 1,
        max_retries: 100,
        implementation: Implementation::CompiledIndexed,
        database_url,
        reset: true,
        format: OutputFormat::Json,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_two_orders_cross_whatever_the_spread() {
        for prices in [Prices::Distinct, Prices::Uniform, Prices::Skewed] {
            let best_bid = (0..2000)
                .step_by(2)
                .map(|n| order(n, prices).1)
                .max()
                .unwrap();
            let best_ask = (1..2001)
                .step_by(2)
                .map(|n| order(n, prices).1)
                .min()
                .unwrap();
            assert!(
                best_bid < best_ask,
                "{prices:?}: {best_bid} against {best_ask}"
            );
        }
    }

    #[test]
    fn distinct_prices_are_distinct_and_uniform_ones_are_not() {
        let distinct: BTreeSet<_> = (0..1000)
            .step_by(2)
            .map(|n| order(n, Prices::Distinct).1)
            .collect();
        assert_eq!(distinct.len(), 500);
        let uniform: BTreeSet<_> = (0..1000)
            .step_by(2)
            .map(|n| order(n, Prices::Uniform).1)
            .collect();
        assert_eq!(uniform.len(), 1);
    }
}
