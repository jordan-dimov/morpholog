//! `provision indexes`: reconcile the indexes a programme's compiled SQL
//! can seek on against the database.
//!
//! The compiler emits the index specifications its SQL needs; this module
//! reconciles them. The catalogue says what exists; the registry says what
//! Morpholog manages and which programmes require it. Neither affects
//! correctness: the compiled checks are right with no index, only slower.
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

use std::fmt;

use morpholog_core::format::canonical_hash;
use sqlx::Row as _;

use crate::PgPool;
use crate::compiled::IndexSpec;
use crate::error::{PgError, classify, classify_checked_query};
use crate::program::PgProgram;
use crate::sql_quote::quote_ident;

/// One advisory lock for index reconciliation on the claims table.
const RECONCILE_LOCK_KEY: i64 = 0x4d4f_5250_4849_4458;
/// How long a provisioner waits for another to finish.
const LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
}

#[derive(Debug, Clone)]
pub struct ProvisionReport {
    pub program_identity: String,
    pub program_hash: String,
    pub entries: Vec<IndexPlanEntry>,
    /// Whether the plan was executed: false for a dry run, and false when
    /// a conflict made the run apply nothing.
    pub applied: bool,
    /// Stale indexes physically dropped, by name.
    pub pruned: Vec<String>,
}

impl ProvisionReport {
    pub fn has_conflict(&self) -> bool {
        self.entries
            .iter()
            .any(|e| e.action == IndexAction::Conflict)
    }
}

/// Plan the reconciliation and change nothing.
pub async fn plan_indexes(pool: &PgPool, program: &PgProgram) -> Result<ProvisionReport, PgError> {
    reconcile(pool, program, false, false).await
}

/// Reconcile the database to the programme's specifications; with
/// `prune`, also drop managed indexes no programme requires.
pub async fn provision_indexes(
    pool: &PgPool,
    program: &PgProgram,
    prune: bool,
) -> Result<ProvisionReport, PgError> {
    reconcile(pool, program, true, prune).await
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

async fn reconcile(
    pool: &PgPool,
    program: &PgProgram,
    apply: bool,
    prune: bool,
) -> Result<ProvisionReport, PgError> {
    let program_identity = program.core().program().name.to_string();
    let program_hash = canonical_hash(program.core().program());
    let specs = program.required_indexes();

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
    let outcome = reconcile_locked(
        &mut conn,
        &specs,
        &program_identity,
        &program_hash,
        apply,
        prune,
    )
    .await;
    // Released explicitly so a pooled connection never hands the lock to
    // another caller.
    let _ = sqlx::query("SELECT pg_advisory_unlock($1)")
        .bind(RECONCILE_LOCK_KEY)
        .execute(&mut *conn)
        .await;
    outcome
}

async fn reconcile_locked(
    conn: &mut sqlx::PgConnection,
    specs: &[IndexSpec],
    program_identity: &str,
    program_hash: &str,
    apply: bool,
    prune: bool,
) -> Result<ProvisionReport, PgError> {
    let catalogue = catalogue(conn).await?;
    let mut entries = Vec::with_capacity(specs.len());
    for spec in specs {
        let normalised = normalise(conn, spec).await?;
        let (action, detail) = classify_spec(spec, &normalised, &catalogue);
        entries.push(IndexPlanEntry {
            action,
            index_name: spec.index_name(),
            predicate: spec.predicate.to_string(),
            position: spec.position,
            representation: spec.representation.as_str(),
            expression_sql: spec.expression_sql.clone(),
            partial_predicate_sql: spec.partial_predicate_sql.clone(),
            detail,
        });
    }

    // Fail closed. A conflict means an index under Morpholog's name differs
    // from the specification. Reconciling around it would drop this
    // programme's requirement, and a later prune could then drop the
    // operator's index as stale. Nothing is applied; the report says why.
    let applied = apply && !entries.iter().any(|e| e.action == IndexAction::Conflict);
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
        // Record every managed specification, then replace this programme's
        // requirement set whole. It includes specifications an operator's
        // index satisfies, so a requirement outlives the index serving it.
        // Runs on the held connection; the session lock outlives the
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
                spec.representation.as_str(),
                spec.expression_sql,
                spec.partial_predicate_sql,
            )
            .execute(&mut *tx)
            .await
            .map_err(classify_checked_query)?;
        }
        sqlx::query!(
            "DELETE FROM morpholog.index_requirement WHERE program_identity = $1",
            program_identity
        )
        .execute(&mut *tx)
        .await
        .map_err(classify_checked_query)?;
        for (spec, entry) in specs.iter().zip(&entries) {
            if entry.action == IndexAction::Conflict {
                continue;
            }
            sqlx::query!(
                "INSERT INTO morpholog.index_requirement (program_identity, spec_digest, program_hash)
                 VALUES ($1, $2, $3)",
                program_identity,
                spec.digest(),
                program_hash,
            )
            .execute(&mut *tx)
            .await
            .map_err(classify_checked_query)?;
        }
        tx.commit().await.map_err(classify)?;
    }

    // Managed indexes no programme requires - after this programme's
    // requirements were replaced, or as they would stand once they are.
    let current: Vec<String> = specs.iter().map(IndexSpec::digest).collect();
    let stale = sqlx::query!(
        r#"SELECT m.index_name AS "index_name!", m.predicate_name AS "predicate!",
                  m.position AS "position!", m.representation AS "representation!"
           FROM morpholog.managed_index m
           WHERE NOT (m.spec_digest = ANY($2))
             AND NOT EXISTS (
                 SELECT 1 FROM morpholog.index_requirement r
                 WHERE r.spec_digest = m.spec_digest AND r.program_identity <> $1)
           ORDER BY m.index_name"#,
        program_identity,
        &current,
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(classify_checked_query)?;
    let mut pruned = Vec::new();
    for row in stale {
        let representation = match row.representation.as_str() {
            "numeric" => "numeric",
            "jsonb" => "jsonb",
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
        });
    }

    Ok(ProvisionReport {
        program_identity: program_identity.to_string(),
        program_hash: program_hash.to_string(),
        entries,
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
