//! Every walk over the audit log in replay order: a keyset over
//! [`AuditPosition`], one chunk in memory at a time, up to
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
//!
//! A page holds at most one baseline chunk, cut in the statement before
//! the second, so a walk over the activation decodes one chunk at a time
//! however many there are. A short page therefore proves nothing about
//! the end of the log; only an empty one does.

use jiff::Timestamp;
use jiff_sqlx::ToSqlx;
use uuid::Uuid;

use crate::activation::BASELINE;
use crate::audit::{
    AuditPosition, AuditRow, REPLAY_CHUNK, list_audit_rows_page, list_audit_rows_page_mentioning,
};
use crate::error::{PgError, classify};

/// The paging state both projections share.
#[derive(Debug)]
struct Keyset {
    cursor: Option<AuditPosition>,
    through: Option<AuditPosition>,
    chunk: i64,
    done: bool,
}

impl Keyset {
    fn new(through: Option<AuditPosition>, chunk: i64) -> Self {
        Keyset {
            cursor: None,
            through,
            chunk,
            done: false,
        }
    }

    /// An empty page ends the walk, and so does a page ending on the
    /// `through` target, saving an empty query. A short page does not: a
    /// page is cut before a second baseline chunk.
    fn advance(&mut self, last: Option<AuditPosition>) {
        let Some(last) = last else {
            self.done = true;
            return;
        };
        self.cursor = Some(last);
        self.done = self.through == Some(last);
    }
}

/// The columns a claims replay folds: the claim arrays, the operations
/// and the coordinates, plus the transformation name coverage attributes
/// by and the activation's rows are known by.
pub(crate) struct ReplayRow {
    pub(crate) transition_id: Uuid,
    pub(crate) transformation_name: String,
    pub(crate) asserted_claims: serde_json::Value,
    pub(crate) retracted_claims: serde_json::Value,
    pub(crate) committed_at: Timestamp,
    pub(crate) audit_order: i64,
    pub(crate) act_ordinal: i32,
    pub(crate) operations: Option<serde_json::Value>,
}

impl ReplayRow {
    pub(crate) fn position(&self) -> AuditPosition {
        AuditPosition {
            audit_order: self.audit_order.cast_unsigned(),
            act_ordinal: self.act_ordinal.cast_unsigned(),
            committed_at: self.committed_at,
            transition_id: self.transition_id,
        }
    }
}

/// A walk that reads `ReplayRow`s, to the end of the snapshot or through
/// one transition inclusive.
#[derive(Debug)]
pub(crate) struct ReplayPages(Keyset);

impl ReplayPages {
    pub(crate) fn new(through: Option<AuditPosition>) -> Self {
        Self::with_chunk(through, REPLAY_CHUNK)
    }

    pub(crate) fn with_chunk(through: Option<AuditPosition>, chunk: i64) -> Self {
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
        self.0.advance(rows.last().map(ReplayRow::position));
        Ok(rows)
    }
}

/// A walk that reads whole `AuditRow`s, to the end of the snapshot or to
/// just before a horizon.
#[derive(Debug)]
pub(crate) struct AuditPages {
    keyset: Keyset,
    horizon: Option<u64>,
    /// Only rows mentioning this subject; the bounds apply either way.
    mentions: Option<String>,
}

impl AuditPages {
    pub(crate) fn new(horizon: Option<u64>) -> Self {
        Self::with_chunk(horizon, REPLAY_CHUNK)
    }

    pub(crate) fn with_chunk(horizon: Option<u64>, chunk: i64) -> Self {
        AuditPages {
            keyset: Keyset::new(None, chunk),
            horizon,
            mentions: None,
        }
    }

    /// Only the rows mentioning this subject, through the filtered
    /// statements; the unfiltered walk keeps its own.
    pub(crate) fn mentioning(mut self, subject: &str) -> Self {
        self.mentions = Some(subject.to_string());
        self
    }

    /// Start strictly after this coordinate rather than at the beginning.
    pub(crate) fn after(mut self, cursor: Option<AuditPosition>) -> Self {
        self.keyset.cursor = cursor;
        self
    }

    /// The next chunk, in replay order; empty once the walk is done, and
    /// from then on without asking the database again.
    pub(crate) async fn next(
        &mut self,
        conn: &mut sqlx::PgConnection,
    ) -> Result<Vec<AuditRow>, PgError> {
        self.next_up_to(conn, self.keyset.chunk).await
    }

    /// As [`AuditPages::next`], asking the database for at most `max` rows
    /// (and never more than a chunk), so a caller that wants few rows
    /// reads only those.
    pub(crate) async fn next_up_to(
        &mut self,
        conn: &mut sqlx::PgConnection,
        max: i64,
    ) -> Result<Vec<AuditRow>, PgError> {
        if self.keyset.done {
            return Ok(Vec::new());
        }
        let asked = max.clamp(1, self.keyset.chunk);
        let rows = match &self.mentions {
            None => list_audit_rows_page(conn, self.keyset.cursor, self.horizon, asked).await?,
            Some(subject) => {
                list_audit_rows_page_mentioning(
                    conn,
                    self.keyset.cursor,
                    self.horizon,
                    asked,
                    subject,
                )
                .await?
            }
        };
        self.keyset.advance(rows.last().map(AuditRow::position));
        Ok(rows)
    }
}

