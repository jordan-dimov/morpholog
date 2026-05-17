//! Morpholog PostgreSQL persistence adapter.
//!
//! Thin async I/O layer around the existing synchronous
//! [`morpholog_core::propose`] kernel. The kernel itself is unchanged.
//!
//! See `crates/morpholog-core/sql/schema.sql` for the canonical schema
//! and `docs/scope-and-ambition.md` for the runtime's positioning.

use chrono::{DateTime, Utc};
use morpholog_core::{
    ClaimInstance, EvalError, EvalValue, IntentInstance, Invariant, Outcome, State, Transformation,
    propose,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};
use std::collections::HashSet;
use uuid::Uuid;

/// Re-export of `sqlx::PgPool` so downstream crates (notably
/// `morpholog-cli`) can use the connection-pool type that the public
/// async functions in this crate take, without pulling `sqlx` in as a
/// direct dependency.
pub use sqlx::PgPool;

/// Errors returned by the PostgreSQL adapter.
///
/// Lawful business rejection is **not** an error — it is returned as
/// [`PgProposalOutcome::Rejected`]. This enum captures only conditions
/// where the caller cannot or should not proceed as if the kernel had
/// run successfully.
#[derive(thiserror::Error, Debug)]
pub enum PgError {
    /// SQLSTATE 40001 from PostgreSQL SSI. The transaction should be
    /// retried by the caller.
    #[error("SERIALIZABLE retry needed (SQLSTATE 40001)")]
    SerializationFailure,
    /// Evaluation error from the in-memory kernel (e.g. unbound variable,
    /// type mismatch). Distinct from a business [`PgProposalOutcome::Rejected`].
    #[error(transparent)]
    Kernel(#[from] EvalError),
    /// Any other database error (connection, schema mismatch, etc.).
    #[error(transparent)]
    Database(sqlx::Error),
    /// JSON serialisation or deserialisation error at the codec boundary.
    #[error(transparent)]
    Encoding(#[from] serde_json::Error),
    /// Persistent state does not match expectations (e.g. a retraction
    /// matched zero rows when exactly one was expected).
    #[error("invalid persistent state: {0}")]
    InvalidState(String),
}

/// The result of proposing a transformation against PostgreSQL.
///
/// On `Committed`, the database transaction has already been committed:
/// claims have been mutated, one audit row written, and one outbox row
/// per emitted intent. On `Rejected`, the transaction has been rolled
/// back and no governed state has changed.
///
/// `Serialize` is derived with serde's internally-tagged enum
/// representation so the CLI can emit outcomes directly as JSON with
/// a `status` discriminant. A committed outcome serialises as
/// `{"status":"committed","transition_id":"...","asserted_claims":[...],
/// "retracted_claims":[...],"emitted_intents":[...]}`; a rejected one as
/// `{"status":"rejected","reason":"..."}`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum PgProposalOutcome {
    Committed {
        transition_id: Uuid,
        asserted_claims: Vec<ClaimInstance>,
        retracted_claims: Vec<ClaimInstance>,
        emitted_intents: Vec<IntentInstance>,
    },
    Rejected {
        reason: String,
    },
}

/// Propose a transformation against the live `morpholog.*` tables.
///
/// Opens one PostgreSQL transaction at SERIALIZABLE isolation, loads
/// the current claims into an in-memory [`State`], calls the existing
/// synchronous [`propose`] kernel, then either commits the changes
/// (writing claims, audit, and outbox rows) or rolls back atomically.
///
/// External side effects do not run inside this transaction. Outbox rows
/// are enqueued for post-commit delivery by workers running outside.
pub async fn propose_against_pg(
    pool: &PgPool,
    transformation: &Transformation,
    args: Vec<EvalValue>,
    invariants: &[Invariant],
) -> Result<PgProposalOutcome, PgError> {
    let mut tx = pool.begin().await.map_err(classify)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await
        .map_err(classify)?;

    let state = load_state(&mut tx).await?;
    let outcome = propose(transformation, args.clone(), &state, invariants)?;

    match outcome {
        Outcome::Rejected { reason } => {
            tx.rollback().await.map_err(classify)?;
            Ok(PgProposalOutcome::Rejected { reason })
        }
        Outcome::Accepted {
            asserted_claims,
            retracted_claims,
            emitted_intents,
            candidate_state: _,
        } => {
            let transition_id = Uuid::now_v7();
            write_accepted(
                &mut tx,
                transition_id,
                transformation,
                &args,
                invariants,
                &asserted_claims,
                &retracted_claims,
                &emitted_intents,
            )
            .await?;
            tx.commit().await.map_err(classify)?;
            Ok(PgProposalOutcome::Committed {
                transition_id,
                asserted_claims,
                retracted_claims,
                emitted_intents,
            })
        }
    }
}

/// Predicate: is this SQLSTATE the PostgreSQL serialization-failure
/// code (`40001`) returned by SSI when a SERIALIZABLE transaction
/// cannot be linearised? Extracted as a pure function so the magic
/// string can be unit-tested without mocking `sqlx::DatabaseError`.
fn is_serialization_failure_code(code: Option<&str>) -> bool {
    code == Some("40001")
}

/// Maps a `sqlx::Error` to a [`PgError`], recognising SQLSTATE 40001
/// (PostgreSQL SSI serialization failure) as the distinct retryable
/// variant. All other errors propagate as [`PgError::Database`].
fn classify(err: sqlx::Error) -> PgError {
    let code = err.as_database_error().and_then(|e| e.code());
    if is_serialization_failure_code(code.as_deref()) {
        return PgError::SerializationFailure;
    }
    PgError::Database(err)
}

async fn load_state(tx: &mut Transaction<'_, Postgres>) -> Result<State, PgError> {
    let rows: Vec<(String, serde_json::Value)> =
        sqlx::query_as("SELECT predicate_name, arguments FROM morpholog.claims")
            .fetch_all(&mut **tx)
            .await
            .map_err(classify)?;

    let mut claims = Vec::with_capacity(rows.len());
    for (predicate, args_json) in rows {
        let args: Vec<EvalValue> = serde_json::from_value(args_json)?;
        claims.push(ClaimInstance { predicate, args });
    }
    Ok(State { claims })
}

/// One entry in an audit row's `invariants_checked` JSONB array. Recorded
/// per committed transformation: the invariant `name` plus the `version`
/// active at admission time. Self-describing audit data is preferred over
/// tuple compactness.
///
/// `Serialize` is derived so the CLI can re-emit audit rows as JSON
/// without an intermediate hand-rolled mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvariantCheck {
    pub name: String,
    pub version: u32,
}

