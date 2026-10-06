//! `provision indexes`: reconcile the indexes a programme's executions
//! seek on against the database.
//!
//! The programme names the index specifications its loads and its
//! compiled checks need; this module reconciles them. The catalogue says
//! what exists; the registry says what Morpholog manages and which
//! programmes require it. Neither affects correctness: the loads and the
//! checks are right with no index, only slower and wider in what they
//! lock.
//!
//! Each specification lands in one state; a dry run prints the same plan
//! the executor applies:
//!
//! - absent -> `CREATE`;
//! - present under Morpholog's name, exact, valid -> `KEEP` (and adopted
//!   into the registry if a crash left it unrecorded);
//! - present under Morpholog's name, exact, invalid (a failed concurrent
//!   build) -> `REPAIR`: concurrent drop, concurrent create;
//! - present under Morpholog's name with another definition -> `CONFLICT`,
//!   never touched, reported for an operator;
//! - an equivalent index under another name -> `SATISFIED EXTERNALLY`,
//!   unmanaged, never pruned;
//! - managed but required by no programme -> `STALE`, dropped only under
//!   `prune`.
//!
//! Indexes are built and dropped `CONCURRENTLY`, each as its own statement
//! outside any transaction, so the table stays writable. A session advisory
//! lock lets only one provisioner run at a time. Equivalence is structural:
//! the expression and partial predicate as PostgreSQL renders them, the
//! access method and the key count. Never `CREATE` strings or
//! `IF NOT EXISTS`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use morpholog_core::format::canonical_hash;
use serde::Serialize;
use sqlx::Row as _;

use crate::PgPool;
use crate::compiled::{IndexSpec, StatisticsSpec};
use crate::error::{PgError, classify, classify_checked_query};
use crate::program::PgProgram;
use crate::sql_quote::quote_ident;

/// One advisory lock for index reconciliation on the claims table.
const RECONCILE_LOCK_KEY: i64 = 0x4d4f_5250_4849_4458;
/// How long a provisioner waits for another to finish.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexAction {
    Keep,
    Create,
    RepairInvalid,
    SatisfiedExternally,
    Stale,
    Conflict,
}

impl fmt::Display for IndexAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            IndexAction::Keep => "KEEP",
            IndexAction::Create => "CREATE",
            IndexAction::RepairInvalid => "REPAIR INVALID",
            IndexAction::SatisfiedExternally => "SATISFIED EXTERNALLY",
            IndexAction::Stale => "STALE",
            IndexAction::Conflict => "CONFLICT",
        })
    }
}

#[derive(Debug, Clone)]
pub struct IndexPlanEntry {
    pub action: IndexAction,
    pub index_name: String,
    pub predicate: String,
    pub position: usize,
    pub representation: &'static str,
    /// The indexed expression, as it appears inside `CREATE INDEX`'s
    /// parentheses, and the partial predicate; empty on a stale entry.
    pub expression_sql: String,
    pub partial_predicate_sql: String,
    /// What the action rests on: the external index that satisfies, what
    /// a conflict differs in, why a stale index is stale.
    pub detail: String,
    /// The programmes that require the index once every named programme's
    /// requirements are replaced, by identity; none on a stale entry.
    pub required_by: Vec<String>,
}

/// What reconciling one statistics object does. An object under
/// Morpholog's name with another definition is a conflict whatever
/// requires it; one nobody requires is stale, dropped under `prune`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum StatisticsAction {
    Keep,
    Create,
    Conflict,
    Stale,
}

impl fmt::Display for StatisticsAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            StatisticsAction::Keep => "KEEP",
            StatisticsAction::Create => "CREATE",
            StatisticsAction::Conflict => "CONFLICT",
            StatisticsAction::Stale => "STALE",
        })
    }
}

/// Statistics on one position's seek expression, across the claims
/// table: what lets the planner see that a seek on a partial index is
/// selective. Every object Morpholog manages is reconciled, the named
/// programmes' and other programmes' alike, since the position says all
/// there is to know about it.
#[derive(Debug, Clone)]
pub struct StatisticsPlanEntry {
    pub action: StatisticsAction,
    pub statistics_name: String,
    pub position: usize,
    pub expression_sql: String,
    /// What a conflict differs in, why a stale object is stale, or what
    /// keeps an unrequired one; empty otherwise.
    pub detail: String,
    /// The programmes known to require the position once every named
    /// programme's requirements are replaced, by identity.
    pub required_by: Vec<String>,
}

