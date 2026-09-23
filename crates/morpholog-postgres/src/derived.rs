use crate::as_of::reconstruct_state_at_for_predicates;
use crate::claims::{decode_claim_rows, list_claims_for_predicates};
use crate::error::{PgError, classify, classify_checked_query};
use crate::txn::{TxIsolation, begin_isolated_tx};
use jiff::Timestamp;
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
/// Loads only the claims of predicates the body references (see
/// [`morpholog_core::predicates_referenced_by_derived`]) and runs
/// [`enumerate_derived`] over them. Returns one [`ClaimInstance`] per
/// distinct key binding, with each computed value appended to the keys.
///
/// Read-only, and recomputed from scratch on every call.
///
/// The scoped load is safe because the footprint analysis matches
/// exhaustively: a new predicate-reading variant fails to compile until
/// it is handled.
///
/// Errors:
/// - [`PgError::Database`] / [`PgError::Encoding`] from the claims read.
/// - [`PgError::Kernel`] if the kernel rejects the derived claim's body
///   (a type mismatch, an unbound variable): an authoring error, not a
///   data condition.
///
/// Output is sorted by the `(keys ++ computed values)` tuple, so it is
/// deterministic for a given state.
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
/// the projection reflects, and per-phase timings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshSummary {
    pub refresh_id: Uuid,
    pub model_hash: String,
    pub derived_predicate_count: usize,
    pub source_claim_count: usize,
    pub derived_claim_count: usize,
    /// The latest audit transition VISIBLE in the refresh snapshot: a
    /// rough freshness marker, not a lossless audit-resume coordinate.
    pub source_snapshot_transition_id: Option<Uuid>,
    pub source_snapshot_committed_at: Option<Timestamp>,
    pub read: Duration,
    pub compute: Duration,
    pub write: Duration,
}
/// Recompute every derived claim with the kernel and publish a new
/// generation of the `morpholog_read` projection.
///
/// Rows are stored exactly as the kernel produced them, shaped like
/// `morpholog.claims`; SQL never recomputes a value. This is a read model,
/// never governed state: nothing in `propose` or evaluation reads it.
///
/// Three phases keep the kernel compute outside any transaction:
///  - **read** (one short `REPEATABLE READ` snapshot): the latest visible
///    audit transition, then the scoped claims. `source_snapshot_*` is a
///    freshness marker, NOT a lossless high-water: `committed_at` is the
///    writer's start time but visibility follows commit order, so an
///    in-flight transaction may sort earlier and is picked up next time.
///  - **compute** (no transaction): the kernel builds the rows.
///  - **write** (one short transaction): insert a new generation, load its
///    rows, flip the active pointer, drop the old generation. Readers see
///    the old generation until commit; a failure leaves it intact.
///
/// A full, single-threaded refresh: cost scales with the claims loaded,
/// the domain matches, and the rows emitted.
///
/// Takes a [`ValidatedProgram`] so an unvalidated programme cannot be
/// materialised by accident.
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
    // Read: one short snapshot for the marker and the claims, released
    // before the compute.
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
    // Compute: no transaction held.
    let compute_start = Instant::now();
    let state = State::from_claims(decode_claim_rows(claim_rows)?);
    let mut rows: Vec<ClaimInstance> = Vec::new();
    for derived in deriveds {
        rows.extend(enumerate_derived(derived, &state, definitions)?);
    }
    let compute = compute_start.elapsed();
    // Write: one short transaction, no kernel work.
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
    // One statement (UNNEST of parallel arrays), not a round-trip per row.
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
    // Flip the active pointer, then drop every other generation (cascading
    // its rows). A reader mid-query keeps its snapshot of the old one.
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
        source_snapshot_committed_at: snapshot_at.map(Into::into),
        read,
        compute,
        write,
    })
}
/// Enumerate a derived claim's extension against the state that
/// existed immediately after `transition_id` committed.
///
/// [`list_derived`] over historical state: the audit log is replayed up to
/// `transition_id`, keeping only the footprint's predicates. Output is
/// byte-identical to what [`list_derived`] returned at that moment. An
/// unknown id is [`PgError::TransitionNotFound`], never current state.
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
