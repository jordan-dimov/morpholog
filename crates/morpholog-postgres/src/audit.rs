use crate::attestation::AuditAttestation;
use crate::error::{PgError, classify, classify_checked_query};
use crate::propose::AuditedInvariantCheck;
use crate::txn::{TxIsolation, begin_isolated_tx};
use jiff::Timestamp;
use jiff_sqlx::ToSqlx;
use morpholog_core::{ClaimInstance, EvalValue, IntentInstance, Subject, TransformationName};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;
/// One row of `morpholog.audit` decoded into typed runtime values.
///
/// One row per committed transformation. JSONB columns decode through the
/// codec that wrote them, so the round-trip is exact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRow {
    pub transition_id: Uuid,
    pub transformation_name: TransformationName,
    pub arguments: Vec<EvalValue>,
    #[serde(with = "morpholog_core::actor_repr")]
    pub actor: Subject,
    pub invariant_epoch: i32,
    pub invariants_checked: Vec<AuditedInvariantCheck>,
    pub asserted_claims: Vec<ClaimInstance>,
    pub retracted_claims: Vec<ClaimInstance>,
    pub emitted_intents: Vec<IntentInstance>,
    #[serde(with = "crate::wire_time")]
    pub committed_at: Timestamp,
    /// How the actor identity was established. Absent on rows written
    /// before attestation existed; those rows keep the original Merkle
    /// leaf encoding, so the field's presence selects the leaf version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation: Option<AuditAttestation>,
    /// The transformation's parameter names in declaration order, one per
    /// argument, stamped at commit so the row stays readable after the
    /// transformation is retired. Absent on older rows; presence selects
    /// the self-describing leaf encoding.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<Vec<String>>,
}