/// A programme named in the call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionedProgram {
    pub identity: String,
    pub hash: String,
}

/// A managed index the call did not reconcile: no named programme requires
/// it, and a programme outside the call does, which keeps a prune from
/// dropping it. It says nothing about the catalogue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequiredElsewhere {
    pub index_name: String,
    pub required_by: Vec<String>,
}

/// What one call planned and did. Every list has one order whatever order
/// the programmes were named in: programmes by identity, indexes and
/// `required_elsewhere` by name, statistics by position.
#[derive(Debug, Clone)]
pub struct ProvisionReport {
    pub programs: Vec<ProvisionedProgram>,
    pub entries: Vec<IndexPlanEntry>,
    pub statistics: Vec<StatisticsPlanEntry>,
    pub required_elsewhere: Vec<RequiredElsewhere>,
    /// Programmes outside the call with a recorded requirement whose
    /// position is not known: one recorded before positions were, for a
    /// specification no managed index or named programme resolves. While
    /// any, no statistics object is stale; provisioning them again
    /// records it.
    pub positions_unknown_for: Vec<String>,
    pub dry_run: bool,
    pub prune: bool,
    /// Whether the plan was executed: false for a dry run, and false when
    /// a conflict made the run apply nothing.
    pub applied: bool,
    /// Every managed object physically dropped, by name: the stale
    /// indexes and statistics of a run that applied under `prune`.
    pub pruned: Vec<String>,
}

impl ProvisionReport {
    /// Whether the database already holds everything the plan asks for:
    /// every index kept or satisfied by another, every statistics object
    /// kept. Anything to create or repair, anything stale and any conflict
    /// make it not current.
    pub fn is_current(&self) -> bool {
        self.entries.iter().all(|e| {
            matches!(
                e.action,
                IndexAction::Keep | IndexAction::SatisfiedExternally
            )
        }) && self
            .statistics
            .iter()
            .all(|s| s.action == StatisticsAction::Keep)
    }

    pub fn has_conflict(&self) -> bool {
        self.entries
            .iter()
            .any(|e| e.action == IndexAction::Conflict)
            || self
                .statistics
                .iter()
                .any(|s| s.action == StatisticsAction::Conflict)
    }
}

/// Whether a call may name these programmes: at least one, and no name
/// twice. A requirement set is kept per name, so the second of two
/// programmes under one name would replace the first's. Needs no database,
/// so a caller can ask before connecting; both entry points ask it too.
pub fn check_named_programs(programs: &[&PgProgram]) -> Result<(), PgError> {
    if programs.is_empty() {
        return Err(PgError::NoProgramNamed);
    }
    let mut seen = BTreeSet::new();
    for program in programs {
        let name = program.prepared().program().name.to_string();
        if !seen.insert(name.clone()) {
            return Err(PgError::ProgramNamedTwice(name));
        }
    }
    Ok(())
}

/// Plan the reconciliation of the programmes' union and change nothing;
/// `prune` is what the plan is for, so the stale entries say what an
/// applying run would drop.
pub async fn plan_indexes(
    pool: &PgPool,
    programs: &[&PgProgram],
    prune: bool,
) -> Result<ProvisionReport, PgError> {
    reconcile(pool, programs, false, prune).await
}

/// Reconcile the database to the union of the programmes' specifications;
/// with `prune`, also drop managed indexes no programme requires. A
/// conflict in any programme applies nothing for any of them.
pub async fn provision_indexes(
    pool: &PgPool,
    programs: &[&PgProgram],
    prune: bool,
) -> Result<ProvisionReport, PgError> {
    reconcile(pool, programs, true, prune).await
}

/// One index on the claims table as the catalogue describes it.
struct CatalogueIndex {
    name: String,
    valid: bool,
    /// A btree over exactly one expression key: the only shape a
    /// specification produces, so anything else cannot be equivalent.
    single_expression_btree: bool,
    expression: Option<String>,
    partial: Option<String>,
}

