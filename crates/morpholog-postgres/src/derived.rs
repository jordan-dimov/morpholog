use crate::as_of::reconstruct_state_at_for_predicates;
use crate::claims::{decode_claim_rows, list_claims_for_predicates};
use crate::error::{PgError, classify, classify_checked_query};
use crate::txn::{TxIsolation, begin_isolated_tx};
use chrono::{DateTime, Utc};
use morpholog_core::{
    ClaimInstance, Definition, DerivedClaim, State, ValidatedProgram, enumerate_derived,
    predicates_referenced_by_derived,
};
use sqlx::PgPool;
use std::collections::BTreeSet;
use std::time::{Duration, Instant};
use uuid::Uuid;
/// Enumerate a derived claim's extension against the current durable state.
///
/// Loads only the admitted claims for predicates the derived claim's
/// body references (via [`list_claims_for_predicates`] and
/// [`morpholog_core::predicates_referenced_by_derived`]), wraps them
/// in an in-memory [`State`], and calls the synchronous
/// [`enumerate_derived`] kernel primitive. The result is a
/// [`ClaimInstance`] per distinct key binding the `domain` produces,
/// with each `DerivedValue` evaluated and appended to the key
/// positions.
///
/// Read-only: no claims written, no audit row, no outbox row. Repeated
/// calls recompute from scratch; there is no materialised view.
///
/// The predicate-scoped load is safe because the footprint analysis's
/// exhaustive `match` fails to compile if a new predicate-referencing
/// `Prop` or `ValueExpr` variant is added without handling it, so this
/// read path cannot silently produce wrong answers under a partial state.
///
/// Errors:
/// - [`PgError::Database`] / [`PgError::Encoding`] from the underlying
///   `list_claims_for_predicates` call.
/// - [`PgError::Kernel`] if the kernel rejects the derived claim's body
///   (type mismatch in a `DerivedValue.expr`, unbound variable in
///   `domain`, etc.). Each is a programmer error in the derived claim's
///   definition, not a runtime data condition.
///
/// Output ordering matches the kernel's contract: sorted by the
/// concatenated `(keys ++ computed values)` tuple under structural
/// `EvalValue` ordering, so results are deterministic across runs for a
/// given state.
pub async fn list_derived(
    pool: &PgPool,
    derived: &DerivedClaim,
    definitions: &[Definition],
) -> Result<Vec<ClaimInstance>, PgError> {
    let footprint: Vec<String> = predicates_referenced_by_derived(derived, definitions)
        .into_iter()
        .map(|p| p.to_string())
        .collect();
    let claims = list_claims_for_predicates(pool, &footprint).await?;
    let state = State::from_claims(claims);
    let rows = enumerate_derived(derived, &state, definitions)?;
    Ok(rows)
}
/// The outcome of [`refresh_derived`]: what was written, the audit point
/// the projection reflects, and per-phase timings. Surfaced by
/// `morpholog refresh derived` so an operator sees the cost of a refresh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshSummary {
    pub refresh_id: Uuid,
    pub model_hash: String,
    pub derived_predicate_count: usize,
    pub source_claim_count: usize,
    pub derived_claim_count: usize,
    /// The latest audit transition VISIBLE in the refresh snapshot - a
    /// coarse freshness marker, not a lossless audit-resume coordinate
    /// (see the field doc on `morpholog_read.derived_refreshes`).
    pub source_snapshot_transition_id: Option<Uuid>,
    pub source_snapshot_committed_at: Option<DateTime<Utc>>,
    pub read: Duration,
    pub compute: Duration,
    pub write: Duration,
}
/// Recompute every derived claim with the kernel and publish a new
/// generation of the `morpholog_read` projection. The exact
/// `enumerate_derived` output is stored as tagged-JSONB rows, byte-shaped
/// like `morpholog.claims` - SQL never recomputes a derived value, it
/// only stores what the kernel produced. A read model, never governed
/// state: nothing in `propose`, invariant evaluation, or value lookups
/// reads `morpholog_read`.
///
/// Three phases keep the long part (the kernel compute) outside any
/// transaction:
///  - **read** (short `REPEATABLE READ` snapshot): the latest visible
///    audit transition then the scoped claims, in one snapshot. The
///    recorded `source_snapshot_*` is a freshness marker, NOT a lossless
///    high-water: `audit.committed_at` is transaction-start time while
///    visibility follows commit order, so a transaction in flight at
///    snapshot time (whose committed_at may sort earlier) is excluded and
///    folded in by the next refresh. Lossless resume is `inspect audit`'s
///    job; this is a discardable cache.
///  - **compute** (no transaction open): the sync kernel builds the rows.
///  - **write** (one short transaction): insert a new generation
///    (`refresh_id`), bulk-load its rows, flip the single-row active
///    pointer, and drop the prior generation. Readers stay on the prior
///    generation until this commits; a failure rolls back, leaving it
///    intact.
///
/// Full refresh, single-threaded: cost scales with the loaded claims,
/// intermediate domain matches, and emitted rows. Good for operational
/// stores; incremental and partitioned refresh are deliberately deferred.
///
/// Takes a [`ValidatedProgram`] so a read contract cannot be materialised
/// for an unvalidated programme by accident, mirroring `render_views`.
pub async fn refresh_derived(
    pool: &PgPool,
    program: ValidatedProgram<'_>,
    model_hash: &str,
) -> Result<RefreshSummary, PgError> {
    let program = program.as_program();
    let definitions = &program.definitions;
    let deriveds = &program.derived_claims;
    let footprint: Vec<String> = deriveds
        .iter()
        .flat_map(|d| predicates_referenced_by_derived(d, definitions))
        .map(|p| p.to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    // Phase: read. One short REPEATABLE READ snapshot - the latest visible
    // audit transition and the scoped claims share it - then released
    // before the compute. The marker reflects snapshot visibility, not a
    // lossless audit order (see the function doc).
    let read_start = Instant::now();
    let mut read_tx = begin_isolated_tx(pool, TxIsolation::RepeatableRead).await?;
    let latest_visible = sqlx::query!(
        "SELECT transition_id, committed_at FROM morpholog.audit
         ORDER BY committed_at DESC, transition_id DESC LIMIT 1",
    )
    .fetch_optional(&mut *read_tx)
    .await
    .map_err(classify_checked_query)?;
    let claim_rows: Vec<(String, serde_json::Value)> = if footprint.is_empty() {
        Vec::new()
    } else {
        sqlx::query!(
            "SELECT predicate_name, arguments FROM morpholog.claims
             WHERE predicate_name = ANY($1)",
            &footprint[..],
        )
        .fetch_all(&mut *read_tx)
        .await
        .map_err(classify_checked_query)?
        .into_iter()
        .map(|r| (r.predicate_name, r.arguments))
        .collect()
    };
    read_tx.commit().await.map_err(classify)?;
    let snapshot_tid = latest_visible.as_ref().map(|r| r.transition_id);
    let snapshot_at = latest_visible.map(|r| r.committed_at);
    let source_claim_count = claim_rows.len();
    let read = read_start.elapsed();
    // Phase: compute. The sync kernel - the sole evaluator - runs with no
    // transaction held.
    let compute_start = Instant::now();
    let state = State::from_claims(decode_claim_rows(claim_rows)?);
    let mut rows: Vec<ClaimInstance> = Vec::new();
    for derived in deriveds {
        rows.extend(enumerate_derived(derived, &state, definitions)?);
    }
    let compute = compute_start.elapsed();
    // Phase: write. Build the new generation, flip the active pointer, drop
    // the old generation - one short transaction, no kernel work.
    let write_start = Instant::now();
    let refresh_id = Uuid::now_v7();
    let mut tx = pool.begin().await.map_err(classify)?;
    sqlx::query!(
        "INSERT INTO morpholog_read.derived_refreshes
            (refresh_id, model_hash, refreshed_at,
             source_snapshot_transition_id, source_snapshot_committed_at,
             derived_claim_count)
         VALUES ($1, $2, now(), $3, $4, $5)",
        refresh_id,
        model_hash,
        snapshot_tid,
        snapshot_at,
        rows.len() as i64,
    )
    .execute(&mut *tx)
    .await
    .map_err(classify_checked_query)?;
    // Bulk insert in one statement (UNNEST of parallel arrays) rather than
    // a round-trip per row. COPY is the next step if a profile demands it.
    if !rows.is_empty() {
        let predicates: Vec<String> = rows.iter().map(|r| r.predicate.to_string()).collect();
        let arguments: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| serde_json::to_value(&r.args))
            .collect::<Result<_, _>>()?;
        sqlx::query!(
            "INSERT INTO morpholog_read.derived_claims (refresh_id, predicate_name, arguments)
             SELECT $1, p, a FROM UNNEST($2::text[], $3::jsonb[]) AS t(p, a)",
            refresh_id,
            &predicates,
            &arguments,
        )
        .execute(&mut *tx)
        .await
        .map_err(classify_checked_query)?;
    }
    // Flip the single-row active pointer, then drop every other generation
    // (cascading its rows). The just-published generation is now the only
    // one. Safe under MVCC: a reader mid-query keeps its snapshot of the
    // old generation.
    sqlx::query!(
        "INSERT INTO morpholog_read.derived_active (singleton, refresh_id)
         VALUES (true, $1)
         ON CONFLICT (singleton) DO UPDATE SET refresh_id = EXCLUDED.refresh_id",
        refresh_id,
    )
    .execute(&mut *tx)
    .await
    .map_err(classify_checked_query)?;
    sqlx::query!(
        "DELETE FROM morpholog_read.derived_refreshes WHERE refresh_id <> $1",
        refresh_id,
    )
    .execute(&mut *tx)
    .await
    .map_err(classify_checked_query)?;
    tx.commit().await.map_err(classify)?;
    let write = write_start.elapsed();
    Ok(RefreshSummary {
        refresh_id,
        model_hash: model_hash.to_string(),
        derived_predicate_count: deriveds.len(),
        source_claim_count,
        derived_claim_count: rows.len(),
        source_snapshot_transition_id: snapshot_tid,
        source_snapshot_committed_at: snapshot_at,
        read,
        compute,
        write,
    })
}
// ===========================================================================
// As-of helpers - audit-log replay to recover historical state
// ===========================================================================
//
// These helpers reconstruct the `State` that existed immediately after a
// chosen `transition_id` committed, by replaying every audit row up to and
// including that transition in causal order. The kernel is unchanged;
// as-of evaluation is just a question of which `State` you hand it.
//
// The coordinate is "as of *this actual committed transition*". An
// unknown id - smaller, larger, or between known ids - is rejected with
// `PgError::TransitionNotFound`; there is no fallback to current state.
//
// Replay is O(transitions up to T); full replay, no materialisation.
/// Enumerate a derived claim's extension against the state that
/// existed immediately after `transition_id` committed.
///
/// Mirrors [`list_derived`] but against historical state: the
/// derived claim's predicate footprint is computed via
/// [`morpholog_core::predicates_referenced_by_derived`], the audit
/// log is replayed up to `transition_id` keeping only claims of
/// those predicates, and `enumerate_derived` runs against the
/// resulting partial state.
///
/// Output is byte-identical to what [`list_derived`] would have
/// returned at the moment `transition_id` committed.
pub async fn list_derived_at(
    pool: &PgPool,
    derived: &DerivedClaim,
    definitions: &[Definition],
    transition_id: Uuid,
) -> Result<Vec<ClaimInstance>, PgError> {
    let footprint: Vec<String> = predicates_referenced_by_derived(derived, definitions)
        .into_iter()
        .map(|p| p.to_string())
        .collect();
    let state = reconstruct_state_at_for_predicates(pool, transition_id, &footprint).await?;
    let rows = enumerate_derived(derived, &state, definitions)?;
    Ok(rows)
}