impl AuditRow {
    /// Check the row is a shape the runtime writes: nothing extra, an
    /// attestation, or an attestation plus one name per argument.
    ///
    /// Checked at the database boundary and before hashing, because packs
    /// carry rows as hostile input.
    pub fn validate_shape(&self) -> Result<(), String> {
        match (&self.attestation, &self.parameters) {
            (_, None) => Ok(()),
            (None, Some(_)) => Err(format!(
                "audit row {} carries parameter names but no attestation",
                self.transition_id
            )),
            (Some(_), Some(names)) if names.len() != self.arguments.len() => Err(format!(
                "audit row {} carries {} parameter names for {} arguments",
                self.transition_id,
                names.len(),
                self.arguments.len()
            )),
            (Some(_), Some(_)) => Ok(()),
        }
    }
}
/// Page size for every keyset read over the replay order. One chunk sits
/// in memory at a time.
pub(crate) const REPLAY_CHUNK: i64 = 1024;
/// One raw `morpholog.audit` row as `query_as!` decodes it; turned into
/// an [`AuditRow`] by [`decode_audit_row`].
pub(crate) struct AuditRowRaw {
    transition_id: Uuid,
    transformation_name: String,
    arguments: serde_json::Value,
    actor: serde_json::Value,
    invariant_epoch: i32,
    invariants_checked: serde_json::Value,
    asserted_claims: serde_json::Value,
    retracted_claims: serde_json::Value,
    emitted_intents: serde_json::Value,
    committed_at: Timestamp,
    // Nullable even though a constraint refuses new NULLs: upgraded
    // databases hold NULL on historical rows, and fresh and upgraded
    // databases must describe the column alike for the query checks.
    attestation: Option<serde_json::Value>,
    // Nullable for the same reason: historical rows carry no names.
    parameters: Option<serde_json::Value>,
}
// The canonical column order, shared by `AuditRowRaw` and every listing
// SELECT (each `query_as!` must spell it out literally):
//   transition_id, transformation_name, arguments, actor,
//   invariant_epoch, invariants_checked,
//   asserted_claims, retracted_claims, emitted_intents, committed_at,
//   attestation, parameters
pub(crate) fn decode_audit_row(row: AuditRowRaw) -> Result<AuditRow, PgError> {
    let decoded = AuditRow {
        transition_id: row.transition_id,
        transformation_name: TransformationName::from(row.transformation_name),
        arguments: serde_json::from_value(row.arguments)?,
        actor: match serde_json::from_value::<EvalValue>(row.actor)? {
            EvalValue::Subject(s) => s,
            other => {
                return Err(PgError::InvalidState(format!(
                    "audit actor is not a subject: {other:?}"
                )));
            }
        },
        invariant_epoch: row.invariant_epoch,
        invariants_checked: serde_json::from_value(row.invariants_checked)?,
        asserted_claims: serde_json::from_value(row.asserted_claims)?,
        retracted_claims: serde_json::from_value(row.retracted_claims)?,
        emitted_intents: serde_json::from_value(row.emitted_intents)?,
        committed_at: row.committed_at,
        // Strict, like the actor: an unrecognised attestation shape is
        // an error at this boundary, never a value that hashes on.
        attestation: row
            .attestation
            .map(serde_json::from_value::<AuditAttestation>)
            .transpose()?,
        parameters: row
            .parameters
            .map(serde_json::from_value::<Vec<String>>)
            .transpose()?,
    };
    decoded.validate_shape().map_err(PgError::InvalidState)?;
    Ok(decoded)
}
/// Return every committed audit row, ordered by `(committed_at,
/// transition_id)`: commit order, with the UUIDv7 key as tie-break.
///
/// A decoding error is [`PgError::Encoding`], which on a database the
/// runtime wrote means corruption or tampering.
///
/// A whole-table fetch for tests and small histories. To tail, use
/// [`list_audit_rows_page`] under [`audit_resume_watermark`].
pub async fn list_audit_rows(pool: &PgPool) -> Result<Vec<AuditRow>, PgError> {
    let mut conn = pool.acquire().await.map_err(classify)?;
    list_audit_rows_page(&mut conn, None, None, i64::MAX).await
}
/// One keyset page of audit rows in `(committed_at, transition_id)`
/// order: strictly after `cursor` and strictly below `horizon`, each when
/// given. Takes a connection so the caller can hold one snapshot across
/// pages.
///
/// `horizon` comes from [`audit_resume_watermark`]; `None` reads to the
/// snapshot's end and loses the lossless-resume guarantee.
// These query texts are mirrored in tests/plan_shapes.rs, which pins their
// plans; a change here belongs there too.
pub async fn list_audit_rows_page(
    conn: &mut sqlx::PgConnection,
    cursor: Option<(Timestamp, Uuid)>,
    horizon: Option<Timestamp>,
    limit: i64,
) -> Result<Vec<AuditRow>, PgError> {
    let rows = match (&cursor, &horizon) {
        (None, None) => {
            sqlx::query_as!(
                AuditRowRaw,
                "SELECT transition_id, transformation_name, arguments, actor,
                        invariant_epoch, invariants_checked,
                        asserted_claims, retracted_claims, emitted_intents, committed_at,
                attestation, parameters
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
                AuditRowRaw,
                "SELECT transition_id, transformation_name, arguments, actor,
                        invariant_epoch, invariants_checked,
                        asserted_claims, retracted_claims, emitted_intents, committed_at,
                attestation, parameters
                 FROM morpholog.audit
                 WHERE (committed_at, transition_id) > ($2, $3)
                 ORDER BY committed_at, transition_id
                 LIMIT $1",
                limit,
                at.to_sqlx(),
                *id,
            )
            .fetch_all(&mut *conn)
            .await
        }
        (None, Some(h)) => {
            sqlx::query_as!(
                AuditRowRaw,
                "SELECT transition_id, transformation_name, arguments, actor,
                        invariant_epoch, invariants_checked,
                        asserted_claims, retracted_claims, emitted_intents, committed_at,
                attestation, parameters
                 FROM morpholog.audit
                 WHERE committed_at < $2
                 ORDER BY committed_at, transition_id
                 LIMIT $1",
                limit,
                h.to_sqlx(),
            )
            .fetch_all(&mut *conn)
            .await
        }
        (Some((at, id)), Some(h)) => {
            sqlx::query_as!(
                AuditRowRaw,
                "SELECT transition_id, transformation_name, arguments, actor,
                        invariant_epoch, invariants_checked,
                        asserted_claims, retracted_claims, emitted_intents, committed_at,
                attestation, parameters
                 FROM morpholog.audit
                 WHERE (committed_at, transition_id) > ($2, $3)
                   AND committed_at < $4
                 ORDER BY committed_at, transition_id
                 LIMIT $1",
                limit,
                at.to_sqlx(),
                *id,
                h.to_sqlx(),
            )
            .fetch_all(&mut *conn)
            .await
        }
    }
    .map_err(classify)?;
    rows.into_iter().map(decode_audit_row).collect()
}
use crate::audit_pages::AuditPages;