async fn catalogue(conn: &mut sqlx::PgConnection) -> Result<Vec<CatalogueIndex>, PgError> {
    let rows = sqlx::query!(
        r#"SELECT c.relname AS "name!",
                  i.indisvalid AS "valid!",
                  am.amname AS "am!",
                  i.indnkeyatts AS "keys!",
                  i.indkey::text AS "indkey!",
                  pg_get_expr(i.indexprs, i.indrelid) AS "expression?",
                  pg_get_expr(i.indpred, i.indrelid) AS "partial?"
           FROM pg_index i
           JOIN pg_class c ON c.oid = i.indexrelid
           JOIN pg_am am ON am.oid = c.relam
           WHERE i.indrelid = 'morpholog.claims'::regclass"#
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(classify_checked_query)?;
    Ok(rows
        .into_iter()
        .map(|r| CatalogueIndex {
            name: r.name,
            valid: r.valid,
            single_expression_btree: r.am == "btree" && r.keys == 1 && r.indkey == "0",
            expression: r.expression,
            partial: r.partial,
        })
        .collect())
}

/// A specification's expression and partial predicate as PostgreSQL
/// renders them. Built on a temporary copy of the claims table in a
/// rolled-back transaction, so the real table is untouched and both sides
/// of the comparison come from the same renderer.
async fn normalise(
    conn: &mut sqlx::PgConnection,
    spec: &IndexSpec,
) -> Result<(Option<String>, Option<String>), PgError> {
    sqlx::raw_sql("BEGIN")
        .execute(&mut *conn)
        .await
        .map_err(classify)?;
    let result: Result<(Option<String>, Option<String>), PgError> = async {
        sqlx::raw_sql(
            "CREATE TEMP TABLE morpholog_index_probe (LIKE morpholog.claims) ON COMMIT DROP",
        )
        .execute(&mut *conn)
        .await
        .map_err(classify)?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE INDEX morpholog_index_probe_ix ON morpholog_index_probe USING btree (({})) WHERE {}",
            spec.expression_sql, spec.partial_predicate_sql
        )))
        .execute(&mut *conn)
        .await
        .map_err(classify)?;
        let row = sqlx::query(
            "SELECT pg_get_expr(indexprs, indrelid), pg_get_expr(indpred, indrelid)
             FROM pg_index WHERE indexrelid = 'morpholog_index_probe_ix'::regclass",
        )
        .fetch_one(&mut *conn)
        .await
        .map_err(classify)?;
        Ok((row.get(0), row.get(1)))
    }
    .await;
    sqlx::raw_sql("ROLLBACK")
        .execute(&mut *conn)
        .await
        .map_err(classify)?;
    result
}

fn classify_spec(
    spec: &IndexSpec,
    normalised: &(Option<String>, Option<String>),
    catalogue: &[CatalogueIndex],
) -> (IndexAction, String) {
    let name = spec.index_name();
    let equivalent = |ix: &CatalogueIndex| {
        ix.single_expression_btree && ix.expression == normalised.0 && ix.partial == normalised.1
    };
    if let Some(ours) = catalogue.iter().find(|ix| ix.name == name) {
        if !equivalent(ours) {
            return (
                IndexAction::Conflict,
                format!(
                    "the index under this name is defined as {} where {}, not {} where {}",
                    ours.expression.as_deref().unwrap_or("<no expression>"),
                    ours.partial.as_deref().unwrap_or("<no predicate>"),
                    normalised.0.as_deref().unwrap_or("<no expression>"),
                    normalised.1.as_deref().unwrap_or("<no predicate>")
                ),
            );
        }
        return if ours.valid {
            (IndexAction::Keep, String::new())
        } else {
            (
                IndexAction::RepairInvalid,
                "an interrupted concurrent build left it invalid".to_string(),
            )
        };
    }
    if let Some(external) = catalogue
        .iter()
        .find(|ix| ix.valid && ix.name != name && equivalent(ix))
    {
        return (
            IndexAction::SatisfiedExternally,
            format!("{} is equivalent and stays unmanaged", external.name),
        );
    }
    (IndexAction::Create, String::new())
}

/// A statistics object on the claims table as the catalogue describes it,
/// looked up by name in Morpholog's own schema only.
struct CatalogueStatistics {
    on_claims: bool,
    /// The plain columns it also covers, by name; none for ours.
    columns: Option<String>,
    /// The statistics kinds, as PostgreSQL spells the array: `{e}` for
    /// expression statistics alone.
    kinds: String,
    /// An explicit statistics target; none means the default. A target of
    /// zero collects nothing, so an altered target is never ours.
    target: Option<i32>,
    expressions: Option<Vec<String>>,
}