#[allow(clippy::too_many_arguments)]
async fn write_accepted(
    tx: &mut Transaction<'_, Postgres>,
    transition_id: Uuid,
    transformation: &Transformation,
    args: &[EvalValue],
    invariants: &[Invariant],
    asserted_claims: &[ClaimInstance],
    retracted_claims: &[ClaimInstance],
    emitted_intents: &[IntentInstance],
) -> Result<(), PgError> {
    // Retractions: dedupe, then delete each distinct claim. We expect
    // exactly one row affected per distinct retraction. Zero rows
    // indicates a persistent-state mismatch — either a concurrent
    // transaction has interfered (SSI will catch it later) or the
    // pre-state snapshot disagrees with the live table.
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for claim in retracted_claims {
        let args_repr = serde_json::to_string(&claim.args)?;
        let key = (claim.predicate.clone(), args_repr);
        if !seen.insert(key) {
            continue;
        }
        let args_json: serde_json::Value = serde_json::to_value(&claim.args)?;
        let result = sqlx::query(
            "DELETE FROM morpholog.claims WHERE predicate_name = $1 AND arguments = $2",
        )
        .bind(&claim.predicate)
        .bind(&args_json)
        .execute(&mut **tx)
        .await
        .map_err(classify)?;
        if result.rows_affected() != 1 {
            return Err(PgError::InvalidState(format!(
                "expected exactly 1 row deleted for retraction of `{}`, got {}",
                claim.predicate,
                result.rows_affected()
            )));
        }
    }

    // Assertions: ON CONFLICT DO NOTHING preserves the set-valued
    // semantics of claims (asserting an already-present claim is an
    // idempotent no-op).
    for claim in asserted_claims {
        let args_json: serde_json::Value = serde_json::to_value(&claim.args)?;
        sqlx::query(
            "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
             VALUES ($1, $2, $3)
             ON CONFLICT (predicate_name, arguments) DO NOTHING",
        )
        .bind(&claim.predicate)
        .bind(&args_json)
        .bind(transition_id)
        .execute(&mut **tx)
        .await
        .map_err(classify)?;
    }

    // Audit row.
    let checked: Vec<InvariantCheck> = invariants
        .iter()
        .map(|inv| InvariantCheck {
            name: inv.name.clone(),
            version: inv.version,
        })
        .collect();
    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(transition_id)
    .bind(&transformation.name)
    .bind(serde_json::to_value(args)?)
    .bind(1_i32)
    .bind(serde_json::to_value(&checked)?)
    .bind(serde_json::to_value(asserted_claims)?)
    .bind(serde_json::to_value(retracted_claims)?)
    .bind(serde_json::to_value(emitted_intents)?)
    .execute(&mut **tx)
    .await
    .map_err(classify)?;

    // Outbox rows, one per emitted intent.
    for intent in emitted_intents {
        let intent_id = Uuid::now_v7();
        let idempotency_key = compute_idempotency_key(transition_id, intent)?;
        let args_json: serde_json::Value = serde_json::to_value(&intent.args)?;
        sqlx::query(
            "INSERT INTO morpholog.outbox (
                intent_id, transition_id, intent_type, arguments, idempotency_key
             ) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(intent_id)
        .bind(transition_id)
        .bind(&intent.name)
        .bind(&args_json)
        .bind(&idempotency_key)
        .execute(&mut **tx)
        .await
        .map_err(classify)?;
    }

    Ok(())
}