/// A streaming audit tail with the lossless-resume order built in.
///
/// [`begin_audit_tail`] resolves the cursor, computes the horizon BEFORE
/// the snapshot, then opens one `REPEATABLE READ READ ONLY` transaction;
/// [`AuditTail::next_page`] pages to the horizon inside it. Rows from
/// writers in flight at the horizon are withheld for the next tail, never
/// skipped (see [`audit_resume_watermark`]).
pub struct AuditTail<'p> {
    tx: Transaction<'p, Postgres>,
    pages: AuditPages,
}
/// Open an audit tail, optionally resuming strictly after a seen
/// transition. An unknown id is [`PgError::TransitionNotFound`], never a
/// silent restart from zero.
pub async fn begin_audit_tail<'p>(
    pool: &'p PgPool,
    after: Option<Uuid>,
    writers: Option<&[String]>,
) -> Result<AuditTail<'p>, PgError> {
    let cursor = match after {
        Some(tid) => {
            let mut conn = pool.acquire().await.map_err(classify)?;
            Some(audit_cursor_for(&mut conn, tid).await?)
        }
        None => None,
    };
    // Horizon strictly before the snapshot - the ordering the
    // lossless-resume proof rests on.
    let horizon = audit_resume_watermark(pool, writers).await?;
    let tx = begin_isolated_tx(pool, TxIsolation::RepeatableReadReadOnly).await?;
    Ok(AuditTail {
        tx,
        pages: AuditPages::new(Some(horizon)).after(cursor),
    })
}
impl AuditTail<'_> {
    /// The next page of transitions in `(committed_at, transition_id)`
    /// order; empty once the tail reaches the horizon.
    pub async fn next_page(&mut self) -> Result<Vec<AuditRow>, PgError> {
        self.pages.next(&mut self.tx).await
    }
}
/// Resolve a transition id to the `(committed_at, transition_id)`
/// keyset cursor every audit read orders by. An unknown id is
/// [`PgError::TransitionNotFound`], so a typo never restarts a tail.
pub async fn audit_cursor_for(
    conn: &mut sqlx::PgConnection,
    transition_id: Uuid,
) -> Result<(Timestamp, Uuid), PgError> {
    let row = sqlx::query!(
        "SELECT committed_at FROM morpholog.audit WHERE transition_id = $1",
        transition_id,
    )
    .fetch_optional(&mut *conn)
    .await
    .map_err(classify_checked_query)?;
    match row {
        Some(row) => Ok((row.committed_at.into(), transition_id)),
        None => Err(PgError::TransitionNotFound(transition_id)),
    }
}
/// The resume horizon for a lossless audit tail: every audit row with
/// `committed_at` strictly below the returned instant is already
/// visible to a snapshot taken AFTER this call returns.
///
/// Why: `committed_at` is the WRITER's transaction start (`now()`), but
/// visibility follows commit order. A snapshot alone can miss an
/// in-flight writer whose row will sort below rows already emitted, and
/// a cursor past that slot would skip it forever. The horizon is the
/// minimum `xact_start` over every other open transaction in this
/// database except autovacuum's, or `now()` if none, computed BEFORE the
/// read snapshot. Any row the snapshot cannot see belongs to a writer
/// that started at or after the horizon, so the `< horizon` clamp
/// excludes it. Such rows are withheld, never lost: the next call's
/// horizon surfaces them.
///
/// Preconditions:
/// - The caller takes its read snapshot AFTER this call returns.
/// - This session can see other sessions in `pg_stat_activity` (same
///   role as the writers, `pg_read_all_stats`, or superuser). An unseen
///   session would silently drop out of the minimum, so it is detected
///   and returned as [`PgError::StatVisibility`].
/// - No prepared (2PC) transaction writes audit. PostgreSQL defaults to
///   `max_prepared_transactions = 0` and the adapter never prepares.
///
/// Autovacuum workers are excluded: they hold long transactions and never
/// write audit. The filter uses `IS DISTINCT FROM`, not `<>`, because an
/// unseen session has a null `backend_type` and `<>` would drop it from
/// both the minimum and the hidden count.
///
/// Liveness: a stuck session anywhere in the database stalls the tail;
/// it never loses rows.
///
/// # The writer assertion (`writers: Some(..)`)
///
/// Managed PostgreSQL hides the platform's sessions and will not grant
/// `pg_read_all_stats`, so the all-sessions horizon is unavailable. The
/// operator can instead name the login roles that write audit, and the
/// horizon covers only their sessions.
///
/// The assertion is verified, not trusted. In the SAME statement as the
/// horizon (one snapshot, so a concurrent grant cannot split them), a
/// catalog census lists every non-superuser role that (a) can hold a
/// session - login-capable or currently connected, which catches a role
/// made NOLOGIN after connecting - and (b) can insert into
/// `morpholog.audit` directly, through inherited membership, or by
/// `SET ROLE` (`pg_has_role(..., 'SET')`; `'MEMBER'` would also demand
/// roles with no usable path). Errors:
/// - an asserted name that does not exist: [`PgError::WriterRoleUnknown`];
/// - a census role missing from the assertion:
///   [`PgError::WriterAssertionIncomplete`];
/// - a hidden session of an asserted role: [`PgError::WriterSessionsHidden`].
///
/// Sessions match by role OID, not name, so a rename cannot misclassify
/// one.
///
/// What the assertion accepts:
/// - SUPERUSER writes are outside the proof. Superusers bypass ACLs and
///   every managed host runs them; they do not write embedder schemas.
/// - Role configuration (grants, memberships, login) must stay stable
///   from this statement until the caller's snapshot. A grant in that
///   window, to a role with an already-open transaction, is the accepted
///   residue.
pub async fn audit_resume_watermark(
    pool: &PgPool,
    writers: Option<&[String]>,
) -> Result<Timestamp, PgError> {
    if let Some(asserted) = writers {
        return audit_resume_watermark_asserted(pool, asserted).await;
    }
    // One statement: the `now()` fallback must be taken at the same
    // instant as the minimum, or a writer starting in between would sort
    // below it. An unseen session shows its query as
    // '<insufficient privilege>' and hides `xact_start`, so it is counted.
    // `horizon!` / `hidden!`: coalesce(_, now()) and count(*) are never
    // null.
    let row = sqlx::query!(
        r#"SELECT coalesce(min(xact_start), now()) AS "horizon!",
                  count(*) FILTER (WHERE query = '<insufficient privilege>') AS "hidden!"
           FROM pg_stat_activity
           WHERE datname = current_database()
             AND pid <> pg_backend_pid()
             AND backend_type IS DISTINCT FROM 'autovacuum worker'"#,
    )
    .fetch_one(pool)
    .await
    .map_err(classify_checked_query)?;
    if row.hidden > 0 {
        return Err(PgError::StatVisibility { hidden: row.hidden });
    }
    Ok(row.horizon.into())
}

