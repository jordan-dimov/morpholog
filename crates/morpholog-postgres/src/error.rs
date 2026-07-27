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

    /// The database schema is older than this binary: a query named a
    /// column the table does not have.
    ///
    /// Every query in this crate is verified against `sql/schema.sql` at
    /// build time through the committed `.sqlx/` cache, so this cannot be a
    /// query bug at runtime - it means the database has not had this
    /// release's migrations applied. Worth its own variant because the raw
    /// error names an internal query position and no remedy, and because
    /// the failure hides: commits keep working, and the first refusal is
    /// what breaks.
    #[error(
        "the database schema is behind this binary ({detail}). \
         Apply the migrations in crates/morpholog-core/sql/migrations/ that \
         postdate your database - `morpholog init` never migrates - then retry. \
         Every query here is checked against the schema at build time, so a \
         missing column means the database is out of date, not the query."
    )]
    SchemaBehind { detail: String },
    /// A supplied `transition_id` does not name an existing audit row.
    /// Returned by the as-of helpers when the caller asks for state at
    /// a coordinate that does not correspond to any committed
    /// transition. The contract is "exists or error": every unknown id
    /// - smaller, larger, or between known ids - is rejected here.
    #[error("transition_id {0} not found in morpholog.audit")]
    TransitionNotFound(Uuid),
    /// A transition selected for disclosure is not in the prefix the
    /// covering checkpoint commits to - it may exist in the audit log
    /// but after the checkpoint, so [`PgError::TransitionNotFound`]
    /// would lie. The remedy differs too: checkpoint later, or select
    /// an earlier covering checkpoint's contents.
    #[error(
        "transition {id} is not in the prefix the covering checkpoint \
         (tree_size {tree_size}) commits to"
    )]
    TransitionNotCovered { id: Uuid, tree_size: i64 },
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
         the role the writers use, or grant pg_read_all_stats \
         (or, on a managed host where neither is possible, assert the \
         audit-writing roles explicitly with --writer-role)"
    )]
    StatVisibility { hidden: i64 },
    /// A writer assertion named a role that does not exist - almost
    /// certainly a typo, refused before it can silently filter nothing.
    #[error(
        "asserted writer role(s) do not exist: {}",
        roles.join(", ")
    )]
    WriterRoleUnknown { roles: Vec<String> },
    /// The catalog shows non-superuser roles that can write
    /// `morpholog.audit` (directly, by inheritance, or via SET ROLE)
    /// and were not asserted, so a horizon over the asserted sessions
    /// alone would be unsound.
    #[error(
        "role(s) not in the asserted writer set can write morpholog.audit: {}; \
         assert them with --writer-role too, or revoke their access",
        missing.join(", ")
    )]
    WriterAssertionIncomplete { missing: Vec<String> },
    /// A session of an ASSERTED writer role is hidden from this role in
    /// `pg_stat_activity`. The assertion cannot compensate for that:
    /// the asserted writers' own sessions must be visible (they are by
    /// construction when the asserted role is the connecting role).
    #[error(
        "{hidden} session(s) of asserted writer roles are hidden from this \
         role in pg_stat_activity; connect as the role the writers use, or \
         grant pg_read_all_stats"
    )]
    WriterSessionsHidden { hidden: i64 },
    /// An empty writer assertion is vacuous, never a lawful "no one
    /// writes audit" claim - omit the assertion to get the default
    /// all-sessions horizon instead.
    #[error(
        "an empty writer-role assertion is vacuous; name the role(s) whose \
         sessions write morpholog.audit, or omit the assertion"
    )]
    WriterAssertionEmpty,
    /// A [`morpholog_core::Transition`] named a transformation the compiled programme
    /// does not declare. Surfaced by the `propose_against_pg*` facade
    /// when it resolves `transition.transformation_name` against the
    /// [`morpholog_core::CompiledProgram`].
    #[error("no transformation named `{name}` in the programme")]
    UnknownTransformation { name: TransformationName },
    /// `export_pack` found no checkpoint to cover the requested prefix -
    /// the chain is empty, or no checkpoint exists at the requested size.
    #[error(
        "no checkpoint to export; run `audit checkpoint` first (or pass an existing --tree-size)"
    )]
    NoCheckpoint,
    /// `export_window` was given a full anchor (`--from-anchor`) whose tree
    /// head does not match the stored checkpoint at its size. The
    /// externally-held anchor is the trust object, so export refuses rather
    /// than silently exporting a window from a possibly-diverged stored
    /// checkpoint.
    #[error(
        "the supplied anchor does not match the stored checkpoint at tree_size {tree_size}; \
         the stored start has diverged from the anchor you hold"
    )]
    AnchorDivergedFromStart { tree_size: i64 },
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
/// Is this SQLSTATE the PostgreSQL `undefined_column` code (`42703`)?
pub(crate) fn is_undefined_column_code(code: Option<&str>) -> bool {
    code == Some("42703")
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

/// As [`classify`], plus: a missing column means the database is behind.
///
/// Only for queries written with `sqlx::query!` / `query_as!` /
/// `query_scalar!`, which the committed `.sqlx/` cache verifies against
/// `sql/schema.sql` at build time. There the inference holds - the query
/// cannot name a column the head schema lacks, so the database must be the
/// out-of-date one.
///
/// It does NOT hold for raw or generated SQL. The `raw_sql(SCHEMA_SQL)`
/// bootstrap and the `SET TRANSACTION` statements go through [`classify`]
/// instead: a typo in `schema.sql` would otherwise tell an operator
/// provisioning a FRESH database to go and apply migrations, which is
/// confidently wrong. New raw-SQL sites get [`classify`] by default and are
/// right to.
pub(crate) fn classify_checked_query(err: sqlx::Error) -> PgError {
    let code = err
        .as_database_error()
        .and_then(sqlx::error::DatabaseError::code);
    if is_undefined_column_code(code.as_deref()) {
        return PgError::SchemaBehind {
            detail: err
                .as_database_error()
                .map_or_else(|| err.to_string(), ToString::to_string),
        };
    }
    classify(err)
}
#[cfg(test)]
mod tests {
    use super::{is_serialization_failure_code, is_undefined_column_code};
    use sqlx::error::DatabaseError;
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

    /// Pins `"42703"` the same way, so the upgrade diagnosis cannot regress
    /// into a raw database error nobody can act on.
    #[test]
    fn undefined_column_code_is_42703() {
        assert!(is_undefined_column_code(Some("42703")));
        assert!(!is_undefined_column_code(Some("42P01")));
        assert!(!is_undefined_column_code(None));
    }

    /// The two classifiers disagree about a real `undefined_column`, which
    /// is the whole point of splitting them.
    ///
    /// A checked query cannot name a column the head schema lacks - the
    /// build fails first - so at runtime the database must be behind. Raw
    /// SQL carries no such guarantee: the `raw_sql(SCHEMA_SQL)` bootstrap
    /// could name a bad column on a FRESH database, and telling that
    /// operator to apply migrations would be confidently wrong.
    ///
    /// Skipped without a database, like the rest of the PG-gated suites.
    #[tokio::test]
    async fn raw_sql_is_not_diagnosed_as_a_stale_schema() {
        let Ok(url) = std::env::var("DATABASE_URL") else {
            return;
        };
        let url = crate::with_default_user(&url);
        let pool = sqlx::PgPool::connect(&url).await.expect("connect");
        // A genuine 42703 that touches nothing: no DDL, no shared state.
        let err = sqlx::query("SELECT no_such_column FROM (SELECT 1) AS t")
            .execute(&pool)
            .await
            .expect_err("selecting an absent column must fail");
        assert_eq!(
            err.as_database_error()
                .and_then(DatabaseError::code)
                .as_deref(),
            Some("42703"),
            "the fixture must actually produce the code under test"
        );

        let raw = super::classify(err);
        assert!(
            matches!(raw, super::PgError::Database(_)),
            "raw SQL must not be diagnosed as a stale schema, got {raw:?}"
        );

        let err = sqlx::query("SELECT no_such_column FROM (SELECT 1) AS t")
            .execute(&pool)
            .await
            .expect_err("selecting an absent column must fail");
        let checked = super::classify_checked_query(err);
        assert!(
            matches!(checked, super::PgError::SchemaBehind { .. }),
            "a checked query's missing column IS a stale schema, got {checked:?}"
        );
    }
}
