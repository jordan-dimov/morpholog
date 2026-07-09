use crate::audit::{REPLAY_CHUNK, audit_cursor_for};
use crate::error::{PgError, classify};
use chrono::{DateTime, Utc};
use morpholog_core::{ClaimInstance, State};
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};
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
/// assertions - matching the kernel's `build_candidate_state`
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
        .map_err(classify)?;
        target.ok_or(PgError::TransitionNotFound(transition_id))?;
        return Ok(State::default());
    }
    let mut conn = pool.acquire().await.map_err(classify)?;
    reconstruct_inner(&mut conn, transition_id, Some(predicates)).await
}
/// Returns the claims admitted as of `transition_id`, in causal
/// first-asserted order (the replay loop's construction order).
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
    at: DateTime<Utc>,
) -> Result<Uuid, PgError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let row = sqlx::query!(
        "SELECT transition_id FROM morpholog.audit
         WHERE committed_at <= $1
         ORDER BY committed_at DESC, transition_id DESC
         LIMIT 1",
        at,
    )
    .fetch_optional(executor)
    .await
    .map_err(classify)?;
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
    let (target_committed_at, target_transition_id) =
        audit_cursor_for(&mut *conn, transition_id).await?;
    // Precompute the scope as a HashSet so each in-loop membership
    // check is O(1) regardless of footprint size or audit-log length.
    let scope_set: Option<HashSet<&str>> =
        predicates.map(|preds| preds.iter().map(String::as_str).collect());
    // Replay every transition with a `(committed_at, transition_id)`
    // tuple less than or equal to the target's. PostgreSQL row
    // comparison (`(a, b) <= (c, d)`) is lexicographic; ordering by
    // the same two columns guarantees a deterministic replay. Keyset
    // pages inside the caller's snapshot keep memory at one chunk
    // regardless of log length - the same shape as every other
    // replay-order read.
    struct Row {
        transition_id: Uuid,
        asserted_claims: serde_json::Value,
        retracted_claims: serde_json::Value,
        committed_at: DateTime<Utc>,
    }
    let mut replay = ReplaySet::new();
    let mut cursor: Option<(DateTime<Utc>, Uuid)> = None;
    loop {
        let rows: Vec<Row> = match &cursor {
            None => {
                sqlx::query_as!(
                    Row,
                    "SELECT transition_id, asserted_claims, retracted_claims, committed_at
                     FROM morpholog.audit
                     WHERE (committed_at, transition_id) <= ($1, $2)
                     ORDER BY committed_at, transition_id
                     LIMIT $3",
                    target_committed_at,
                    target_transition_id,
                    REPLAY_CHUNK,
                )
                .fetch_all(&mut *conn)
                .await
            }
            Some((after_at, after_id)) => {
                sqlx::query_as!(
                    Row,
                    "SELECT transition_id, asserted_claims, retracted_claims, committed_at
                     FROM morpholog.audit
                     WHERE (committed_at, transition_id) <= ($1, $2)
                       AND (committed_at, transition_id) > ($4, $5)
                     ORDER BY committed_at, transition_id
                     LIMIT $3",
                    target_committed_at,
                    target_transition_id,
                    REPLAY_CHUNK,
                    after_at,
                    *after_id,
                )
                .fetch_all(&mut *conn)
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
            let asserted: Vec<ClaimInstance> = serde_json::from_value(row.asserted_claims)?;
            let retracted: Vec<ClaimInstance> = serde_json::from_value(row.retracted_claims)?;
            // Within each transition: retractions first, then
            // assertions. Matches build_candidate_state in the kernel.
            for r in &retracted {
                if !predicate_in_scope_set(r.predicate.as_str(), scope_set.as_ref()) {
                    continue;
                }
                replay.retract(r);
            }
            for a in &asserted {
                if !predicate_in_scope_set(a.predicate.as_str(), scope_set.as_ref()) {
                    continue;
                }
                replay.assert(a);
            }
        }
        if exhausted {
            break;
        }
    }
    Ok(replay.into_state())
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
/// Working state for audit-log replay. Keeps claims in first-asserted
/// order (the contract `list_claims_at` documents) while making both
/// `assert` and `retract` O(1) amortised - a plain `Vec` with linear
/// dedupe and `retain` would be O(N^2) over a full replay.
///
/// Internals:
/// - `claims` holds every claim ever asserted during this replay, in
///   first-asserted order. Never shrinks; compacted once at the end
///   via [`into_state`].
/// - `index` maps `claim -> position in claims`, used by both `assert`
///   (re-assertion detection) and `retract` (entry to mark dead).
/// - `live[i]` is `true` iff `claims[i]` is currently asserted.
///   Retraction flips it `false`; re-assertion flips it back.
pub(crate) struct ReplaySet {
    claims: Vec<ClaimInstance>,
    index: HashMap<ClaimInstance, usize>,
    live: Vec<bool>,
}
impl ReplaySet {
    pub(crate) fn new() -> Self {
        Self {
            claims: Vec::new(),
            index: HashMap::new(),
            live: Vec::new(),
        }
    }
    /// Assert a claim. First-time claims are appended and marked live;
    /// a previously-seen claim (live or retracted) has its existing
    /// slot flipped back to live. Re-asserting an already-live claim is
    /// a no-op, matching the set semantics the kernel pins.
    ///
    /// Re-assertion is zero-clone (only the `live` bit changes).
    /// First-time assertion is two clones - one for the `claims` Vec,
    /// one for the owned `index` key - the cost of keeping `claims`
    /// contiguous. The clone is cheap relative to the JSON decode that
    /// produced the input.
    pub(crate) fn assert(&mut self, claim: &ClaimInstance) {
        if let Some(&i) = self.index.get(claim) {
            self.live[i] = true;
        } else {
            let i = self.claims.len();
            self.claims.push(claim.clone());
            self.live.push(true);
            self.index.insert(claim.clone(), i);
        }
    }
    /// Retract a claim by marking its `live` slot `false`. If the
    /// claim was never asserted in this replay, the call is a no-op
    /// (matches the kernel's `Stmt::Retract` semantics: retracting
    /// a non-existent claim is an idempotent no-op).
    pub(crate) fn retract(&mut self, claim: &ClaimInstance) {
        if let Some(&i) = self.index.get(claim) {
            self.live[i] = false;
        }
    }
    /// Compact into a `State` containing only the live claims, in
    /// their first-asserted order. Runs once at the end of replay;
    /// O(|all-ever-asserted|).
    fn into_state(self) -> State {
        let claims: Vec<ClaimInstance> = self
            .claims
            .into_iter()
            .zip(self.live)
            .filter_map(|(c, alive)| alive.then_some(c))
            .collect();
        State::from_claims(claims)
    }
    /// A `State` of the currently-live claims without consuming the
    /// replay - the per-step snapshot coverage needs. O(live claims)
    /// per call, which is why [`coverage_replay`] only calls it when
    /// a transition's delta actually touches a tracked antecedent.
    pub(crate) fn snapshot_state(&self) -> State {
        let claims: Vec<ClaimInstance> = self
            .claims
            .iter()
            .zip(&self.live)
            .filter(|&(_, &alive)| alive)
            .map(|(c, _)| c.clone())
            .collect();
        State::from_claims(claims)
    }
}
