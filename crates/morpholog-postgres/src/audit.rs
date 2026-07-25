use crate::attestation::AuditAttestation;
use crate::error::{PgError, classify};
use crate::propose::AuditedInvariantCheck;
use crate::txn::{TxIsolation, begin_isolated_tx};
use chrono::{DateTime, Utc};
use morpholog_core::{ClaimInstance, EvalValue, IntentInstance, Subject, TransformationName};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;
/// One row of `morpholog.audit` decoded into typed runtime values.
///
/// Each row corresponds to exactly one committed transformation. The
/// JSONB columns are decoded through the same codec that wrote them,
/// so the round-trip is exact for any value the kernel can represent.
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
    pub committed_at: DateTime<Utc>,
    /// How the actor identity was established. Absent on rows written
    /// before attestation existed; those rows keep the original Merkle
    /// leaf encoding, so the field's presence selects the leaf version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attestation: Option<AuditAttestation>,
}
/// Page size for every keyset read over the replay order - the audit
/// tail, coverage's two passes, and the chunked replays. One chunk in
/// memory at a time; hardcoded until a real history forces tuning.
pub(crate) const REPLAY_CHUNK: i64 = 1024;
/// One raw `morpholog.audit` row as `query_as!` decodes it (DB shape
/// only); turned into a typed [`AuditRow`] by [`decode_audit_row`].
/// Field order matches the SELECT column order in the listing queries.
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
    committed_at: DateTime<Utc>,
    // Nullable by column attribute even though the named constraint
    // refuses new NULLs: a database upgraded from before attestation
    // lawfully holds NULL on its historical rows, and fresh and
    // upgraded databases must describe the column identically for the
    // compile-time query checks.
    attestation: Option<serde_json::Value>,
}
// The audit columns, in the order `AuditRowRaw`'s fields and the listing
// queries' SELECTs share. Inlined as a literal in each `query_as!`
// (macros cannot interpolate a runtime column list); this note is the
// one place that records the canonical order:
//   transition_id, transformation_name, arguments, actor,
//   invariant_epoch, invariants_checked,
//   asserted_claims, retracted_claims, emitted_intents, committed_at,
//   attestation
pub(crate) fn decode_audit_row(row: AuditRowRaw) -> Result<AuditRow, PgError> {
    Ok(AuditRow {
        transition_id: row.transition_id,
        transformation_name: TransformationName::from(row.transformation_name),
        arguments: serde_json::from_value(row.arguments)?,
        // Decode the tagged actor JSON and extract the subject,
        // erroring at this boundary if the column somehow holds a
        // non-subject value.
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
    })
}
/// Return every committed audit row from `morpholog.audit`, ordered by
/// `(committed_at, transition_id)`: causal commit order with the
/// time-ordered UUIDv7 PRIMARY KEY as the stable tie-break.
///
/// JSONB columns are decoded through the codec into typed values. A
/// decoding error surfaces as [`PgError::Encoding`]; against a database
/// the runtime itself wrote, that indicates corruption or tampering.
///
/// A whole-table fetch, intended for tests, demos, and small-history
/// inspection. The blessed tailing surface is
/// [`list_audit_rows_page`] under [`audit_resume_watermark`], which
/// is what `inspect audit` streams.
pub async fn list_audit_rows(pool: &PgPool) -> Result<Vec<AuditRow>, PgError> {
    let rows = sqlx::query_as!(
        AuditRowRaw,
        "SELECT transition_id, transformation_name, arguments, actor,
                invariant_epoch, invariants_checked,
                asserted_claims, retracted_claims, emitted_intents, committed_at,
                attestation
         FROM morpholog.audit
         ORDER BY committed_at, transition_id"
    )
    .fetch_all(pool)
    .await
    .map_err(classify)?;
    rows.into_iter().map(decode_audit_row).collect()
}
/// One keyset page of audit rows in `(committed_at, transition_id)`
/// order: strictly after `cursor` (when given) and strictly below
/// `horizon` (when given). Takes a connection rather than a pool so a
/// caller can hold one snapshot across pages - `inspect audit` opens
/// a `REPEATABLE READ READ ONLY` transaction and loops this until a
/// short page.
///
/// `horizon` is the frontier-completeness clamp from
/// [`audit_resume_watermark`]; passing `None` reads to the snapshot's
/// end and forfeits the lossless-resume guarantee.
pub async fn list_audit_rows_page(
    conn: &mut sqlx::PgConnection,
    cursor: Option<(DateTime<Utc>, Uuid)>,
    horizon: Option<DateTime<Utc>>,
    limit: i64,
) -> Result<Vec<AuditRow>, PgError> {
    let rows = match (&cursor, &horizon) {
        (None, None) => {
            sqlx::query_as!(
                AuditRowRaw,
                "SELECT transition_id, transformation_name, arguments, actor,
                        invariant_epoch, invariants_checked,
                        asserted_claims, retracted_claims, emitted_intents, committed_at,
                attestation
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
                attestation
                 FROM morpholog.audit
                 WHERE (committed_at, transition_id) > ($2, $3)
                 ORDER BY committed_at, transition_id
                 LIMIT $1",
                limit,
                at,
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
                attestation
                 FROM morpholog.audit
                 WHERE committed_at < $2
                 ORDER BY committed_at, transition_id
                 LIMIT $1",
                limit,
                *h,
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
                attestation
                 FROM morpholog.audit
                 WHERE (committed_at, transition_id) > ($2, $3)
                   AND committed_at < $4
                 ORDER BY committed_at, transition_id
                 LIMIT $1",
                limit,
                at,
                *id,
                *h,
            )
            .fetch_all(&mut *conn)
            .await
        }
    }
    .map_err(classify)?;
    rows.into_iter().map(decode_audit_row).collect()
}
/// A streaming audit tail: the lossless-resume recipe with its
/// load-bearing order baked in, so a caller cannot get it wrong.
/// [`begin_audit_tail`] resolves the resume cursor, computes the
/// horizon BEFORE the snapshot, then opens one `REPEATABLE READ READ
/// ONLY` transaction; [`AuditTail::next_page`] pages to the horizon
/// inside that snapshot. Rows whose writers were in flight when the
/// horizon was computed are withheld for the next tail, never
/// skipped - see [`audit_resume_watermark`] for the proof.
pub struct AuditTail<'p> {
    tx: Transaction<'p, Postgres>,
    horizon: DateTime<Utc>,
    cursor: Option<(DateTime<Utc>, Uuid)>,
    done: bool,
}
/// Open an audit tail, optionally resuming strictly after a
/// previously seen transition (unknown ids are
/// [`PgError::TransitionNotFound`], never a silent restart from
/// zero).
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
        horizon,
        cursor,
        done: false,
    })
}
impl AuditTail<'_> {
    /// The next page of transitions, in `(committed_at,
    /// transition_id)` order; empty when the tail has reached the
    /// horizon. One page sits in memory at a time.
    pub async fn next_page(&mut self) -> Result<Vec<AuditRow>, PgError> {
        if self.done {
            return Ok(Vec::new());
        }
        let page =
            list_audit_rows_page(&mut self.tx, self.cursor, Some(self.horizon), REPLAY_CHUNK)
                .await?;
        match page.last() {
            Some(last) => {
                self.cursor = Some((last.committed_at, last.transition_id));
                self.done = (page.len() as i64) < REPLAY_CHUNK;
            }
            None => self.done = true,
        }
        Ok(page)
    }
}
/// Resolve a transition id to the `(committed_at, transition_id)`
/// keyset cursor every audit read orders by. Unknown ids surface as
/// [`PgError::TransitionNotFound`] - a tail resuming from a cursor it
/// was handed must learn about a typo, never silently restart from
/// zero.
pub async fn audit_cursor_for(
    conn: &mut sqlx::PgConnection,
    transition_id: Uuid,
) -> Result<(DateTime<Utc>, Uuid), PgError> {
    let row = sqlx::query!(
        "SELECT committed_at FROM morpholog.audit WHERE transition_id = $1",
        transition_id,
    )
    .fetch_optional(&mut *conn)
    .await
    .map_err(classify)?;
    match row {
        Some(row) => Ok((row.committed_at, transition_id)),
        None => Err(PgError::TransitionNotFound(transition_id)),
    }
}
/// The resume horizon for a lossless audit tail: every audit row with
/// `committed_at` strictly below the returned instant is already
/// visible to a snapshot taken AFTER this call returns.
///
/// Why this works: `committed_at` is server-evaluated `now()` - the
/// WRITER's transaction start time - while row visibility follows
/// commit order, so a snapshot alone can miss an in-flight writer
/// whose row will sort below rows already emitted; a cursor that
/// advanced past that slot would skip the row forever. The horizon
/// closes the race from the other side: it is the minimum
/// `xact_start` over every open transaction in this database (or
/// `now()` when none is open), computed BEFORE the read snapshot.
/// Any row invisible to the snapshot belongs to a writer that either
/// was in flight here (so its `committed_at` = its `xact_start` >=
/// the minimum, excluded by the `< horizon` clamp) or started later
/// (excluded likewise). Rows at or above the horizon are withheld,
/// never lost - the next invocation's fresh horizon surfaces them.
///
/// Preconditions, checked or documented:
/// - The caller takes its read snapshot AFTER this call returns (the
///   ordering is load-bearing; `inspect audit` does this).
/// - This session can SEE other sessions in `pg_stat_activity` (same
///   role as the writers, `pg_read_all_stats`, or superuser). A
///   session it cannot see would silently fall out of the minimum,
///   so insufficient visibility is DETECTED and surfaced as
///   [`PgError::StatVisibility`] rather than an unsound horizon.
/// - No prepared transactions (2PC) write audit; PostgreSQL ships
///   with `max_prepared_transactions = 0` and the adapter never
///   prepares.
///
/// Liveness: the horizon trails the oldest open transaction in the
/// database, whatever it is doing - a stuck session stalls the tail;
/// it never loses rows.
///
/// # The writer assertion (`writers: Some(..)`)
///
/// On managed PostgreSQL the platform's own sessions are permanently
/// hidden and `pg_read_all_stats` cannot be granted, so the
/// all-sessions horizon is structurally unavailable even when the
/// deployment satisfies the property it establishes. The assertion is
/// the explicit, verified opt-in: the operator names the SESSION
/// (login) roles that write audit, and the horizon is computed over
/// those roles' sessions only.
///
/// Verified, not trusted: in the SAME statement that computes the
/// horizon (one snapshot, so the census and the minimum cannot be
/// split by a concurrent grant), the catalog census enumerates every
/// non-superuser role that (a) can hold a session - login-capable, or
/// currently connected, which catches a role made NOLOGIN after its
/// session opened - and (b) can write `morpholog.audit` directly, by
/// inherited membership, or by `SET ROLE` into a granted role. An
/// asserted name that does not exist is
/// [`PgError::WriterRoleUnknown`]; a census role missing from the
/// assertion is [`PgError::WriterAssertionIncomplete`]; a hidden
/// session OF an asserted role is [`PgError::WriterSessionsHidden`]
/// (the assertion cannot compensate there). Sessions are matched by
/// role OID, not name, so a rename between census and filter cannot
/// misclassify one.
///
/// What the assertion accepts, in the operator's own words:
/// - SUPERUSER writes are outside the proof - superusers bypass ACLs,
///   every managed host runs platform superusers, and they do not
///   write embedder schemas. That residue is exactly what the flag
///   acknowledges.
/// - Role configuration (grants, memberships, login ability) must stay
///   stable from this statement until the caller's read snapshot is
///   established. The single statement closes the census-vs-horizon
///   gap; a grant landing in the remaining statement-to-snapshot
///   window, to a role with an already-open transaction, is the
///   documented residue of the opt-in.
pub async fn audit_resume_watermark(
    pool: &PgPool,
    writers: Option<&[String]>,
) -> Result<DateTime<Utc>, PgError> {
    if let Some(asserted) = writers {
        return audit_resume_watermark_asserted(pool, asserted).await;
    }
    // One statement, deliberately: the `now()` fallback must be
    // evaluated at the same instant as the minimum, because a writer
    // starting between two separate queries would carry a
    // `committed_at` below a later-computed fallback - reopening the
    // exact window the horizon exists to close. The hidden-session
    // count rides along: a session this role cannot see renders its
    // query text as the literal '<insufficient privilege>' and hides
    // `xact_start`, which would silently corrupt the minimum.
    // `horizon!`: coalesce(_, now()) can never be null. `hidden!`:
    // count(*) can never be null. The `!` overrides tell sqlx what the
    // aggregate expressions guarantee but cannot prove.
    let row = sqlx::query!(
        r#"SELECT coalesce(min(xact_start), now()) AS "horizon!",
                  count(*) FILTER (WHERE query = '<insufficient privilege>') AS "hidden!"
           FROM pg_stat_activity
           WHERE datname = current_database()
             AND pid <> pg_backend_pid()"#,
    )
    .fetch_one(pool)
    .await
    .map_err(classify)?;
    if row.hidden > 0 {
        return Err(PgError::StatVisibility { hidden: row.hidden });
    }
    Ok(row.horizon)
}