/// Deterministic idempotency key for an emitted intent:
///
/// ```text
/// hex(sha256(transition_id_bytes ‖ 0x00 ‖ name_bytes ‖ 0x00 ‖ canonical_json(args)))
/// ```
///
/// `canonical_json` is `serde_json` output using the PR #4 pinned wire shape
/// (stable for the current structs because field order is fixed by derived
/// `Serialize` and there are no map-like runtime values).
///
/// The key is unique per `(transition_id, intent.name, intent.args)`. It
/// prevents duplicate outbox rows under retry/redelivery mechanics — not
/// duplicate business events, which would require an idempotency key
/// derived from the inbound request.
///
/// **Duplicate intents within one transformation:** if a transformation
/// emits the same intent (same `name` and `args`) twice, both rows will
/// share an idempotency key and the second `INSERT` will violate the
/// `outbox.idempotency_key` UNIQUE constraint — surfacing as a
/// `PgError::Database` and rolling back the whole transformation. This
/// is intentional for v0: identical duplicate intents are almost always
/// a bug and should not silently produce two outbox rows. If genuinely
/// distinct same-shaped intents are needed later, the `Intent` type
/// will gain a discriminator field (logical key, purpose, sequence) and
/// this docstring should be updated.
pub fn compute_idempotency_key(
    transition_id: Uuid,
    intent: &IntentInstance,
) -> Result<String, serde_json::Error> {
    let args_bytes = serde_json::to_vec(&intent.args)?;
    let mut hasher = Sha256::new();
    hasher.update(transition_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(intent.name.as_bytes());
    hasher.update(b"\0");
    hasher.update(&args_bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

// ===========================================================================
// Read API — current-state inspection
// ===========================================================================
//
// These helpers expose the durable substrate for inspection without
// requiring callers to write raw SQL. They return *current* state only:
// no as-of evaluation, no derived claims, no projection. A caller that
// needs "what did the state look like at transition T" must build that
// by replay over `list_audit_rows`; the kernel does not yet support
// historical reconstruction directly.

/// One row of `morpholog.audit` decoded into typed runtime values.
///
/// Each row corresponds to exactly one committed transformation. The
/// JSONB columns (`arguments`, `invariants_checked`, `asserted_claims`,
/// `retracted_claims`, `emitted_intents`) are decoded through the same
/// codec that wrote them, so the round-trip is exact for any value the
/// kernel can represent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditRow {
    pub transition_id: Uuid,
    pub transformation_name: String,
    pub arguments: Vec<EvalValue>,
    pub invariant_epoch: i32,
    pub invariants_checked: Vec<InvariantCheck>,
    pub asserted_claims: Vec<ClaimInstance>,
    pub retracted_claims: Vec<ClaimInstance>,
    pub emitted_intents: Vec<IntentInstance>,
    pub committed_at: DateTime<Utc>,
}

/// One row of `morpholog.outbox` decoded into typed runtime values.
///
/// `attempt_count` and `last_attempt_at` are included because retry
/// activity is part of the outbox contract — a `pending` row with
/// `attempt_count > 0` and a non-NULL `last_attempt_at` is one a
/// worker has tried and failed, not a fresh enqueue. Inspection
/// helpers should surface that signal.
///
/// `delivered_at` is excluded: [`list_pending_outbox`] filters to
/// `status = 'pending'`, and `delivered_at` is set only when status
/// transitions to `'delivered'`, so it is structurally NULL for
/// every row this helper returns. A future `list_all_outbox` or
/// per-status query would surface it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OutboxRow {
    pub intent_id: Uuid,
    pub transition_id: Uuid,
    pub intent_type: String,
    pub arguments: Vec<EvalValue>,
    pub idempotency_key: String,
    pub status: String,
    pub attempt_count: i32,
    pub enqueued_at: DateTime<Utc>,
    pub last_attempt_at: Option<DateTime<Utc>>,
}

/// Return every currently-admitted claim from `morpholog.claims`.
///
/// Order is `(asserted_at, predicate_name, arguments::text)` — causal
/// admission order, with predicate-then-args as the stable tie-break.
/// Two claims admitted in the same microsecond will appear in a
/// deterministic order across runs.
///
/// This is a `SELECT *` over the entire table. For large states the
/// caller should use SQL directly; the v0 helper is for tests, demos,
/// and small-state inspection.
pub async fn list_claims(pool: &PgPool) -> Result<Vec<ClaimInstance>, PgError> {
    let rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT predicate_name, arguments
         FROM morpholog.claims
         ORDER BY asserted_at, predicate_name, arguments::text",
    )
    .fetch_all(pool)
    .await
    .map_err(classify)?;

    rows.into_iter()
        .map(|(predicate, args_json)| {
            Ok(ClaimInstance {
                predicate,
                args: serde_json::from_value(args_json)?,
            })
        })
        .collect()
}