/// The assertion-mode horizon: census, filter, and minimum in one
/// statement. See `audit_resume_watermark`.
async fn audit_resume_watermark_asserted(
    pool: &PgPool,
    asserted: &[String],
) -> Result<Timestamp, PgError> {
    if asserted.is_empty() {
        return Err(PgError::WriterAssertionEmpty);
    }
    let mut names: Vec<String> = asserted.to_vec();
    names.sort();
    names.dedup();
    // `horizon!` / `hidden!`: coalesce and count are never null.
    // `unknown` and `missing` stay nullable: array_agg over nothing is
    // null, which means "nothing wrong".
    let row = sqlx::query!(
        r#"WITH asserted AS (
               SELECT a.name, r.oid AS role_oid
               FROM unnest($1::text[]) AS a(name)
               LEFT JOIN pg_roles r ON r.rolname = a.name
           ),
           census AS (
               SELECT r.rolname, r.oid
               FROM pg_roles r
               WHERE NOT r.rolsuper
                 AND (r.rolcanlogin OR EXISTS (
                        SELECT 1 FROM pg_stat_activity s
                        WHERE s.usesysid = r.oid
                          AND s.datname = current_database()))
                 AND (has_table_privilege(r.oid, 'morpholog.audit', 'INSERT')
                      OR EXISTS (
                        SELECT 1 FROM pg_roles w
                        WHERE has_table_privilege(w.oid, 'morpholog.audit', 'INSERT')
                          AND pg_has_role(r.oid, w.oid, 'SET')))
           )
           SELECT
               (SELECT array_agg(a.name ORDER BY a.name)
                  FROM asserted a WHERE a.role_oid IS NULL) AS unknown,
               (SELECT array_agg(c.rolname::text ORDER BY c.rolname)
                  FROM census c
                 WHERE c.oid NOT IN (SELECT a.role_oid FROM asserted a
                                     WHERE a.role_oid IS NOT NULL)) AS missing,
               coalesce(min(s.xact_start), now()) AS "horizon!",
               count(*) FILTER (WHERE s.query = '<insufficient privilege>') AS "hidden!"
           FROM pg_stat_activity s
           WHERE s.datname = current_database()
             AND s.pid <> pg_backend_pid()
             AND s.usesysid IN (SELECT a.role_oid FROM asserted a
                                WHERE a.role_oid IS NOT NULL)"#,
        &names,
    )
    .fetch_one(pool)
    .await
    .map_err(classify_checked_query)?;
    if let Some(unknown) = row.unknown.filter(|u| !u.is_empty()) {
        return Err(PgError::WriterRoleUnknown { roles: unknown });
    }
    if let Some(missing) = row.missing.filter(|m| !m.is_empty()) {
        return Err(PgError::WriterAssertionIncomplete { missing });
    }
    if row.hidden > 0 {
        return Err(PgError::WriterSessionsHidden { hidden: row.hidden });
    }
    Ok(row.horizon.into())
}
