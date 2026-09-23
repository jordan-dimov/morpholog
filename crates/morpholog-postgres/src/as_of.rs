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
/// Replays every audit row up to and including the target's
/// `(committed_at, transition_id)`, in causal order. Within a
/// transition, retractions apply before assertions, as in the kernel.
/// Asserting a claim already present is a no-op, as on commit.
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
/// Like [`reconstruct_state_at`] but keeps only claims whose predicate
/// is in `predicates`: the as-of analogue of
/// [`crate::list_claims_for_predicates`].
///
/// The resulting [`State`] is **partial**: callers must not query
/// predicates outside the set, which would wrongly match nothing.
///
/// Empty `predicates` returns an empty `State`, but the target must still
/// exist (otherwise [`PgError::TransitionNotFound`]).
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
/// `transition_id`: the historical counterpart of
/// [`crate::list_claims_for_predicates`], replaying only those predicates.
///
/// Empty `predicates` still checks that `transition_id` exists (else
/// [`PgError::TransitionNotFound`]), then returns no claims. Unknown
/// predicate names match nothing; no programme vocabulary is consulted.
pub async fn list_claims_at_for_predicates(
    pool: &PgPool,
    transition_id: Uuid,
    predicates: &[String],
) -> Result<Vec<ClaimInstance>, PgError> {
    let state = reconstruct_state_at_for_predicates(pool, transition_id, predicates).await?;
    Ok(state.claims().to_vec())
}
/// Resolve a wall-clock instant to the last transition committed at or
/// before it.
///
/// Uses the replay's total order `(committed_at, transition_id)`, so the
/// answer is exact even when transitions share a `committed_at`. A
/// timestamp earlier than every transition is
/// [`PgError::NoTransitionAtOrBefore`].
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
/// Shared replay behind [`reconstruct_state_at`] (`predicates` is `None`)
/// and [`reconstruct_state_at_for_predicates`]. Out-of-scope claims are
/// skipped during replay, never held in memory.
pub(crate) async fn reconstruct_inner(
    conn: &mut sqlx::PgConnection,
    transition_id: Uuid,
    predicates: Option<&[String]>,
) -> Result<State, PgError> {
    // A missing target is TransitionNotFound.
    let target = audit_cursor_for(&mut *conn, transition_id).await?;
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
                    claims.retain(|c| scope.contains(c.predicate.as_str()));
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