/// Return every committed audit row from `morpholog.audit`, ordered by
/// `(committed_at, transition_id)` — causal commit order with the
/// `transition_id` PRIMARY KEY (UUIDv7, time-ordered) as the stable
/// tie-break.
///
/// All five JSONB columns are decoded through the codec; the caller
/// receives typed values, not raw `serde_json::Value`. A decoding error
/// surfaces as [`PgError::Encoding`] — that should never happen against
/// a database the runtime itself wrote to, and indicates corruption or
/// out-of-band tampering.
pub async fn list_audit_rows(pool: &PgPool) -> Result<Vec<AuditRow>, PgError> {
    type Row = (
        Uuid,
        String,
        serde_json::Value,
        i32,
        serde_json::Value,
        serde_json::Value,
        serde_json::Value,
        serde_json::Value,
        DateTime<Utc>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT transition_id, transformation_name, arguments,
                invariant_epoch, invariants_checked,
                asserted_claims, retracted_claims, emitted_intents,
                committed_at
         FROM morpholog.audit
         ORDER BY committed_at, transition_id",
    )
    .fetch_all(pool)
    .await
    .map_err(classify)?;

    rows.into_iter()
        .map(
            |(
                transition_id,
                transformation_name,
                args_json,
                invariant_epoch,
                invariants_checked_json,
                asserted_json,
                retracted_json,
                intents_json,
                committed_at,
            )| {
                Ok(AuditRow {
                    transition_id,
                    transformation_name,
                    arguments: serde_json::from_value(args_json)?,
                    invariant_epoch,
                    invariants_checked: serde_json::from_value(invariants_checked_json)?,
                    asserted_claims: serde_json::from_value(asserted_json)?,
                    retracted_claims: serde_json::from_value(retracted_json)?,
                    emitted_intents: serde_json::from_value(intents_json)?,
                    committed_at,
                })
            },
        )
        .collect()
}

/// Return outbox rows whose `status = 'pending'`, ordered by
/// `(enqueued_at, intent_id)` — the same causal-order-with-PK-tie-break
/// pattern used elsewhere. This is the natural "what does the worker
/// have to deliver?" query.
///
/// Delivered and failed rows are excluded; a future `list_all_outbox`
/// or status-filtered helper would surface them. v0 only exposes the
/// in-flight queue.
pub async fn list_pending_outbox(pool: &PgPool) -> Result<Vec<OutboxRow>, PgError> {
    type Row = (
        Uuid,
        Uuid,
        String,
        serde_json::Value,
        String,
        String,
        i32,
        DateTime<Utc>,
        Option<DateTime<Utc>>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT intent_id, transition_id, intent_type, arguments,
                idempotency_key, status, attempt_count, enqueued_at,
                last_attempt_at
         FROM morpholog.outbox
         WHERE status = 'pending'
         ORDER BY enqueued_at, intent_id",
    )
    .fetch_all(pool)
    .await
    .map_err(classify)?;

    rows.into_iter()
        .map(
            |(
                intent_id,
                transition_id,
                intent_type,
                args_json,
                idempotency_key,
                status,
                attempt_count,
                enqueued_at,
                last_attempt_at,
            )| {
                Ok(OutboxRow {
                    intent_id,
                    transition_id,
                    intent_type,
                    arguments: serde_json::from_value(args_json)?,
                    idempotency_key,
                    status,
                    attempt_count,
                    enqueued_at,
                    last_attempt_at,
                })
            },
        )
        .collect()
}

#[cfg(test)]
mod tests {
    use super::is_serialization_failure_code;

    /// Pins the SQLSTATE used to identify PostgreSQL SSI serialization
    /// failures. If anyone changes the magic string `"40001"` this test
    /// fails — the retry contract cannot regress silently.
    #[test]
    fn sqlstate_40001_classified_as_serialization_failure() {
        assert!(is_serialization_failure_code(Some("40001")));
    }

    /// Negative cases: other SQLSTATEs and the absence of a code must not
    /// be treated as retryable serialization failures.
    #[test]
    fn other_sqlstates_are_not_serialization_failures() {
        assert!(!is_serialization_failure_code(None));
        assert!(!is_serialization_failure_code(Some("40000")));
        assert!(!is_serialization_failure_code(Some("23505"))); // unique_violation
        assert!(!is_serialization_failure_code(Some("40P01"))); // deadlock_detected
    }
}
