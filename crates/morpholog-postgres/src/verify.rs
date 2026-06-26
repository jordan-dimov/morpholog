use crate::as_of::{ReplaySet, reconstruct_inner};
use crate::audit::REPLAY_CHUNK;
use crate::checkpoints::TreeVerification;
use crate::claims::decode_claim_rows;
use crate::error::{PgError, classify};
use crate::propose::REJECTION_KIND_INVARIANT;
use crate::txn::{TxIsolation, begin_isolated_tx};
use chrono::{DateTime, Utc};
use morpholog_core::{
    ClaimInstance, CoverageReport, CoverageTracker, PredicateName, Program, State,
};
use serde::Serialize;
use sqlx::PgPool;
use std::collections::HashMap;
use uuid::Uuid;
/// The canonical Morpholog schema, compiled into this crate. The same
/// file a repo checkout applies with `psql -f`; embedding it means a
/// binary-only deployment provisions exactly the schema this build
/// expects - nothing to vendor, nothing to drift.
pub const SCHEMA_SQL: &str = include_str!("../../morpholog-core/sql/schema.sql");
/// Outcome of [`initialise_schema`]: provisioned now, or found already
/// provisioned (the caller decides whether that is fine or an error).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitOutcome {
    Initialised,
    AlreadyInitialised,
}
/// Provision the `morpholog` schema in an existing database from the
/// embedded [`SCHEMA_SQL`]. Day-zero only: if the schema already
/// exists this returns [`InitOutcome::AlreadyInitialised`] without
/// touching anything - it never drops and never migrates. Schema
/// *evolution* is the deferred migrations story, not this function.
///
/// Provisioning is atomic: the existence check and the whole schema
/// script run in one transaction (the script is plain DDL, which
/// PostgreSQL rolls back like any other statement), so a mid-script
/// failure leaves nothing behind - in particular, no partial schema
/// the existence guard would later misread as already-initialised.
pub async fn initialise_schema(pool: &PgPool) -> Result<InitOutcome, PgError> {
    let mut tx = pool.begin().await.map_err(classify)?;
    let exists = sqlx::query!("SELECT 1 AS one FROM pg_namespace WHERE nspname = 'morpholog'")
        .fetch_optional(&mut *tx)
        .await
        .map_err(classify)?;
    if exists.is_some() {
        return Ok(InitOutcome::AlreadyInitialised);
    }
    sqlx::raw_sql(SCHEMA_SQL)
        .execute(&mut *tx)
        .await
        .map_err(classify)?;
    tx.commit().await.map_err(classify)?;
    Ok(InitOutcome::Initialised)
}
/// The outcome of replaying the audit log against the claims table.
///
/// The two tables are independent records of the same history: the
/// claims table is current state maintained write-by-write, the audit
/// log is the journal those writes came from. Replaying the journal
/// must land on the same claim set; a difference is evidence that one
/// of them was modified outside the runtime.
///
/// `Serialize` uses the same `status`-tagged representation as the
/// other CLI envelopes.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum VerifyOutcome {
    /// Replay reproduces the claims table exactly.
    Consistent {
        /// Committed transitions replayed.
        transitions: i64,
        /// Currently-admitted claims confirmed.
        claims: usize,
    },
    /// The two records disagree.
    Divergent {
        /// Claims present in the claims table that replaying the audit
        /// log does not produce - out-of-band inserts or edits.
        only_in_claims_table: Vec<ClaimInstance>,
        /// Claims the audit log says should be current but the claims
        /// table lacks - out-of-band deletes or edits.
        only_in_replay: Vec<ClaimInstance>,
    },
}
/// The `morpholog verify` envelope: the replay verdict (claims table vs
/// audit log) beside the tamper-evidence verdict (the audit Merkle tree
/// against its checkpoints), so one read carries both. Field order is
/// the wire contract; `replay` then `tree`.
#[derive(Debug, Clone, Serialize)]
pub struct VerifyReport {
    pub replay: VerifyOutcome,
    pub tree: TreeVerification,
}
/// Replay the audit log through a [`CoverageTracker`], then count
/// the rejection log into it, and report, per invariant of
/// `program`, whether its antecedent ever bound and whether it ever
/// refused a real proposal - the auditor questions "which of these
/// rules has ever actually done work?" and "which has demonstrably
/// said no?". See `morpholog_core::coverage` for the verdict
/// semantics and the bounds that shape them.
///
/// One `SERIALIZABLE READ ONLY DEFERRABLE` transaction reads the
/// whole log: the deferrable mode waits for a safe snapshot and then
/// runs with a guaranteed-serializable view and zero SSI footprint -
/// the right isolation for a long analytical read over a live system.
///
/// Cost: one pass over the audit log, plus a state snapshot and an
/// antecedent evaluation for each transition whose delta predicates
/// touch a tracked antecedent (the tracker's pruning). The same
/// scaling family as `verify` and as-of replay - an offline auditor
/// command, not a hot path.
pub async fn coverage_replay(pool: &PgPool, program: &Program) -> Result<CoverageReport, PgError> {
    let mut tx = begin_isolated_tx(pool, TxIsolation::SerializableReadOnlyDeferrable).await?;
    let mut tracker = CoverageTracker::new(program);
    let needs_pre = tracker.needs_pre_state();
    let mut replay = ReplaySet::new();
    // The previous transition's state, carried only when some tracked
    // antecedent contains pre(...); the empty state otherwise (and for
    // the first transition - never absent, so pre(...) evaluates
    // instead of erroring).
    let mut pre_state = State::from_claims(Vec::new());
    // Keyset-paginated read inside the one deferrable snapshot: the
    // replay is linear and streamable, so the whole log never needs to
    // sit in memory at once - only one chunk of rows does. The cursor
    // is the same (committed_at, transition_id) tuple every replay
    // path orders by.
    struct Row {
        transition_id: Uuid,
        transformation_name: String,
        asserted_claims: serde_json::Value,
        retracted_claims: serde_json::Value,
        committed_at: DateTime<Utc>,
    }
    let mut cursor: Option<(DateTime<Utc>, Uuid)> = None;
    loop {
        let rows: Vec<Row> = match &cursor {
            None => {
                sqlx::query_as!(
                    Row,
                    "SELECT transition_id, transformation_name,
                            asserted_claims, retracted_claims, committed_at
                     FROM morpholog.audit
                     ORDER BY committed_at, transition_id
                     LIMIT $1",
                    REPLAY_CHUNK,
                )
                .fetch_all(&mut *tx)
                .await
            }
            Some((after_at, after_id)) => {
                sqlx::query_as!(
                    Row,
                    "SELECT transition_id, transformation_name,
                            asserted_claims, retracted_claims, committed_at
                     FROM morpholog.audit
                     WHERE (committed_at, transition_id) > ($2, $3)
                     ORDER BY committed_at, transition_id
                     LIMIT $1",
                    REPLAY_CHUNK,
                    after_at,
                    *after_id,
                )
                .fetch_all(&mut *tx)
                .await
            }
        }
        .map_err(classify)?;
        let Some(last) = rows.last() else {
            break;
        };
        cursor = Some((last.committed_at, last.transition_id));
        let exhausted = (rows.len() as i64) < REPLAY_CHUNK;
        for row in rows {
            let Row {
                transition_id,
                transformation_name,
                asserted_claims: asserted_json,
                retracted_claims: retracted_json,
                ..
            } = row;
            let asserted: Vec<ClaimInstance> = serde_json::from_value(asserted_json)?;
            let retracted: Vec<ClaimInstance> = serde_json::from_value(retracted_json)?;
            let delta: std::collections::BTreeSet<PredicateName> = retracted
                .iter()
                .chain(asserted.iter())
                .map(|c| c.predicate.clone())
                .collect();
            // Within each transition: retractions first, then assertions -
            // the same order the kernel's candidate build uses.
            for r in &retracted {
                replay.retract(r);
            }
            for a in &asserted {
                replay.assert(a);
            }
            // A snapshot costs O(live claims); take one only when this
            // transition can fire something, or when pre(...) tracking
            // forces the previous state to stay current.
            let relevant = tracker.delta_is_relevant(&delta);
            if relevant || needs_pre {
                let post_state = replay.snapshot_state();
                tracker
                    .observe(
                        &post_state,
                        &pre_state,
                        &delta,
                        &transition_id.to_string(),
                        &transformation_name,
                    )
                    .map_err(PgError::Kernel)?;
                if needs_pre {
                    pre_state = post_state;
                }
            } else {
                // Nothing to evaluate; the states are unread, but the
                // transition still counts and the usage table grows.
                tracker
                    .observe(
                        &pre_state,
                        &pre_state,
                        &delta,
                        &transition_id.to_string(),
                        &transformation_name,
                    )
                    .map_err(PgError::Kernel)?;
            }
        }
        if exhausted {
            break;
        }
    }
    // Second pass: the rejection log, inside the same deferrable
    // snapshot so refusal counts and committed history describe one
    // consistent moment. Pure counting - no state folding - but the
    // same keyset shape, over the index made for it.
    struct RejRow {
        rejection_id: Uuid,
        transformation_name: String,
        kind: String,
        rule: String,
        rejected_at: DateTime<Utc>,
    }
    let mut rej_cursor: Option<(DateTime<Utc>, Uuid)> = None;
    loop {
        let rows: Vec<RejRow> = match &rej_cursor {
            None => {
                sqlx::query_as!(
                    RejRow,
                    "SELECT rejection_id, transformation_name, kind, rule, rejected_at
                     FROM morpholog.rejections
                     ORDER BY rejected_at, rejection_id
                     LIMIT $1",
                    REPLAY_CHUNK,
                )
                .fetch_all(&mut *tx)
                .await
            }
            Some((after_at, after_id)) => {
                sqlx::query_as!(
                    RejRow,
                    "SELECT rejection_id, transformation_name, kind, rule, rejected_at
                     FROM morpholog.rejections
                     WHERE (rejected_at, rejection_id) > ($2, $3)
                     ORDER BY rejected_at, rejection_id
                     LIMIT $1",
                    REPLAY_CHUNK,
                    after_at,
                    *after_id,
                )
                .fetch_all(&mut *tx)
                .await
            }
        }
        .map_err(classify)?;
        let Some(last) = rows.last() else {
            break;
        };
        rej_cursor = Some((last.rejected_at, last.rejection_id));
        let exhausted = (rows.len() as i64) < REPLAY_CHUNK;
        for row in rows {
            let invariant = (row.kind == REJECTION_KIND_INVARIANT).then_some(row.rule.as_str());
            tracker.observe_rejection(
                invariant,
                &row.transformation_name,
                &row.rejection_id.to_string(),
            );
        }
        if exhausted {
            break;
        }
    }
    tx.commit().await.map_err(classify)?;
    Ok(tracker.into_report())
}
/// Replay the audit log to its latest transition and compare the
/// reconstructed state against the claims table.
///
/// All reads happen inside one `REPEATABLE READ READ ONLY`
/// transaction, so the comparison is over a single database snapshot:
/// a commit landing while `verify` runs can never manufacture a false
/// divergence by appearing in one record but not the other. This is
/// the contract that makes the command safe to run against a live
/// system, not only during quiescence.
///
/// An empty database (no transitions, no claims) is trivially
/// consistent. The comparison is a multiset diff, order-insensitive:
/// replay order is causal while the claims table orders by
/// `(asserted_at, ...)`, and neither order is part of the contract.
/// The divergence buckets are sorted by `(predicate, args)` so the
/// operator-facing report is deterministic across runs.
pub async fn verify_replay(pool: &PgPool) -> Result<VerifyOutcome, PgError> {
    let mut tx = begin_isolated_tx(pool, TxIsolation::RepeatableReadReadOnly).await?;
    let latest = sqlx::query!(
        "SELECT transition_id FROM morpholog.audit
         ORDER BY committed_at DESC, transition_id DESC
         LIMIT 1",
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(classify)?;
    // count(*) is never null, but Postgres cannot prove it.
    let transitions = sqlx::query!(r#"SELECT count(*) AS "count!" FROM morpholog.audit"#)
        .fetch_one(&mut *tx)
        .await
        .map_err(classify)?
        .count;
    let replayed = match latest {
        Some(row) => reconstruct_inner(&mut tx, row.transition_id, None)
            .await?
            .claims()
            .to_vec(),
        None => Vec::new(),
    };
    // Keyset over the primary key: the multiset diff below is
    // order-insensitive, so the PK order serves where the listing
    // helpers' (asserted_at, ...) contract is not needed - and the
    // claims table never has to fit in memory beyond the diff's own
    // accumulation.
    struct ClaimRow {
        predicate_name: String,
        arguments: serde_json::Value,
    }
    let mut rows: Vec<(String, serde_json::Value)> = Vec::new();
    let mut claims_cursor: Option<(String, serde_json::Value)> = None;
    loop {
        let page: Vec<ClaimRow> = match &claims_cursor {
            None => {
                sqlx::query_as!(
                    ClaimRow,
                    "SELECT predicate_name, arguments
                     FROM morpholog.claims
                     ORDER BY predicate_name, arguments
                     LIMIT $1",
                    REPLAY_CHUNK,
                )
                .fetch_all(&mut *tx)
                .await
            }
            Some((pred, args)) => {
                sqlx::query_as!(
                    ClaimRow,
                    "SELECT predicate_name, arguments
                     FROM morpholog.claims
                     WHERE (predicate_name, arguments) > ($2, $3)
                     ORDER BY predicate_name, arguments
                     LIMIT $1",
                    REPLAY_CHUNK,
                    pred,
                    args,
                )
                .fetch_all(&mut *tx)
                .await
            }
        }
        .map_err(classify)?;
        let Some(last) = page.last() else {
            break;
        };
        claims_cursor = Some((last.predicate_name.clone(), last.arguments.clone()));
        let exhausted = (page.len() as i64) < REPLAY_CHUNK;
        rows.extend(page.into_iter().map(|r| (r.predicate_name, r.arguments)));
        if exhausted {
            break;
        }
    }
    tx.commit().await.map_err(classify)?;
    let current = decode_claim_rows(rows)?;
    // Multiset diff: +1 per current claim, -1 per replayed claim.
    // Positive residue exists only in the claims table, negative only
    // in the replay.
    let mut counts: HashMap<&ClaimInstance, i64> = HashMap::new();
    for c in &current {
        *counts.entry(c).or_default() += 1;
    }
    for c in &replayed {
        *counts.entry(c).or_default() -= 1;
    }
    let mut only_in_claims_table = Vec::new();
    let mut only_in_replay = Vec::new();
    for (claim, n) in counts {
        for _ in 0..n.abs() {
            if n > 0 {
                only_in_claims_table.push(claim.clone());
            } else if n < 0 {
                only_in_replay.push(claim.clone());
            }
        }
    }
    let sort_key = |c: &ClaimInstance| {
        (
            c.predicate.to_string(),
            serde_json::to_string(&c.args).unwrap_or_default(),
        )
    };
    only_in_claims_table.sort_by_key(sort_key);
    only_in_replay.sort_by_key(sort_key);
    if only_in_claims_table.is_empty() && only_in_replay.is_empty() {
        Ok(VerifyOutcome::Consistent {
            transitions,
            claims: current.len(),
        })
    } else {
        Ok(VerifyOutcome::Divergent {
            only_in_claims_table,
            only_in_replay,
        })
    }
}