async fn catalogue_statistics(
    conn: &mut sqlx::PgConnection,
    name: &str,
) -> Result<Option<CatalogueStatistics>, PgError> {
    let row = sqlx::query!(
        r#"SELECT s.stxrelid = 'morpholog.claims'::regclass AS "on_claims!",
                  (SELECT string_agg(a.attname::text, ', ' ORDER BY a.attnum)
                   FROM pg_attribute a
                   WHERE a.attrelid = s.stxrelid AND a.attnum = ANY(s.stxkeys)) AS "columns?",
                  s.stxkind::text AS "kinds!",
                  s.stxstattarget::integer AS "target?",
                  pg_get_statisticsobjdef_expressions(s.oid) AS "expressions?"
           FROM pg_statistic_ext s
           WHERE s.stxnamespace = 'morpholog'::regnamespace AND s.stxname = $1"#,
        name
    )
    .fetch_optional(&mut *conn)
    .await
    .map_err(classify_checked_query)?;
    Ok(row.map(|r| CatalogueStatistics {
        on_claims: r.on_claims,
        columns: r.columns,
        kinds: r.kinds,
        target: r.target,
        expressions: r.expressions,
    }))
}

/// A statistics specification's expression as PostgreSQL renders it, from
/// a probe on a temporary copy of the claims table in a rolled-back
/// transaction, as the indexes are normalised.
async fn normalise_statistics(
    conn: &mut sqlx::PgConnection,
    spec: &StatisticsSpec,
) -> Result<Option<Vec<String>>, PgError> {
    sqlx::raw_sql("BEGIN")
        .execute(&mut *conn)
        .await
        .map_err(classify)?;
    let result: Result<Option<Vec<String>>, PgError> = async {
        sqlx::raw_sql(
            "CREATE TEMP TABLE morpholog_index_probe (LIKE morpholog.claims) ON COMMIT DROP",
        )
        .execute(&mut *conn)
        .await
        .map_err(classify)?;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE STATISTICS pg_temp.morpholog_statistics_probe ON ({}) FROM morpholog_index_probe",
            spec.expression_sql()
        )))
        .execute(&mut *conn)
        .await
        .map_err(classify)?;
        let row = sqlx::query(
            "SELECT pg_get_statisticsobjdef_expressions(oid) FROM pg_statistic_ext
             WHERE stxname = 'morpholog_statistics_probe' AND stxnamespace = pg_my_temp_schema()",
        )
        .fetch_one(&mut *conn)
        .await
        .map_err(classify)?;
        Ok(row.get(0))
    }
    .await;
    sqlx::raw_sql("ROLLBACK")
        .execute(&mut *conn)
        .await
        .map_err(classify)?;
    result
}

fn classify_statistics(
    name: &str,
    normalised: &Option<Vec<String>>,
    existing: Option<&CatalogueStatistics>,
) -> (StatisticsAction, String) {
    let Some(ours) = existing else {
        return (StatisticsAction::Create, String::new());
    };
    let listed = |expressions: &Option<Vec<String>>| {
        expressions
            .as_ref()
            .map_or_else(|| "no expressions".to_string(), |e| e.join(", "))
    };
    let mut differences = Vec::new();
    if !ours.on_claims {
        differences.push("it is on another table".to_string());
    }
    if &ours.expressions != normalised {
        differences.push(format!("it covers {}", listed(&ours.expressions)));
    }
    if let Some(columns) = &ours.columns {
        differences.push(format!("it also covers the columns {columns}"));
    }
    if ours.kinds != "{e}" {
        differences.push(format!("its statistics kinds are {}", ours.kinds));
    }
    if let Some(target) = ours.target {
        differences.push(format!(
            "its statistics target is {target}, which `ALTER STATISTICS morpholog.{} SET STATISTICS DEFAULT` restores",
            quote_ident(name)
        ));
    }
    if differences.is_empty() {
        return (StatisticsAction::Keep, String::new());
    }
    (
        StatisticsAction::Conflict,
        format!(
            "the statistics under this name differ from Morpholog's ({} on morpholog.claims, default target): {}",
            listed(normalised),
            differences.join("; ")
        ),
    )
}

/// A programme named in the call, as reconciliation reads it.
struct Named {
    identity: String,
    hash: String,
    specs: Vec<IndexSpec>,
}

