use chrono::{DateTime, Utc};
use morpholog_core::{EvalError, TransformationName};
use uuid::Uuid;
/// Errors returned by the PostgreSQL adapter.
///
/// Lawful business rejection is **not** an error - it is returned as
/// [`crate::PgProposalOutcome::Rejected`]. This enum captures only conditions
/// where the caller cannot or should not proceed as if the kernel had
/// run successfully.
#[derive(thiserror::Error, Debug)]
pub enum PgError {
    /// SQLSTATE 40001 from PostgreSQL SSI. The transaction should be
    /// retried by the caller.
    #[error("SERIALIZABLE retry needed (SQLSTATE 40001)")]
    SerializationFailure,
    /// Evaluation error from the in-memory kernel (e.g. unbound variable,
    /// type mismatch). Distinct from a business [`crate::PgProposalOutcome::Rejected`].
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
    /// A supplied `transition_id` does not name an existing audit row.
    /// Returned by the as-of helpers when the caller asks for state at
    /// a coordinate that does not correspond to any committed
    /// transition. The contract is "exists or error": every unknown id
    /// - smaller, larger, or between known ids - is rejected here.
    #[error("transition_id {0} not found in morpholog.audit")]
    TransitionNotFound(Uuid),
    /// A transformation emitted the same intent (same name and args)
    /// more than once, so two outbox rows collided on the
    /// deterministic idempotency key (SQLSTATE 23505 on the outbox
    /// idempotency-key unique constraint). The whole transformation
    /// rolls back. Named distinctly from [`PgError::Database`] because
    /// it is a modelling bug, not a transient condition: identical
    /// duplicate intents must not silently produce two outbox rows.
    #[error(
        "transformation emitted a duplicate intent (same name and args); \
         outbox idempotency keys collided"
    )]
    DuplicateIntent,
    /// An `--as-of` timestamp earlier than every committed transition:
    /// there is no state to reconstruct at or before that instant.
    /// Distinct from [`PgError::TransitionNotFound`] because the caller
    /// supplied a time, not an id, and the remedy differs (pick a later
    /// instant vs fix a wrong id).
    #[error("no transition committed at or before {0}")]
    NoTransitionAtOrBefore(DateTime<Utc>),
    /// `pg_stat_activity` hides sessions from this role, so the audit
    /// resume horizon cannot be computed soundly - a hidden writer
    /// would silently fall out of the minimum and the tail could skip
    /// its row. The remedy is in the message because the condition is
    /// configuration, not code.
    #[error(
        "{hidden} session(s) in pg_stat_activity are hidden from this role, \
         so a lossless audit resume horizon cannot be computed; connect as \
         the role the writers use, or grant pg_read_all_stats"
    )]
    StatVisibility { hidden: i64 },
    /// A [`morpholog_core::Transition`] named a transformation the compiled programme
    /// does not declare. Surfaced by the `propose_against_pg*` facade
    /// when it resolves `transition.transformation_name` against the
    /// [`morpholog_core::CompiledProgram`].
    #[error("no transformation named `{name}` in the programme")]
    UnknownTransformation { name: TransformationName },
}
/// Is this SQLSTATE the PostgreSQL serialization-failure code
/// (`40001`) returned by SSI when a SERIALIZABLE transaction cannot be
/// linearised? Pure function so the magic string can be unit-tested
/// without mocking `sqlx::DatabaseError`.
pub(crate) fn is_serialization_failure_code(code: Option<&str>) -> bool {
    code == Some("40001")
}
/// Is this SQLSTATE the PostgreSQL `unique_violation` code (`23505`)?
pub(crate) fn is_unique_violation_code(code: Option<&str>) -> bool {
    code == Some("23505")
}
/// Maps a `sqlx::Error` to a [`PgError`], recognising SQLSTATE 40001
/// (PostgreSQL SSI serialization failure) as the distinct retryable
/// variant and a 23505 on the outbox idempotency-key constraint as
/// [`PgError::DuplicateIntent`]. All other errors propagate as
/// [`PgError::Database`].
pub(crate) fn classify(err: sqlx::Error) -> PgError {
    let db = err.as_database_error();
    let code = db.and_then(sqlx::error::DatabaseError::code);
    if is_serialization_failure_code(code.as_deref()) {
        return PgError::SerializationFailure;
    }
    if is_unique_violation_code(code.as_deref())
        && db
            .and_then(|e| e.constraint())
            .is_some_and(|c| c.contains("idempotency_key"))
    {
        return PgError::DuplicateIntent;
    }
    PgError::Database(err)
}
#[cfg(test)]
mod tests {
    use super::is_serialization_failure_code;
    /// Pins the `"40001"` magic string so the retry contract cannot
    /// regress silently.
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
