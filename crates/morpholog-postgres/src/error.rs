use jiff::Timestamp;
use morpholog_core::{EvalError, Subject, TransformationName};
use uuid::Uuid;
/// Errors returned by the PostgreSQL adapter.
///
/// A business rejection is **not** an error; it is returned as
/// [`crate::PgProposalOutcome::Rejected`].
///
/// On the proposal path (`propose_against_pg` and its traced twin), every
/// variant except [`PgError::CommitOutcomeUnknown`] means the proposal did
/// not commit: it was rolled back or never reached COMMIT. Other commit
/// sites in this crate make no such promise.
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
    /// Any other database error (connection, schema mismatch, etc.). On
    /// the proposal path: the proposal was not committed.
    #[error(transparent)]
    Database(sqlx::Error),
    /// The proposal's COMMIT failed without a PostgreSQL error response.
    /// It may have taken effect: the connection may have dropped after the
    /// server made it durable. Read the record before re-submitting.
    #[error("the commit outcome is unknown: {0}")]
    CommitOutcomeUnknown(sqlx::Error),
    /// The proposal was rejected and rolled back, but the rejection could
    /// not be written to the operational log. The verdict stands; only its
    /// record is missing.
    #[error("the rejection was decided but could not be recorded: {0}")]
    RejectionLogFailure(Box<PgError>),
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
    /// The checked queries are verified against `sql/schema.sql` at build
    /// time, so this means the database lacks this release's migrations.
    /// Its own variant because the raw error gives no remedy, and the
    /// failure can hide until the first refusal.
    #[error(
        "the database schema is behind this binary ({detail}). \
         Run `morpholog migrate` to bring it up to date - the migrations are \
         embedded in this binary, so nothing needs fetching, and \
         `morpholog init` deliberately never migrates. Every query here is \
         checked against the schema at build time, so a missing column means \
         the database is out of date, not the query."
    )]
    SchemaBehind { detail: String },
    /// A supplied `transition_id` does not name an existing audit row.
    /// Every unknown id is refused, including one that sorts between known
    /// ids.
    #[error("transition_id {0} not found in morpholog.audit")]
    TransitionNotFound(Uuid),
    /// A transition selected for disclosure is not in the prefix the
    /// covering checkpoint commits to. It may exist after the checkpoint,
    /// so [`PgError::TransitionNotFound`] would be wrong. Remedy:
    /// checkpoint later, or select from an earlier checkpoint's contents.
    #[error(
        "transition {id} is not in the prefix the covering checkpoint \
         (tree_size {tree_size}) commits to"
    )]
    TransitionNotCovered { id: Uuid, tree_size: i64 },
    /// A transformation emitted the same intent (same name and args) more
    /// than once, so two outbox rows collided on the idempotency key
    /// (SQLSTATE 23505). The whole transformation rolls back. A modelling
    /// bug, not a transient condition.
    #[error(
        "transformation emitted a duplicate intent (same name and args); \
         outbox idempotency keys collided"
    )]
    DuplicateIntent,
    /// An `--as-of` timestamp earlier than every committed transition, so
    /// there is no state to reconstruct. Distinct from
    /// [`PgError::TransitionNotFound`]: the remedy is a later instant, not a
    /// corrected id.
    #[error("no transition committed at or before {0}")]
    NoTransitionAtOrBefore(Timestamp),
    /// `pg_stat_activity` hides sessions from this role, so the audit
    /// resume horizon cannot be computed soundly: a hidden writer would
    /// drop out of the minimum and the tail could skip its row.
    #[error(
        "{hidden} session(s) in pg_stat_activity are hidden from this role, \
         so a lossless audit resume horizon cannot be computed; connect as \
         the role the writers use, or grant pg_read_all_stats \
         (or, on a managed host where neither is possible, assert the \
         audit-writing roles explicitly with --writer-role)"
    )]
    StatVisibility { hidden: i64 },
    /// A writer assertion named a role that does not exist, probably a
    /// typo. Refused so it cannot silently filter nothing.
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
    /// `pg_stat_activity`. The asserted writers' own sessions must be
    /// visible; they always are when the asserted role is the connecting
    /// role.
    #[error(
        "{hidden} session(s) of asserted writer roles are hidden from this \
         role in pg_stat_activity; connect as the role the writers use, or \
         grant pg_read_all_stats"
    )]
    WriterSessionsHidden { hidden: i64 },
    /// An empty writer assertion is vacuous, not a claim that no one writes
    /// audit. Omit it to get the default all-sessions horizon.
    #[error(
        "an empty writer-role assertion is vacuous; name the role(s) whose \
         sessions write morpholog.audit, or omit the assertion"
    )]
    WriterAssertionEmpty,
    /// A programme declares one of the reserved actor-policy
    /// predicates in a shape the runtime does not match.
    ///
    /// Refused, not ignored: an unrecognised declaration never arms, so a
    /// restriction the author believes is in force would protect nothing.
    /// `morpholog check` reports it too, but callers need not run `check`.
    #[error("actor-assertion policy declaration is unusable: {}", findings.join("; "))]
    ActorPolicyDeclaration { findings: Vec<String> },
    /// The connecting login role is not authorised to propose as the
    /// named actor: an `ActorAssertionRestricted` claim arms the actor
    /// and no `ActorAssertionAuthority` grants this role.
    ///
    /// Never a business rejection: this is someone claiming to be the
    /// actor, not the actor being refused. Nothing is evaluated or
    /// recorded, so a caller cannot manufacture a history of attempts by
    /// an actor they cannot speak for.
    #[error(
        "login role `{login_role}` is not authorised to propose as actor \
         `{actor}`; admit ActorAssertionAuthority({actor}, {login_role}) to \
         grant it"
    )]
    ActorAssertionUnauthorised { actor: Subject, login_role: String },
    /// A [`morpholog_core::Transition`] named a transformation the
    /// [`morpholog_core::CompiledProgram`] does not declare.
    #[error("no transformation named `{name}` in the programme")]
    UnknownTransformation { name: TransformationName },
    /// `export_pack` found no checkpoint to cover the requested prefix -
    /// the chain is empty, or no checkpoint exists at the requested size.
    #[error(
        "no checkpoint to export; run `audit checkpoint` first (or pass an existing --tree-size)"
    )]
    NoCheckpoint,
    /// A checkpoint commits to more audit rows than the log now holds under
    /// it, so no pack can be exported against it. The covered prefix is no
    /// longer all present.
    #[error(
        "checkpoint commits to {tree_size} audit rows but only {rows_present} are present; \
         the audit log under it is incomplete"
    )]
    AuditPrefixIncomplete { tree_size: i64, rows_present: i64 },
    /// `export_window` was given an anchor (`--from-anchor`) whose tree head
    /// does not match the stored checkpoint at its size. The anchor held
    /// outside is what is trusted, so export refuses.
    #[error(
        "the supplied anchor does not match the stored checkpoint at tree_size {tree_size}; \
         the stored start has diverged from the anchor you hold"
    )]
    AnchorDivergedFromStart { tree_size: i64 },
    /// No `AuditSigningKey` claim authorises the signing key as of the
    /// prefix being signed, and the resume horizon is withholding no
    /// committed rows. The key itself is the problem. Signing refuses
    /// rather than produce a checkpoint verification would judge
    /// `unauthorized_key`.
    #[error(
        "signing key is not authorised as AuditSigningKey({key_id}, {purpose}, \
         {public_key}) as of tree_size {tree_size}; propose an AuditSigningKey \
         admission for it, or sign with a key the ledger has authorised"
    )]
    SigningKeyUnauthorised {
        key_id: String,
        purpose: String,
        public_key: String,
        tree_size: i64,
    },
    /// The signing key is not authorised in the stable prefix, AND
    /// committed rows sit at or above the resume horizon, so a recent
    /// authorisation may not be visible yet. Distinct from
    /// [`PgError::SigningKeyUnauthorised`] because it may be transient: the
    /// horizon advances once older open transactions end. The count is a
    /// floor: in-flight transactions' rows cannot be seen to count.
    #[error(
        "signing key is not authorised as AuditSigningKey({key_id}, {purpose}, \
         {public_key}) as of tree_size {tree_size}, and \
         {committed_beyond_horizon} committed audit row(s) at or above the \
         resume horizon ({horizon}) are outside this stable prefix; if the \
         key was authorised in one of those rows the condition is transient - \
         retry after the horizon advances (it is held back while older \
         transactions stay open)"
    )]
    SigningKeyUnauthorisedAtTruncatedPrefix {
        key_id: String,
        purpose: String,
        public_key: String,
        tree_size: i64,
        committed_beyond_horizon: i64,
        horizon: Timestamp,
    },
}
/// Is this SQLSTATE the SSI serialization-failure code (`40001`)? A pure
/// function so it can be tested without mocking `sqlx::DatabaseError`.
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
/// Maps a `sqlx::Error` to a [`PgError`]: SQLSTATE 40001 is the retryable
/// [`PgError::SerializationFailure`], a 23505 on the outbox idempotency key
/// is [`PgError::DuplicateIntent`], and everything else is
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