async fn reconcile(
    pool: &PgPool,
    programs: &[&PgProgram],
    apply: bool,
    prune: bool,
) -> Result<ProvisionReport, PgError> {
    check_named_programs(programs)?;
    let mut named: Vec<Named> = programs
        .iter()
        .map(|program| Named {
            identity: program.prepared().program().name.to_string(),
            hash: canonical_hash(program.prepared().program()),
            specs: program.required_indexes(),
        })
        .collect();
    named.sort_by(|a, b| a.identity.cmp(&b.identity));

    // The lock holder also runs the DDL: concurrent builds cannot run
    // inside a transaction, and the lock keeps provisioning single-writer.
    let mut conn = pool.acquire().await.map_err(classify)?;
    // Polled, not a blocking `pg_advisory_lock`: a blocked session holds a
    // snapshot, and a concurrent index build waits for every snapshot to
    // end, which deadlocks. Short separate tries let that build finish.
    let started = std::time::Instant::now();
    loop {
        let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(RECONCILE_LOCK_KEY)
            .fetch_one(&mut *conn)
            .await
            .map_err(classify)?;
        if got {
            break;
        }
        if started.elapsed() > LOCK_WAIT {
            return Err(PgError::InvalidState(
                "another provisioner has held the index reconciliation lock for over ten minutes"
                    .to_string(),
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let outcome = reconcile_locked(&mut conn, &named, apply, prune).await;
    // Released explicitly so a pooled connection never hands the lock to
    // another caller.
    let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(RECONCILE_LOCK_KEY)
        .execute(&mut *conn)
        .await;
    outcome
}

/// The requirements as they will stand once every named programme's
/// requirements are replaced: the named programmes' own specifications,
/// and what the registry records for every other identity. The report,
/// the stale sets and the prune all read this one relation.
struct Prospective {
    /// Who requires each specification, by spec digest.
    required: BTreeMap<String, BTreeSet<String>>,
    /// The position each required specification seeks on, where known: a
    /// named programme's specification says it, a recorded requirement may,
    /// and the managed index of the same specification does.
    position_of: BTreeMap<String, usize>,
    /// Outside identities with a requirement whose position none of those
    /// resolve.
    positions_unknown_for: Vec<String>,
}

impl Prospective {
    fn required_by(&self, digest: &str) -> Vec<String> {
        self.required
            .get(digest)
            .map(|identities| identities.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Who requires each position, over the specifications whose position
    /// is known.
    fn required_positions(&self) -> BTreeMap<usize, BTreeSet<String>> {
        let mut by_position: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
        for (digest, identities) in &self.required {
            if let Some(position) = self.position_of.get(digest) {
                by_position
                    .entry(*position)
                    .or_default()
                    .extend(identities.iter().cloned());
            }
        }
        by_position
    }
}

async fn prospective_requirements(
    conn: &mut sqlx::PgConnection,
    named: &[Named],
) -> Result<Prospective, PgError> {
    let identities: Vec<String> = named.iter().map(|p| p.identity.clone()).collect();
    // A subquery, not an outer join: the cache's nullability inference
    // reads the planner's plan, and an outer join's plan changes with
    // the rows, which made the committed cache differ from a checked one.
    let outside = sqlx::query!(
        r#"SELECT r.program_identity AS "identity!", r.spec_digest AS "digest!",
                  r.position AS "recorded?",
                  (SELECT m.position FROM morpholog.managed_index m
                    WHERE m.spec_digest = r.spec_digest) AS "managed?"
           FROM morpholog.index_requirement r
           WHERE NOT (r.program_identity = ANY($1))"#,
        &identities,
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(classify_checked_query)?;
    let mut required: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut position_of: BTreeMap<String, usize> = BTreeMap::new();
    // A position decides what a prune drops, so every source that knows
    // one for a specification must say the same; a disagreement is
    // refused before any change rather than settled by whichever came
    // first.
    let mut learn = |digest: &str, position: i32, source: &str| -> Result<(), PgError> {
        let position = usize::try_from(position).map_err(|_| {
            PgError::InvalidState(format!(
                "the {source} of specification {digest} records position {position}"
            ))
        })?;
        match position_of.insert(digest.to_string(), position) {
            Some(known) if known != position => Err(PgError::InvalidState(format!(
                "specification {digest} seeks on position {known}, but the {source} records {position}; \
                 provisioning refuses to drop anything until the registry agrees with itself"
            ))),
            _ => Ok(()),
        }
    };
    for program in named {
        for spec in &program.specs {
            required
                .entry(spec.digest())
                .or_default()
                .insert(program.identity.clone());
            learn(&spec.digest(), spec.position as i32, "named programme")?;
        }
    }
    for row in &outside {
        required
            .entry(row.digest.clone())
            .or_default()
            .insert(row.identity.clone());
        if let Some(position) = row.managed {
            learn(&row.digest, position, "managed index")?;
        }
        if let Some(position) = row.recorded {
            learn(&row.digest, position, "recorded requirement")?;
        }
    }
    let positions_unknown_for: Vec<String> = outside
        .iter()
        .filter(|row| !position_of.contains_key(&row.digest))
        .map(|row| row.identity.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    Ok(Prospective {
        required,
        position_of,
        positions_unknown_for,
    })
}

/// The positions of every statistics object under Morpholog's exact
/// naming in its schema, whatever their definition.
async fn managed_statistics_positions(
    conn: &mut sqlx::PgConnection,
) -> Result<BTreeSet<usize>, PgError> {
    let names = sqlx::query!(
        r#"SELECT stxname AS "name!" FROM pg_statistic_ext
           WHERE stxnamespace = 'morpholog'::regnamespace AND stxname LIKE 'morpholog\_cs\_%'"#
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(classify_checked_query)?;
    Ok(names
        .iter()
        .filter_map(|row| StatisticsSpec::from_name(&row.name))
        .map(|spec| spec.position)
        .collect())
}

async fn reconcile_locked(
    conn: &mut sqlx::PgConnection,
    named: &[Named],
    apply: bool,
    prune: bool,
) -> Result<ProvisionReport, PgError> {
    let specs: Vec<IndexSpec> = named
        .iter()
        .flat_map(|p| p.specs.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let prospective = prospective_requirements(conn, named).await?;
    let required_by = |digest: &str| prospective.required_by(digest);

    let catalogue = catalogue(conn).await?;
    // Every position some programme is known to require, and every
    // statistics object under Morpholog's name: an object is fully known
    // from its position, so another programme's is reconciled here too.
    let required_positions = prospective.required_positions();
    let positions: BTreeSet<usize> = required_positions
        .keys()
        .copied()
        .chain(managed_statistics_positions(conn).await?)
        .collect();
    let statistics_specs: Vec<StatisticsSpec> = positions
        .into_iter()
        .map(|position| StatisticsSpec { position })
        .collect();
    let mut statistics = Vec::with_capacity(statistics_specs.len());
    for spec in &statistics_specs {
        let normalised = normalise_statistics(conn, spec).await?;
        let existing = catalogue_statistics(conn, &spec.name()).await?;
        let (found, mut detail) = classify_statistics(&spec.name(), &normalised, existing.as_ref());
        let required_by: Vec<String> = required_positions
            .get(&spec.position)
            .map(|identities| identities.iter().cloned().collect())
            .unwrap_or_default();
        let action = match found {
            StatisticsAction::Conflict => StatisticsAction::Conflict,
            StatisticsAction::Create => StatisticsAction::Create,
            _ if !required_by.is_empty() => StatisticsAction::Keep,
            _ if !prospective.positions_unknown_for.is_empty() => {
                detail = format!(
                    "required by no programme known; kept while positions are unknown for {}",
                    prospective.positions_unknown_for.join(", ")
                );
                StatisticsAction::Keep
            }
            _ => {
                detail = "required by no programme; `--prune` drops it".to_string();
                StatisticsAction::Stale
            }
        };
        statistics.push(StatisticsPlanEntry {
            action,
            statistics_name: spec.name(),
            position: spec.position,
            expression_sql: spec.expression_sql(),
            detail,
            required_by,
        });
    }
    let mut entries = Vec::with_capacity(specs.len());
    for spec in &specs {
        let normalised = normalise(conn, spec).await?;
        let (action, detail) = classify_spec(spec, &normalised, &catalogue);
        entries.push(IndexPlanEntry {
            action,
            index_name: spec.index_name(),
            predicate: spec.predicate.to_string(),
            position: spec.position,
            representation: crate::compiled::SEEK_REPRESENTATION,
            expression_sql: spec.expression_sql.clone(),
            partial_predicate_sql: spec.partial_predicate_sql.clone(),
            detail,
            required_by: required_by(&spec.digest()),
        });
    }

    // Managed indexes no named programme requires: stale when nobody else
    // does either, required elsewhere otherwise.
    let current: BTreeSet<String> = specs.iter().map(IndexSpec::digest).collect();
    let managed = sqlx::query!(
        r#"SELECT m.spec_digest AS "digest!", m.index_name AS "index_name!",
                  m.predicate_name AS "predicate!", m.position AS "position!",
                  m.representation AS "representation!"
           FROM morpholog.managed_index m
           ORDER BY m.index_name"#,
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(classify_checked_query)?;
    let mut stale = Vec::new();
    let mut required_elsewhere = Vec::new();
    for row in managed {
        if current.contains(&row.digest) {
            continue;
        }
        let elsewhere = required_by(&row.digest);
        if elsewhere.is_empty() {
            stale.push(row);
        } else {
            required_elsewhere.push(RequiredElsewhere {
                index_name: row.index_name,
                required_by: elsewhere,
            });
        }
    }

    // Fail closed. A conflict means an index under Morpholog's name differs
    // from the specification. Reconciling around it would drop a
    // programme's requirement, and a later prune could then drop the
    // operator's index as stale. Nothing is applied; the report says why.
    let applied = apply
        && !entries.iter().any(|e| e.action == IndexAction::Conflict)
        && !statistics
            .iter()
            .any(|s| s.action == StatisticsAction::Conflict);
    if applied {
        for (spec, entry) in specs.iter().zip(&entries) {
            match entry.action {
                IndexAction::RepairInvalid => {
                    drop_concurrently(conn, &entry.index_name).await?;
                    sqlx::raw_sql(sqlx::AssertSqlSafe(spec.create_sql()))
                        .execute(&mut *conn)
                        .await
                        .map_err(classify)?;
                }
                IndexAction::Create => {
                    sqlx::raw_sql(sqlx::AssertSqlSafe(spec.create_sql()))
                        .execute(&mut *conn)
                        .await
                        .map_err(classify)?;
                }
                IndexAction::Keep
                | IndexAction::SatisfiedExternally
                | IndexAction::Conflict
                | IndexAction::Stale => {}
            }
        }
        for (spec, entry) in statistics_specs.iter().zip(&statistics) {
            if entry.action == StatisticsAction::Create {
                sqlx::raw_sql(sqlx::AssertSqlSafe(spec.create_sql()))
                    .execute(&mut *conn)
                    .await
                    .map_err(classify)?;
            }
        }
        // Statistics over an expression index exist only from the first
        // ANALYZE after it is built. Without them the planner cannot tell
        // a selective key from one every row shares. Every applied run
        // analyzes, so an index adopted or kept from an earlier run that
        // stopped short has them too.
        sqlx::raw_sql("ANALYZE morpholog.claims")
            .execute(&mut *conn)
            .await
            .map_err(classify)?;
        // Record every managed specification, then replace every named
        // programme's requirement set whole, all in one transaction and
        // before any drop: a prune never sees some programmes replaced and
        // others not. The sets include specifications an operator's index
        // satisfies, so a requirement outlives the index serving it. Runs
        // on the held connection; the session lock outlives the
        // transaction.
        let mut tx = sqlx::Connection::begin(&mut *conn)
            .await
            .map_err(classify)?;
        for (spec, entry) in specs.iter().zip(&entries) {
            if !matches!(
                entry.action,
                IndexAction::Keep | IndexAction::Create | IndexAction::RepairInvalid
            ) {
                continue;
            }
            sqlx::query!(
                "INSERT INTO morpholog.managed_index
                    (spec_digest, index_name, predicate_name, position, representation,
                     expression_sql, partial_predicate)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (spec_digest) DO NOTHING",
                spec.digest(),
                spec.index_name(),
                spec.predicate.as_str(),
                spec.position as i32,
                crate::compiled::SEEK_REPRESENTATION,
                spec.expression_sql,
                spec.partial_predicate_sql,
            )
            .execute(&mut *tx)
            .await
            .map_err(classify_checked_query)?;
        }
        for program in named {
            sqlx::query!(
                "DELETE FROM morpholog.index_requirement WHERE program_identity = $1",
                program.identity
            )
            .execute(&mut *tx)
            .await
            .map_err(classify_checked_query)?;
            for spec in &program.specs {
                sqlx::query!(
                    "INSERT INTO morpholog.index_requirement
                        (program_identity, spec_digest, program_hash, position)
                     VALUES ($1, $2, $3, $4)",
                    program.identity,
                    spec.digest(),
                    program.hash,
                    spec.position as i32,
                )
                .execute(&mut *tx)
                .await
                .map_err(classify_checked_query)?;
            }
        }
        tx.commit().await.map_err(classify)?;
    }

    let mut pruned = Vec::new();
    for row in stale {
        let representation = match row.representation.as_str() {
            crate::compiled::SEEK_REPRESENTATION => crate::compiled::SEEK_REPRESENTATION,
            "numeric" => "numeric",
            "jsonb" => "jsonb",
            "quantity_amount" => "quantity_amount",
            _ => "text",
        };
        if applied && prune {
            drop_concurrently(conn, &row.index_name).await?;
            sqlx::query!(
                "DELETE FROM morpholog.managed_index WHERE index_name = $1",
                row.index_name
            )
            .execute(&mut *conn)
            .await
            .map_err(classify_checked_query)?;
            pruned.push(row.index_name.clone());
        }
        entries.push(IndexPlanEntry {
            action: IndexAction::Stale,
            index_name: row.index_name,
            predicate: row.predicate,
            position: row.position as usize,
            representation,
            expression_sql: String::new(),
            partial_predicate_sql: String::new(),
            detail: if applied && prune {
                "required by no programme; dropped".to_string()
            } else {
                "required by no programme; `--prune` drops it".to_string()
            },
            required_by: Vec::new(),
        });
    }
    entries.sort_by(|a, b| a.index_name.cmp(&b.index_name));
    if applied && prune {
        for entry in &mut statistics {
            if entry.action == StatisticsAction::Stale {
                sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                    "DROP STATISTICS IF EXISTS morpholog.{}",
                    quote_ident(&entry.statistics_name)
                )))
                .execute(&mut *conn)
                .await
                .map_err(classify)?;
                entry.detail = "required by no programme; dropped".to_string();
                pruned.push(entry.statistics_name.clone());
            }
        }
    }

    Ok(ProvisionReport {
        programs: named
            .iter()
            .map(|p| ProvisionedProgram {
                identity: p.identity.clone(),
                hash: p.hash.clone(),
            })
            .collect(),
        entries,
        statistics,
        required_elsewhere,
        positions_unknown_for: prospective.positions_unknown_for,
        dry_run: !apply,
        prune,
        applied,
        pruned,
    })
}

async fn drop_concurrently(conn: &mut sqlx::PgConnection, name: &str) -> Result<(), PgError> {
    // The only dynamic part is the name, quoted like every generated view.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP INDEX CONCURRENTLY IF EXISTS morpholog.{}",
        quote_ident(name)
    )))
    .execute(&mut *conn)
    .await
    .map_err(classify)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(index: IndexAction, statistics: StatisticsAction) -> ProvisionReport {
        ProvisionReport {
            programs: Vec::new(),
            entries: vec![IndexPlanEntry {
                action: index,
                index_name: "morpholog_ci_x".to_string(),
                predicate: "P".to_string(),
                position: 0,
                representation: "key",
                expression_sql: String::new(),
                partial_predicate_sql: String::new(),
                detail: String::new(),
                required_by: Vec::new(),
            }],
            statistics: vec![StatisticsPlanEntry {
                action: statistics,
                statistics_name: "morpholog_cs_0".to_string(),
                position: 0,
                expression_sql: String::new(),
                detail: String::new(),
                required_by: Vec::new(),
            }],
            required_elsewhere: Vec::new(),
            positions_unknown_for: Vec::new(),
            dry_run: true,
            prune: false,
            applied: false,
            pruned: Vec::new(),
        }
    }

    /// Current means nothing to do: an index kept or satisfied by another,
    /// a statistics object kept. Every other action is outstanding.
    #[test]
    fn current_is_nothing_left_to_do() {
        for index in [IndexAction::Keep, IndexAction::SatisfiedExternally] {
            assert!(
                report(index, StatisticsAction::Keep).is_current(),
                "{index}"
            );
        }
        for index in [
            IndexAction::Create,
            IndexAction::RepairInvalid,
            IndexAction::Stale,
            IndexAction::Conflict,
        ] {
            assert!(
                !report(index, StatisticsAction::Keep).is_current(),
                "{index}"
            );
        }
        for statistics in [
            StatisticsAction::Create,
            StatisticsAction::Stale,
            StatisticsAction::Conflict,
        ] {
            assert!(
                !report(IndexAction::Keep, statistics).is_current(),
                "{statistics}"
            );
        }
    }
}
