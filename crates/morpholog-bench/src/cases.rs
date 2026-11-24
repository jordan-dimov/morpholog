//! Independent cases on one record (#906).
//!
//! Each case is a contract with its own book. Case-local admission
//! checks an order only against its own contract, so a rule never makes
//! two contracts' orders conflict. The question is what PostgreSQL's
//! physical locking costs them anyway: the committed rate when the cases
//! share one predicate, when each has its own predicate, and when each
//! has its own database, the control. That separates each case's
//! database-local structures; the cases still share the server.
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

mod diagnosis;

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

    /// Resting orders each case starts with; workers add to them.
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
    /// `<name>_cases_<run>_<i>` on the same server, under a name unique
    /// to the run, and drops only those.
    #[arg(long, env = "DATABASE_URL")]
    database_url: String,

    /// Acknowledge that every database the run uses is truncated.
    #[arg(long)]
    reset: bool,

    #[arg(long, value_enum, default_value = "markdown")]
    format: OutputFormat,

    /// Runs of every point. Each repeat runs the layouts in the order
    /// opposite to the one before, so drift falls on all of them.
    #[arg(long, default_value_t = 1)]
    repeat: usize,

    /// Every act also requires that its contract is not halted, a
    /// predicate almost always empty. Each `Halted` predicate the
    /// workers read has a writer of its own, halting contracts no worker
    /// uses about four times a second, so each case's read is written as
    /// often in every layout.
    #[arg(long)]
    halt: bool,

    /// Samples the serializable read locks every 50 ms while the workers
    /// run, in the shared and apart layouts. It costs a connection and
    /// a query each sample, so compare its rates with a run without it.
    #[arg(long)]
    sample_locks: bool,

    /// The server's log, written with `log_line_prefix = '%m [%p] %d '`.
    /// Each point then counts its failed attempts, retried ones included,
    /// by the statement they were running. Only databases named with the
    /// main database's name as a prefix count, so keep other work with
    /// such names off the server while it runs.
    #[arg(long)]
    server_log: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Layout {
    /// One `Resting` predicate for every case: today's shape.
    Shared,
    /// One predicate per case. Diagnostic only: it changes the
    /// programme, and the cases still share the claims table and the log.
    Apart,
    /// One database per case: the control. It separates database-local
    /// structures; the server is still shared.
    Databases,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum, Serialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Prices {
    /// Every order at its own price.
    Distinct,
    /// Every bid at one price, every ask at another.
    Uniform,
    /// On each side, nine orders in ten at the uniform price, the rest
    /// distinct.
    Skewed,
}

fn book_programme(suffix: &str, halt: bool) -> Result<Program> {
    let halting = if halt {
        format!(
            "
predicate Halted{suffix}(contract: Subject)

transformation halt(contract):
    admit Halted{suffix}(contract)
"
        )
    } else {
        String::new()
    };
    let open = if halt {
        format!("\n    require open: not Halted{suffix}(contract)")
    } else {
        String::new()
    };
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
    require listed: Contract{suffix}(contract){open}
    admit Resting{suffix}(order, contract, side, price, qty)
{halting}"
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
        // Counted within each side, so both sides get the exceptions.
        Prices::Skewed => (n / 2).is_multiple_of(10),
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
    /// Orders as (case, position), by how their act ended.
    committed: Vec<(usize, usize)>,
    exhausted: Vec<(usize, usize)>,
    /// Acts that failed because the server ran out of a resource, as an
    /// embedder would see it, with the first such message.
    failed: Vec<(usize, usize)>,
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
    repeat: usize,
    /// Failed attempts, retried ones included, by kind and the
    /// statement they were running, with `--server-log`.
    failures_by_stage: Option<BTreeMap<String, u64>>,
    /// Halts committed, with `--halt`.
    halts: Option<usize>,
    /// The serializable read locks, with `--sample-locks`.
    locks: Option<diagnosis::Locks>,
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
    halt: bool,
    debug_assertions: bool,
}

pub(crate) async fn run_cases(args: CasesArgs) -> Result<()> {
    check_reset_ack(args.reset, &args.database_url)?;
    if args.cases.is_empty()
        || args.cases.contains(&0)
        || args.layout.is_empty()
        || args.repeat == 0
    {
        bail!(
            "--cases needs counts above zero, --layout at least one layout, and --repeat one run at least"
        );
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

    // Each case count's rows go out as soon as its layouts are done, so a
    // failure later in the sweep loses nothing already measured.
    match args.format {
        OutputFormat::Markdown => print!("{}", render_header(&identity)),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string(&identity).context("serialise the identity")?
        ),
    }
    let mut owned = Owned::new(&options)?;
    let swept = sweep(&args, &options, &mut owned, &main).await;
    // This run's databases go whether the sweep finished or not.
    let dropped = owned.drop_all(&main).await;
    swept.and(dropped)
}

async fn sweep(
    args: &CasesArgs,
    options: &PgConnectOptions,
    owned: &mut Owned,
    main: &PgPool,
) -> Result<()> {
    for &cases in &args.cases {
        let per_case = args
            .workers
            .map_or(args.workers_per_case.unwrap_or(1), |w| w / cases);
        for repeat in 0..args.repeat {
            let mut layouts = args.layout.clone();
            if repeat % 2 == 1 {
                layouts.reverse();
            }
            let mut points = Vec::with_capacity(layouts.len());
            for layout in layouts {
                let mut point =
                    run_point(args, options, owned, main, layout, cases, per_case).await?;
                point.repeat = repeat;
                points.push(point);
            }
            let control = points
                .iter()
                .find(|p| p.layout == Layout::Databases)
                .map(|p| p.rate)
                .filter(|r| *r > 0.0);
            for p in &mut points {
                p.independence_efficiency = control.map(|r| p.rate / r);
                match args.format {
                    OutputFormat::Markdown => print!("{}", render_row(p)),
                    OutputFormat::Json => {
                        println!("{}", serde_json::to_string(p).context("serialise a point")?)
                    }
                }
            }
        }
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

/// Refuses a point the server cannot hold, before its fixture is built:
/// a connection refused mid-point would discard everything before it.
async fn require_connections(main: &PgPool, needed: usize) -> Result<()> {
    let free: i64 = sqlx::query_scalar(
        "SELECT current_setting('max_connections')::int
              - current_setting('superuser_reserved_connections')::int
              - (SELECT count(*) FROM pg_stat_activity WHERE backend_type = 'client backend')",
    )
    .fetch_one(main)
    .await
    .context("count the server's free connections")?;
    if usize::try_from(free).unwrap_or(0) < needed {
        bail!(
            "this point needs {needed} connections and the server has {free} free; raise max_connections or lower the workers"
        );
    }
    Ok(())
}

/// The case databases this run created, under a name no other run
/// shares. Only these are ever reset or dropped.
struct Owned {
    prefix: String,
    created: BTreeSet<usize>,
}

impl Owned {
    fn new(options: &PgConnectOptions) -> Result<Self> {
        let base = options
            .get_database()
            .ok_or_else(|| anyhow!("the database URL names no database"))?;
        let run = uuid::Uuid::now_v7().simple().to_string();
        // The last characters of a v7 id are its random part.
        Ok(Owned {
            prefix: format!("{base}_cases_{}", &run[run.len() - 8..]),
            created: BTreeSet::new(),
        })
    }

    fn name(&self, case: usize) -> String {
        format!("{}_{case}", self.prefix)
    }

    /// The case's database, created on first use. A database already
    /// under the name is refused, never adopted.
    async fn pool(
        &mut self,
        main: &PgPool,
        options: &PgConnectOptions,
        case: usize,
        connections: usize,
    ) -> Result<PgPool> {
        let name = self.name(case);
        if !self.created.contains(&case) {
            // Safe: the name is quoted, and built from the main database's.
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "CREATE DATABASE {}",
                quote_ident(&name)
            )))
            .execute(main)
            .await
            .with_context(|| format!("create {name}, which must not exist yet"))?;
            self.created.insert(case);
            let pool = connect(&options.clone().database(&name), 1).await?;
            morpholog_postgres::initialise_schema(&pool)
                .await
                .with_context(|| format!("initialise {name}"))?;
            pool.close().await;
        }
        connect(&options.clone().database(&name), connections).await
    }

    async fn drop_all(&self, main: &PgPool) -> Result<()> {
        for &case in &self.created {
            let name = self.name(case);
            // FORCE ends this run's own lingering connections.
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "DROP DATABASE {} WITH (FORCE)",
                quote_ident(&name)
            )))
            .execute(main)
            .await
            .with_context(|| format!("drop {name}"))?;
        }
        Ok(())
    }
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

