use crate::audit::audit_cursor_for;
use crate::audit_pages::ReplayPages;
use crate::error::{PgError, classify, classify_checked_query};
use jiff::Timestamp;
use jiff_sqlx::ToSqlx;
use morpholog_core::{ClaimInstance, State};
use sqlx::PgPool;
use std::collections::HashSet;
use uuid::Uuid;
/// Reconstruct the full [`State`] that existed immediately after
/// `transition_id` committed.
///
/// Two queries: first looks up the target audit row to obtain its
/// `(committed_at, transition_id)` pair (and to verify the
/// transition exists); second replays every audit row whose
/// `(committed_at, transition_id)` tuple is less than or equal to
/// the target's tuple, in causal order.
///
/// Within each replayed transition, retractions are applied before
/// assertions - matching the kernel's `State::with_delta`
/// semantics. Assertions are set-valued: asserting an already-present
/// claim is an idempotent no-op (matches the PG adapter's
/// `INSERT ... ON CONFLICT DO NOTHING` on commit).
///
/// Errors:
/// - [`PgError::TransitionNotFound`] if `transition_id` does not name
///   an existing audit row.
/// - [`PgError::Database`] / [`PgError::Encoding`] from the underlying
///   queries.
///
/// Replay cost is O(transitions up to T).
pub async fn reconstruct_state_at(pool: &PgPool, transition_id: Uuid) -> Result<State, PgError> {
    let mut conn = pool.acquire().await.map_err(classify)?;
    reconstruct_inner(&mut conn, transition_id, None).await
}
/// Like [`reconstruct_state_at`] but only retains claims whose
/// predicate is in `predicates`. Used internally by
/// [`list_derived_at`] to load only the predicates the derived
/// claim's body references - the as-of analogue of
/// [`crate::list_claims_for_predicates`].
///
/// Unlike the public [`reconstruct_state_at`], the resulting [`State`]
/// is **partial**: callers must not query predicates outside the
/// supplied set, since the kernel would report zero matches because
/// those claims were never added, not because they do not exist.
///
/// Empty `predicates` short-circuits to an empty `State`, but the
/// target `transition_id` must still exist (otherwise
/// [`PgError::TransitionNotFound`]). Mirrors
/// [`crate::list_claims_for_predicates`].
pub(crate) async fn reconstruct_state_at_for_predicates(
    pool: &PgPool,
    transition_id: Uuid,
    predicates: &[String],
) -> Result<State, PgError> {
    if predicates.is_empty() {
        // The "as of this committed transition" contract still
        // requires the target to exist, even with an empty footprint.
        let target = sqlx::query!(
            "SELECT transition_id FROM morpholog.audit WHERE transition_id = $1",
            transition_id,
        )
        .fetch_optional(pool)
        .await
        .map_err(classify_checked_query)?;
        target.ok_or(PgError::TransitionNotFound(transition_id))?;
        return Ok(State::default());
    }
    let mut conn = pool.acquire().await.map_err(classify)?;
    reconstruct_inner(&mut conn, transition_id, Some(predicates)).await
}
/// Returns the claims admitted as of `transition_id`, in audit replay
/// order: live claims keep the order the replay first admitted them,
/// and a claim retracted and re-admitted moves to the tail.
/// Differs from [`crate::list_claims`] in two ways: the state is historical,
/// and the ordering is replay causality rather than `(asserted_at,
/// predicate_name, args)`.
///
/// Errors propagate from [`reconstruct_state_at`].
pub async fn list_claims_at(
    pool: &PgPool,
    transition_id: Uuid,
) -> Result<Vec<ClaimInstance>, PgError> {
    let state = reconstruct_state_at(pool, transition_id).await?;
    Ok(state.claims().to_vec())
}
/// Returns the claims of the given predicates admitted as of
/// `transition_id` - the historical counterpart of
/// [`crate::list_claims_for_predicates`], replaying only the named
/// predicates rather than reconstructing the full state and
/// filtering after.
///
/// Empty `predicates` still validates that `transition_id` exists
/// (an unknown id is [`PgError::TransitionNotFound`], same as the
/// unscoped read) and then returns no claims: an empty footprint is
/// meaningful, not an error. Unknown predicate names simply match
/// nothing - the claims table is the authority here, not any
/// programme's declared vocabulary.
pub async fn list_claims_at_for_predicates(
    pool: &PgPool,
    transition_id: Uuid,
    predicates: &[String],
) -> Result<Vec<ClaimInstance>, PgError> {
    let state = reconstruct_state_at_for_predicates(pool, transition_id, predicates).await?;
    Ok(state.claims().to_vec())
}
/// Resolve a wall-clock instant to the last transition committed at or
/// before it - the timestamp form of an as-of coordinate. Uses the
/// `(committed_at, transition_id)` ordering, the same total order the
/// replay helpers use, so the answer is exact even when several
/// transitions share a `committed_at` under concurrent commits.
///
/// A timestamp earlier than every committed transition is
/// [`PgError::NoTransitionAtOrBefore`]: there is no state to
/// reconstruct at or before that instant.
pub async fn resolve_transition_at_or_before<'e, E>(
    executor: E,
    at: Timestamp,
) -> Result<Uuid, PgError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let row = sqlx::query!(
        "SELECT transition_id FROM morpholog.audit
         WHERE committed_at <= $1
         ORDER BY committed_at DESC, transition_id DESC
         LIMIT 1",
        at.to_sqlx(),
    )
    .fetch_optional(executor)
    .await
    .map_err(classify_checked_query)?;
    row.map(|r| r.transition_id)
        .ok_or(PgError::NoTransitionAtOrBefore(at))
}
/// Shared implementation behind [`reconstruct_state_at`] (full state)
/// and [`reconstruct_state_at_for_predicates`] (partial state). The
/// `predicates` parameter is `None` for the full case and
/// `Some(slice)` for the scoped case; the loop checks membership
/// during replay and skips both asserts and retracts whose predicate
/// is not in the set, so the scoped case never materialises
/// out-of-footprint claims in memory.
pub(crate) async fn reconstruct_inner(
    conn: &mut sqlx::PgConnection,
    transition_id: Uuid,
    predicates: Option<&[String]>,
) -> Result<State, PgError> {
    // Resolve the target transition's (committed_at, transition_id)
    // tuple. Missing target -> TransitionNotFound; this is the
    // contract that lets every other unknown id also be an error.
    let target = audit_cursor_for(&mut *conn, transition_id).await?;
    // Precompute the scope as a HashSet so each in-loop membership
    // check is O(1) regardless of footprint size or audit-log length.
    let scope_set: Option<HashSet<&str>> =
        predicates.map(|preds| preds.iter().map(String::as_str).collect());
    let mut state = State::default();
    let mut pages = ReplayPages::new(Some(target));
    loop {
        let rows = pages.next(&mut *conn).await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            let in_scope = |claims: serde_json::Value| -> Result<Vec<ClaimInstance>, PgError> {
                let mut claims: Vec<ClaimInstance> = serde_json::from_value(claims)?;
                if let Some(scope) = scope_set.as_ref() {
                    claims.retain(|c| predicate_in_scope_set(c.predicate.as_str(), Some(scope)));
                }
                Ok(claims)
            };
            let asserted = in_scope(row.asserted_claims)?;
            let retracted = in_scope(row.retracted_claims)?;
            // The kernel's own order within a transition: retractions
            // first, then assertions.
            state.apply(&asserted, &retracted);
        }
    }
    Ok(state)
}
/// Predicate-scope check. `None` (full reconstruction) accepts
/// everything; `Some(set)` accepts only predicates whose name is in
/// the set. The set is precomputed once per reconstruction in
/// [`reconstruct_inner`], so each check is O(1).
pub(crate) fn predicate_in_scope_set(predicate: &str, scope: Option<&HashSet<&str>>) -> bool {
    match scope {
        None => true,
        Some(set) => set.contains(predicate),
    }
}
