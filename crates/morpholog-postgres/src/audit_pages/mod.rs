//! Every walk over the audit log in replay order: a keyset over
//! `(committed_at, transition_id)`, one chunk in memory at a time, up to
//! a bound. The pager owns the bound, so no caller can apply a row past
//! the coordinate it asked for.
//!
//! A claims replay reads only `ReplayRow`, so it neither pays for nor
//! fails on columns it never uses. Everything needing the whole row reads
//! `AuditRow`.
//!
//! Each consumer opens the snapshot its guarantee needs and lends the
//! connection.
//!
//! Each bound combination has its own query: one query with the bounds
//! behind flags loses the index condition under a generic plan.

use jiff::Timestamp;
use jiff_sqlx::ToSqlx;
use uuid::Uuid;

use crate::audit::{AuditRow, REPLAY_CHUNK, list_audit_rows_page};
use crate::error::{PgError, classify};

/// The paging state both projections share.
#[derive(Debug)]
struct Keyset {
    cursor: Option<(Timestamp, Uuid)>,
    through: Option<(Timestamp, Uuid)>,
    chunk: i64,
    done: bool,
}

impl Keyset {
    fn new(through: Option<(Timestamp, Uuid)>, chunk: i64) -> Self {
        Keyset {
            cursor: None,
            through,
            chunk,
            done: false,
        }
    }

    /// A short page ends the walk, and so does a page ending on the
    /// `through` target, saving an empty query.
    fn advance(&mut self, len: usize, last: Option<(Timestamp, Uuid)>) {
        let Some(last) = last else {
            self.done = true;
            return;
        };
        self.cursor = Some(last);
        self.done = (len as i64) < self.chunk || self.through == Some(last);
    }
}

/// The columns a claims replay folds: the claim arrays and the
/// coordinates, plus the transformation name coverage attributes by.
pub(crate) struct ReplayRow {
    pub(crate) transition_id: Uuid,
    pub(crate) transformation_name: String,
    pub(crate) asserted_claims: serde_json::Value,
    pub(crate) retracted_claims: serde_json::Value,
    pub(crate) committed_at: Timestamp,
}

/// A walk that reads `ReplayRow`s, to the end of the snapshot or through
/// one transition inclusive.
#[derive(Debug)]
pub(crate) struct ReplayPages(Keyset);

impl ReplayPages {
    pub(crate) fn new(through: Option<(Timestamp, Uuid)>) -> Self {
        Self::with_chunk(through, REPLAY_CHUNK)
    }

    pub(crate) fn with_chunk(through: Option<(Timestamp, Uuid)>, chunk: i64) -> Self {
        ReplayPages(Keyset::new(through, chunk))
    }

    /// The next chunk, in replay order; empty once the walk is done, and
    /// from then on without asking the database again.
    pub(crate) async fn next(
        &mut self,
        conn: &mut sqlx::PgConnection,
    ) -> Result<Vec<ReplayRow>, PgError> {
        if self.0.done {
            return Ok(Vec::new());
        }
        let rows = replay_page(conn, self.0.cursor, self.0.through, self.0.chunk).await?;
        self.0.advance(
            rows.len(),
            rows.last().map(|r| (r.committed_at, r.transition_id)),
        );
        Ok(rows)
    }
}

/// A walk that reads whole `AuditRow`s, to the end of the snapshot or to
/// just before a horizon.
#[derive(Debug)]
pub(crate) struct AuditPages {
    keyset: Keyset,
    horizon: Option<Timestamp>,
}

impl AuditPages {
    pub(crate) fn new(horizon: Option<Timestamp>) -> Self {
        Self::with_chunk(horizon, REPLAY_CHUNK)
    }

    pub(crate) fn with_chunk(horizon: Option<Timestamp>, chunk: i64) -> Self {
        AuditPages {
            keyset: Keyset::new(None, chunk),
            horizon,
        }
    }

    /// Start strictly after this coordinate rather than at the beginning.
    pub(crate) fn after(mut self, cursor: Option<(Timestamp, Uuid)>) -> Self {
        self.keyset.cursor = cursor;
        self
    }

    /// The next chunk, in replay order; empty once the walk is done, and
    /// from then on without asking the database again.
    pub(crate) async fn next(
        &mut self,
        conn: &mut sqlx::PgConnection,
    ) -> Result<Vec<AuditRow>, PgError> {
        if self.keyset.done {
            return Ok(Vec::new());
        }
        let rows =
            list_audit_rows_page(conn, self.keyset.cursor, self.horizon, self.keyset.chunk).await?;
        self.keyset.advance(
            rows.len(),
            rows.last().map(|r| (r.committed_at, r.transition_id)),
        );
        Ok(rows)
    }
}

// These query texts are mirrored in tests/plan_shapes.rs, which pins their
// plans; a change here belongs there too.
async fn replay_page(
    conn: &mut sqlx::PgConnection,
    cursor: Option<(Timestamp, Uuid)>,
    through: Option<(Timestamp, Uuid)>,
    limit: i64,
) -> Result<Vec<ReplayRow>, PgError> {
    match (cursor, through) {
        (None, None) => {
            sqlx::query_as!(
                ReplayRow,
                "SELECT transition_id, transformation_name,
                        asserted_claims, retracted_claims, committed_at
                 FROM morpholog.audit
                 ORDER BY committed_at, transition_id
                 LIMIT $1",
                limit,
            )
            .fetch_all(&mut *conn)
            .await
        }
        (Some((at, id)), None) => {
            sqlx::query_as!(
                ReplayRow,
                "SELECT transition_id, transformation_name,
                        asserted_claims, retracted_claims, committed_at
                 FROM morpholog.audit
                 WHERE (committed_at, transition_id) > ($2, $3)
                 ORDER BY committed_at, transition_id
                 LIMIT $1",
                limit,
                at.to_sqlx(),
                id,
            )
            .fetch_all(&mut *conn)
            .await
        }
        (None, Some((to_at, to_id))) => {
            sqlx::query_as!(
                ReplayRow,
                "SELECT transition_id, transformation_name,
                        asserted_claims, retracted_claims, committed_at
                 FROM morpholog.audit
                 WHERE (committed_at, transition_id) <= ($2, $3)
                 ORDER BY committed_at, transition_id
                 LIMIT $1",
                limit,
                to_at.to_sqlx(),
                to_id,
            )
            .fetch_all(&mut *conn)
            .await
        }
        (Some((at, id)), Some((to_at, to_id))) => {
            sqlx::query_as!(
                ReplayRow,
                "SELECT transition_id, transformation_name,
                        asserted_claims, retracted_claims, committed_at
                 FROM morpholog.audit
                 WHERE (committed_at, transition_id) > ($2, $3)
                   AND (committed_at, transition_id) <= ($4, $5)
                 ORDER BY committed_at, transition_id
                 LIMIT $1",
                limit,
                at.to_sqlx(),
                id,
                to_at.to_sqlx(),
                to_id,
            )
            .fetch_all(&mut *conn)
            .await
        }
    }
    .map_err(classify)
}

#[cfg(test)]
mod tests;