/// Classify a failed proposal COMMIT. A PostgreSQL error response means
/// the server rolled back, so [`classify`] applies. Any other failure
/// (dropped connection, broken protocol, closed pool) has no server
/// verdict and the commit may have happened, so it is unknown. A false
/// "unknown" costs a read; a false "not committed" could duplicate a
/// business action.
pub(crate) fn classify_commit(err: sqlx::Error) -> PgError {
    match err {
        sqlx::Error::Database(_) => classify(err),
        other => PgError::CommitOutcomeUnknown(other),
    }
}

/// As [`classify`], plus: a missing column means the database is behind.
///
/// Only for `sqlx::query!` / `query_as!` / `query_scalar!` queries, which
/// are checked against `sql/schema.sql` at build time, so the database must
/// be the out-of-date side.
///
/// Raw or generated SQL uses [`classify`]: a typo in `schema.sql` would
/// otherwise tell an operator setting up a FRESH database to apply
/// migrations.
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
    use super::{classify_commit, is_serialization_failure_code, is_undefined_column_code};
    use crate::PgError;
    use sqlx::error::DatabaseError;

    /// The commit boundary: a server error response is a known
    /// non-commit, a failure without one is unknown.
    #[test]
    fn a_commit_failure_without_a_server_verdict_is_unknown() {
        let dropped = sqlx::Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "connection reset by peer",
        ));
        assert!(matches!(
            classify_commit(dropped),
            PgError::CommitOutcomeUnknown(sqlx::Error::Io(_))
        ));
        assert!(matches!(
            classify_commit(sqlx::Error::Protocol("half a message".into())),
            PgError::CommitOutcomeUnknown(_)
        ));
        assert!(matches!(
            classify_commit(sqlx::Error::PoolClosed),
            PgError::CommitOutcomeUnknown(_)
        ));
    }
    /// Pins `"40001"` so the retry contract cannot regress silently.
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

    /// Pins `"42703"` so the upgrade diagnosis cannot regress silently.
    #[test]
    fn undefined_column_code_is_42703() {
        assert!(is_undefined_column_code(Some("42703")));
        assert!(!is_undefined_column_code(Some("42P01")));
        assert!(!is_undefined_column_code(None));
    }

    /// The two classifiers must disagree about a real `undefined_column`:
    /// only a build-checked query proves the database is behind.
    ///
    /// Skipped without a database.
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