/// The assertion-mode horizon: census, filter, and minimum in one
/// statement (one snapshot). See `audit_resume_watermark`'s doc for
/// the semantics and the accepted residue.
async fn audit_resume_watermark_asserted(
    pool: &PgPool,
    asserted: &[String],
) -> Result<DateTime<Utc>, PgError> {
    if asserted.is_empty() {
        return Err(PgError::WriterAssertionEmpty);
    }
    let mut names: Vec<String> = asserted.to_vec();
    names.sort();
    names.dedup();
    // `horizon!` / `hidden!`: aggregates over a one-row aggregate query
    // can never be null (coalesce / count). `unknown` and `missing`
    // stay nullable: array_agg over an empty set IS null, and empty
    // means "nothing wrong".
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
                          AND pg_has_role(r.oid, w.oid, 'MEMBER')))
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
    .map_err(classify)?;
    if let Some(unknown) = row.unknown.filter(|u| !u.is_empty()) {
        return Err(PgError::WriterRoleUnknown { roles: unknown });
    }
    if let Some(missing) = row.missing.filter(|m| !m.is_empty()) {
        return Err(PgError::WriterAssertionIncomplete { missing });
    }
    if row.hidden > 0 {
        return Err(PgError::WriterSessionsHidden { hidden: row.hidden });
    }
    Ok(row.horizon)
}
