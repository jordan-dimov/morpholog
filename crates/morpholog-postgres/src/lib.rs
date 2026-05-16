//! Morpholog PostgreSQL persistence adapter.
//!
//! Thin async I/O layer around the existing synchronous
//! [`morpholog_core::propose`] kernel. The kernel itself is unchanged.
//!
//! See `docs/postgres-persistence-v0.md` for the design pin and
//! `crates/morpholog-core/sql/schema.sql` for the canonical schema.

use morpholog_core::{
    ClaimInstance, EvalError, EvalValue, IntentInstance, Invariant, Outcome, State, Transformation,
    propose,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use std::collections::HashSet;
use uuid::Uuid;

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
#[derive(Debug, Clone)]
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

/// Audit JSON shape for `invariants_checked`: an array of objects with
/// `name` and `version` fields. Self-describing audit data is preferred
/// over tuple compactness.
#[derive(Serialize, Deserialize)]
struct CheckedInvariant {
    name: String,
    version: u32,
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
    let checked: Vec<CheckedInvariant> = invariants
        .iter()
        .map(|inv| CheckedInvariant {
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
