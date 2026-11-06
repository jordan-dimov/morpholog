//! The admitted state at settled places in the audit log, kept so that an
//! as-of read replays only the rows after the nearest one. A cache built
//! on request: no commit writes it, nothing that verifies the record
//! reads it, and a read that finds none, or one it cannot use, replays
//! the whole log.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU64;

use jiff_sqlx::ToSqlx;
use morpholog_core::{ClaimInstance, EvalValue, State};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

use crate::as_of::{reconstruct_inner, settled_target};
use crate::audit::AuditPosition;
use crate::error::{PgError, classify, classify_checked_query};

/// How a snapshot's claims encode the state. A read ignores any other.
const FORMAT: i32 = 1;

/// What [`refresh_state_snapshots`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SnapshotRefresh {
    /// The interval kept: one snapshot after every this many rows of the
    /// log, in canonical order.
    pub every: u64,
    /// Snapshots on that grid that were already kept.
    pub kept: u64,
    /// Snapshots on the grid built now.
    pub built: u64,
    /// Snapshots off the grid, or in another format, deleted.
    pub removed: u64,
    /// The latest snapshot kept after the refresh, when there is one.
    pub latest: Option<Uuid>,
}

/// Keep one snapshot after every `every` rows of the log, in canonical
/// order, and no others. Missing ones are built in order, each from the
/// one before it, so a refresh costs one replay of the rows since the
/// last; it stops at the first place not yet settled, and skips a place
/// inside a baseline. Snapshots off that grid, from an earlier interval,
/// or in another format are deleted first. A read never needs any of
/// them: with none it replays the log.
pub async fn refresh_state_snapshots(
    pool: &PgPool,
    every: NonZeroU64,
) -> Result<SnapshotRefresh, PgError> {
    let every = every.get();
    let places: Vec<Uuid> = sqlx::query_scalar!(
        r#"SELECT transition_id AS "transition_id!" FROM (
               SELECT transition_id, row_number() OVER (
                   ORDER BY audit_order, act_ordinal, committed_at, transition_id) AS i
               FROM morpholog.audit) a
           WHERE i % $1 = 0
           ORDER BY i"#,
        i64::try_from(every).unwrap_or(i64::MAX),
    )
    .fetch_all(pool)
    .await
    .map_err(classify_checked_query)?;
    let removed = sqlx::query!(
        "DELETE FROM morpholog.state_snapshots
         WHERE format <> $1 OR NOT (transition_id = ANY($2))",
        FORMAT,
        &places,
    )
    .execute(pool)
    .await
    .map_err(classify_checked_query)?
    .rows_affected();
    let present: HashSet<Uuid> = sqlx::query_scalar!(
        "SELECT transition_id FROM morpholog.state_snapshots WHERE format = $1",
        FORMAT,
    )
    .fetch_all(pool)
    .await
    .map_err(classify_checked_query)?
    .into_iter()
    .collect();
    let (mut kept, mut built, mut latest) = (0, 0, None);
    for place in places {
        if present.contains(&place) {
            kept += 1;
            latest = Some(place);
            continue;
        }
        match build_state_snapshot(pool, place).await {
            Ok(done) => {
                if done.already_kept {
                    kept += 1;
                } else {
                    built += 1;
                }
                latest = Some(place);
            }
            Err(PgError::TransitionNotSettled(_)) => break,
            Err(PgError::BaselineChunk { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(SnapshotRefresh {
        every,
        kept,
        built,
        removed,
        latest,
    })
}

/// What [`build_state_snapshot`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SnapshotBuilt {
    /// The transition the state is as of.
    pub transition_id: Uuid,
    /// The live claims it holds.
    pub claims: u64,
    /// A snapshot of this place was already kept, and nothing was written.
    pub already_kept: bool,
}

/// Keep the admitted state as of `transition_id`, for as-of reads at or
/// after it. The state is the replay's own, from the nearest earlier
/// snapshot, so a series of them costs one pass over the log. Refused
/// like an as-of read at the same place: while it is not settled, and at
/// a baseline chunk.
pub(crate) async fn build_state_snapshot(
    pool: &PgPool,
    transition_id: Uuid,
) -> Result<SnapshotBuilt, PgError> {
    let mut conn = pool.acquire().await.map_err(classify)?;
    let position = settled_target(&mut conn, transition_id).await?;
    let mut asserted_in = HashMap::new();
    let state =
        reconstruct_inner(&mut conn, transition_id, None, Some(&mut asserted_in), None).await?;
    drop(conn);

    let claims = state.claims();
    let count = claims.len();
    let mut ordinals = Vec::with_capacity(count);
    let mut predicates = Vec::with_capacity(count);
    let mut arguments = Vec::with_capacity(count);
    let mut admitted = Vec::with_capacity(count);
    for (ordinal, claim) in claims.iter().enumerate() {
        ordinals.push(i64::try_from(ordinal).unwrap_or(i64::MAX));
        predicates.push(claim.predicate.as_str().to_string());
        arguments.push(serde_json::to_value(&claim.args)?);
        admitted.push(asserted_in[claim]);
    }
    let count = i64::try_from(count).unwrap_or(i64::MAX);

    let mut tx = pool.begin().await.map_err(classify)?;
    let id = sqlx::query_scalar!(
        "INSERT INTO morpholog.state_snapshots
             (audit_order, act_ordinal, committed_at, transition_id, format, claim_count)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT DO NOTHING
         RETURNING snapshot_id",
        position.audit_order.cast_signed(),
        position.act_ordinal.cast_signed(),
        position.committed_at.to_sqlx(),
        position.transition_id,
        FORMAT,
        count,
    )
    .fetch_optional(&mut *tx)
    .await
    .map_err(classify_checked_query)?;
    let Some(id) = id else {
        return Ok(SnapshotBuilt {
            transition_id,
            claims: count.cast_unsigned(),
            already_kept: true,
        });
    };
    sqlx::query!(
        "INSERT INTO morpholog.state_snapshot_claims
             (snapshot_id, ordinal, predicate_name, arguments, asserted_in)
         SELECT $1, * FROM unnest($2::bigint[], $3::text[], $4::jsonb[], $5::uuid[])",
        id,
        &ordinals,
        &predicates,
        &arguments,
        &admitted,
    )
    .execute(&mut *tx)
    .await
    .map_err(classify_checked_query)?;
    tx.commit().await.map_err(classify)?;
    Ok(SnapshotBuilt {
        transition_id,
        claims: count.cast_unsigned(),
        already_kept: false,
    })
}

/// A kept state and the place it is as of.
pub(crate) struct Snapshot {
    pub(crate) position: AuditPosition,
    pub(crate) state: State,
    pub(crate) asserted_in: HashMap<ClaimInstance, Uuid>,
}

/// The nearest snapshot at or before `target`, holding only the claims in
/// `scope` when one is given. `None` when there is none, or the one found
/// does not decode or holds fewer claims than it says: the caller replays
/// from the start.
pub(crate) async fn nearest(
    conn: &mut sqlx::PgConnection,
    target: AuditPosition,
    scope: Option<&HashSet<&str>>,
) -> Result<Option<Snapshot>, PgError> {
    let Some(head) = sqlx::query!(
        r#"SELECT snapshot_id, audit_order, act_ordinal, committed_at, transition_id, claim_count
           FROM morpholog.state_snapshots
           WHERE format = $5
             AND (audit_order, act_ordinal, committed_at, transition_id) <= ($1, $2, $3, $4)
           ORDER BY audit_order DESC, act_ordinal DESC, committed_at DESC, transition_id DESC
           LIMIT 1"#,
        target.audit_order.cast_signed(),
        target.act_ordinal.cast_signed(),
        target.committed_at.to_sqlx(),
        target.transition_id,
        FORMAT,
    )
    .fetch_optional(&mut *conn)
    .await
    .map_err(classify_checked_query)?
    else {
        return Ok(None);
    };
    let scoped: Option<Vec<String>> = scope.map(|s| s.iter().map(|p| (*p).to_string()).collect());
    let rows = sqlx::query!(
        r#"SELECT predicate_name, arguments, asserted_in
           FROM morpholog.state_snapshot_claims
           WHERE snapshot_id = $1 AND ($2::text[] IS NULL OR predicate_name = ANY($2))
           ORDER BY ordinal"#,
        head.snapshot_id,
        scoped.as_deref(),
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(classify_checked_query)?;
    if scope.is_none() && i64::try_from(rows.len()).ok() != Some(head.claim_count) {
        return Ok(None);
    }
    let mut claims = Vec::with_capacity(rows.len());
    let mut asserted_in = HashMap::with_capacity(rows.len());
    for row in rows {
        let Ok(args) = serde_json::from_value::<Vec<EvalValue>>(row.arguments) else {
            return Ok(None);
        };
        let claim = ClaimInstance {
            predicate: row.predicate_name.into(),
            args,
        };
        asserted_in.insert(claim.clone(), row.asserted_in);
        claims.push(claim);
    }
    Ok(Some(Snapshot {
        position: AuditPosition {
            audit_order: head.audit_order.cast_unsigned(),
            act_ordinal: head.act_ordinal.cast_unsigned(),
            committed_at: head.committed_at.into(),
            transition_id: head.transition_id,
        },
        state: State::from_claims(claims),
        asserted_in,
    }))
}