async fn targets(
    args: &CasesArgs,
    options: &PgConnectOptions,
    owned: &mut Owned,
    main: &PgPool,
    layout: Layout,
    cases: usize,
    per_case: usize,
) -> Result<Vec<Target>> {
    let target = |pool: PgPool, suffix: &str, cases: Vec<usize>| -> Result<Target> {
        let core = book_programme(suffix, args.halt)?;
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
                let pool = owned
                    .pool(main, options, c, per_case + usize::from(args.halt))
                    .await?;
                out.push(target(pool, "", vec![c])?);
            }
            out
        }
    })
}

async fn run_point(
    args: &CasesArgs,
    options: &PgConnectOptions,
    owned: &mut Owned,
    main: &PgPool,
    layout: Layout,
    cases: usize,
    per_case: usize,
) -> Result<Point> {
    let workers = cases * per_case;
    let halters = match (args.halt, layout) {
        (false, _) => 0,
        (true, Layout::Shared) => 1,
        (true, _) => cases,
    };
    require_connections(main, workers + halters + 2).await?;
    // Shared and apart workers draw on the main pool.
    let main = if layout == Layout::Databases {
        main.clone()
    } else {
        connect(&main.connect_options(), workers + halters + 2).await?
    };
    let targets = targets(args, options, owned, &main, layout, cases, per_case).await?;

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
    let mut log = args
        .server_log
        .as_deref()
        .map(|path| {
            diagnosis::LogCursor::at_end(
                path,
                main.connect_options().get_database().unwrap_or_default(),
            )
        })
        .transpose()?;
    let sampler = if args.sample_locks && layout != Layout::Databases {
        Some(diagnosis::Sampler::start(
            connect(&main.connect_options(), 1).await?,
        ))
    } else {
        None
    };
    let started = Instant::now();
    let load = Load {
        per_case,
        book: args.book,
        prices: args.prices,
        max_retries: args.max_retries,
        deadline: started + Duration::from_secs(args.seconds),
    };
    let halting: Vec<_> = if args.halt {
        targets
            .iter()
            .map(|t| {
                let t = Arc::clone(t);
                tokio::spawn(async move { halt_spares(&t, load.deadline).await })
            })
            .collect()
    } else {
        Vec::new()
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
    let mut halts = args.halt.then_some(0);
    for h in halting {
        let n = h.await.context("join a halting writer")??;
        halts = halts.map(|total| total + n);
    }
    let locks = match sampler {
        Some(s) => Some(s.finish().await?),
        None => None,
    };

    for group in by_database(targets.iter().map(|t| &**t)) {
        reconcile(&group, args.book, args.prices, &tally).await?;
    }
    for t in &targets {
        t.pool.close().await;
    }
    // After the reconciliation, so the server has logged every failure.
    let failures_by_stage = log
        .as_mut()
        .map(diagnosis::LogCursor::failures_since)
        .transpose()?;

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
        repeat: 0,
        failures_by_stage,
        halts,
        locks,
    })
}