// These query texts are mirrored in tests/plan_shapes.rs, which pins their
// plans; a change here belongs there too.
async fn replay_page(
    conn: &mut sqlx::PgConnection,
    cursor: Option<AuditPosition>,
    through: Option<AuditPosition>,
    limit: i64,
) -> Result<Vec<ReplayRow>, PgError> {
    match (cursor, through) {
        (None, None) => {
            sqlx::query_as!(
                ReplayRow,
                "SELECT transition_id, transformation_name,
                        asserted_claims, retracted_claims, committed_at, audit_order, act_ordinal,
                        operations
                  FROM (SELECT transition_id, transformation_name,
                               asserted_claims, retracted_claims, committed_at, audit_order, act_ordinal,
                               operations,
                               sum((transformation_name = $2)::int) OVER w AS chunks
                        FROM morpholog.audit
                        WINDOW w AS (ORDER BY audit_order, act_ordinal, committed_at, transition_id
                                     ROWS UNBOUNDED PRECEDING)
                        ORDER BY audit_order, act_ordinal, committed_at, transition_id
                        LIMIT $1) s
                  WHERE chunks <= 1
                  ORDER BY audit_order, act_ordinal, committed_at, transition_id",
                limit,
                BASELINE,
            )
            .fetch_all(&mut *conn)
            .await
        }
        (
            Some(AuditPosition {
                audit_order: o,
                act_ordinal: k,
                committed_at: at,
                transition_id: id,
            }),
            None,
        ) => {
            sqlx::query_as!(
                ReplayRow,
                "SELECT transition_id, transformation_name,
                        asserted_claims, retracted_claims, committed_at, audit_order, act_ordinal,
                        operations
                  FROM (SELECT transition_id, transformation_name,
                               asserted_claims, retracted_claims, committed_at, audit_order, act_ordinal,
                               operations,
                               sum((transformation_name = $6)::int) OVER w AS chunks
                        FROM morpholog.audit
                        WHERE (audit_order, act_ordinal, committed_at, transition_id) > ($4, $5, $2, $3)
                        WINDOW w AS (ORDER BY audit_order, act_ordinal, committed_at, transition_id
                                     ROWS UNBOUNDED PRECEDING)
                        ORDER BY audit_order, act_ordinal, committed_at, transition_id
                        LIMIT $1) s
                  WHERE chunks <= 1
                  ORDER BY audit_order, act_ordinal, committed_at, transition_id",
                limit,
                at.to_sqlx(),
                id,
                o.cast_signed(),
                k.cast_signed(),
                BASELINE,
            )
            .fetch_all(&mut *conn)
            .await
        }
        (
            None,
            Some(AuditPosition {
                audit_order: to_o,
                act_ordinal: to_k,
                committed_at: to_at,
                transition_id: to_id,
            }),
        ) => {
            sqlx::query_as!(
                ReplayRow,
                "SELECT transition_id, transformation_name,
                        asserted_claims, retracted_claims, committed_at, audit_order, act_ordinal,
                        operations
                  FROM (SELECT transition_id, transformation_name,
                               asserted_claims, retracted_claims, committed_at, audit_order, act_ordinal,
                               operations,
                               sum((transformation_name = $6)::int) OVER w AS chunks
                        FROM morpholog.audit
                        WHERE (audit_order, act_ordinal, committed_at, transition_id) <= ($4, $5, $2, $3)
                        WINDOW w AS (ORDER BY audit_order, act_ordinal, committed_at, transition_id
                                     ROWS UNBOUNDED PRECEDING)
                        ORDER BY audit_order, act_ordinal, committed_at, transition_id
                        LIMIT $1) s
                  WHERE chunks <= 1
                  ORDER BY audit_order, act_ordinal, committed_at, transition_id",
                limit,
                to_at.to_sqlx(),
                to_id,
                to_o.cast_signed(),
                to_k.cast_signed(),
                BASELINE,
            )
            .fetch_all(&mut *conn)
            .await
        }
        (
            Some(AuditPosition {
                audit_order: o,
                act_ordinal: k,
                committed_at: at,
                transition_id: id,
            }),
            Some(AuditPosition {
                audit_order: to_o,
                act_ordinal: to_k,
                committed_at: to_at,
                transition_id: to_id,
            }),
        ) => {
            sqlx::query_as!(
                ReplayRow,
                "SELECT transition_id, transformation_name,
                        asserted_claims, retracted_claims, committed_at, audit_order, act_ordinal,
                        operations
                  FROM (SELECT transition_id, transformation_name,
                               asserted_claims, retracted_claims, committed_at, audit_order, act_ordinal,
                               operations,
                               sum((transformation_name = $10)::int) OVER w AS chunks
                        FROM morpholog.audit
                        WHERE (audit_order, act_ordinal, committed_at, transition_id) > ($6, $7, $2, $3)
                          AND (audit_order, act_ordinal, committed_at, transition_id) <= ($8, $9, $4, $5)
                        WINDOW w AS (ORDER BY audit_order, act_ordinal, committed_at, transition_id
                                     ROWS UNBOUNDED PRECEDING)
                        ORDER BY audit_order, act_ordinal, committed_at, transition_id
                        LIMIT $1) s
                  WHERE chunks <= 1
                  ORDER BY audit_order, act_ordinal, committed_at, transition_id",
                limit,
                at.to_sqlx(),
                id,
                to_at.to_sqlx(),
                to_id,
                o.cast_signed(),
                k.cast_signed(),
                to_o.cast_signed(),
                to_k.cast_signed(),
                BASELINE,
            )
            .fetch_all(&mut *conn)
            .await
        }
    }
    .map_err(classify)
}

#[cfg(test)]
mod tests;