/// Halts contracts no worker trades, one every quarter second, so the
/// predicate the target's acts read is written now and then but never
/// matches. The halts committed.
async fn halt_spares(target: &Target, deadline: Instant) -> Result<usize> {
    let mut spare = 0usize;
    while Instant::now() < deadline {
        let proposal = Proposal::gateway(&Transition {
            transformation_name: TransformationName::from("halt"),
            args: vec![EvalValue::Subject(Subject::from(
                format!("spare_{spare}").as_str(),
            ))],
            actor: Subject::from("bench"),
            decided_at: None,
        });
        match propose_against_pg(&target.pool, &target.program, &proposal).await {
            Ok(PgProposalOutcome::Committed { .. }) => spare += 1,
            Ok(PgProposalOutcome::Rejected { reason, .. }) => bail!("a halt was refused: {reason}"),
            Err(PgError::SerializationFailure | PgError::DeadlockDetected) => {}
            Err(PgError::Database(e)) if out_of_resources(&e) => {}
            Err(e) => return Err(anyhow::Error::new(e).context("halt a spare contract")),
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    Ok(spare)
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
                    tally.committed.push((case, n));
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
                        tally.exhausted.push((case, n));
                        break;
                    }
                    // The contend scenario's backoff: linear, no jitter.
                    tokio::time::sleep(Duration::from_micros(100 * attempt)).await;
                }
                Err(PgError::Database(e)) if out_of_resources(&e) => {
                    tally.wasted += tried.elapsed();
                    tally.failed.push((case, n));
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

/// A resting claim's arguments as text, decimals normalised, so a
/// claim read back compares with the one the fixture submitted.
fn rendered(args: &[EvalValue]) -> Result<Vec<String>> {
    args.iter()
        .map(|v| match v {
            EvalValue::Subject(s) => Ok(s.to_string()),
            EvalValue::Decimal(d) => Ok(d.normalize().to_string()),
            other => Err(anyhow!(
                "a resting claim with an argument of another kind: {other:?}"
            )),
        })
        .collect()
}

fn expected_claim(case: usize, n: usize, prices: Prices) -> Vec<String> {
    let (side, price) = order(n, prices);
    vec![
        order_id(case, n),
        contract(case),
        side.to_string(),
        price.normalize().to_string(),
        Decimal::ONE.to_string(),
    ]
}

/// Every order submitted to one database against what it holds. The
/// book must be exactly the prepopulated and committed orders, claim for
/// claim. Each must be asserted once in the history and nothing else
/// asserted there, an abandoned or failed order never, and the history
/// must replay to the claims.
async fn reconcile(group: &[&Target], book: usize, prices: Prices, tally: &Tally) -> Result<()> {
    let pool = &group[0].pool;
    let mut asserted: BTreeMap<(String, Vec<String>), usize> = BTreeMap::new();
    for row in list_audit_rows(pool).await.context("read the history")? {
        for claim in &row.asserted_claims {
            let predicate = claim.predicate.as_str();
            if group.iter().any(|t| t.resting == predicate) {
                *asserted
                    .entry((predicate.to_string(), rendered(&claim.args)?))
                    .or_default() += 1;
            }
        }
    }
    let mut expected_anywhere = BTreeSet::new();
    for t in group {
        let ours = |&&(case, _): &&(usize, usize)| t.cases.contains(&case);
        let mut expected: BTreeSet<Vec<String>> = t
            .cases
            .iter()
            .flat_map(|&c| (0..book).map(move |n| expected_claim(c, n, prices)))
            .collect();
        expected.extend(
            tally
                .committed
                .iter()
                .filter(ours)
                .map(|&(c, n)| expected_claim(c, n, prices)),
        );

        let claims = list_claims_for_predicates(pool, std::slice::from_ref(&t.resting))
            .await
            .context("read the book")?;
        let held: BTreeSet<Vec<String>> = claims
            .iter()
            .map(|c| rendered(&c.args))
            .collect::<Result<_>>()?;
        if held.len() != claims.len() {
            bail!("{} holds the same resting claim twice", t.resting);
        }
        if held != expected {
            bail!(
                "{} is not the committed orders: {} missing (first {:?}), {} unexpected (first {:?})",
                t.resting,
                expected.difference(&held).count(),
                expected.difference(&held).take(3).collect::<Vec<_>>(),
                held.difference(&expected).count(),
                held.difference(&expected).take(3).collect::<Vec<_>>()
            );
        }
        for claim in &expected {
            match asserted.get(&(t.resting.clone(), claim.clone())) {
                Some(1) => {}
                Some(n) => bail!("{claim:?} is asserted {n} times in the history"),
                None => bail!("{claim:?} is on the book but not in the history"),
            }
        }
        for &(c, n) in tally.exhausted.iter().chain(&tally.failed).filter(ours) {
            let claim = expected_claim(c, n, prices);
            if asserted.contains_key(&(t.resting.clone(), claim.clone())) {
                bail!("{claim:?} was abandoned or failed but the history asserts it");
            }
        }
        expected_anywhere.extend(expected.into_iter().map(|c| (t.resting.clone(), c)));
    }
    if let Some((key, _)) = asserted
        .iter()
        .find(|(k, _)| !expected_anywhere.contains(*k))
    {
        bail!("the history asserts {key:?}, which no committed order submitted");
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
        halt: args.halt,
        debug_assertions: cfg!(debug_assertions),
    })
}

fn render_header(id: &Identity) -> String {
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
    out
}

fn render_row(p: &Point) -> String {
    let mut out = format!(
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
    );
    if let Some(stages) = &p.failures_by_stage {
        for (stage, n) in stages {
            out.push_str(&format!("  {n} attempts: {stage}\n"));
        }
    }
    if let Some(halts) = p.halts {
        out.push_str(&format!("  {halts} halts committed\n"));
    }
    if let Some(locks) = &p.locks {
        out.push_str(&format!(
            "  locks: peak {} a transaction, {} at once; most held: {}\n",
            locks.peak_per_transaction(),
            locks.peak_total(),
            locks.summary()
        ));
    }
    if let Some(error) = &p.first_error {
        out.push_str(&format!(
            "\n{:?} at {} cases, {} a case: {} acts failed, first with: {error}\n\n",
            p.layout, p.cases, p.workers_per_case, p.failed
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
        repeat: 2,
        halt: true,
        sample_locks: true,
        server_log: None,
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
    fn skewed_prices_spread_on_both_sides() {
        for side in [0, 1] {
            let spread: BTreeSet<_> = (side..200)
                .step_by(2)
                .map(|n| order(n, Prices::Skewed).1)
                .collect();
            assert_eq!(
                spread.len(),
                11,
                "side {side}: the uniform price and ten exceptions"
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
