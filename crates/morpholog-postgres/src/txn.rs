use crate::error::{PgError, classify, classify_checked_query};
use morpholog_core::Subject;
use sqlx::{PgPool, Postgres, Transaction};

/// The transaction isolation levels the adapter opens. A closed enum,
/// not a `&str`, so the concurrency contract cannot be set to an
/// arbitrary level - every isolation the adapter uses is named here.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TxIsolation {
    Serializable,
    SerializableReadOnlyDeferrable,
    RepeatableRead,
    RepeatableReadReadOnly,
}

impl TxIsolation {
    /// The full `SET TRANSACTION` statement as a `'static` literal, so
    /// the per-transaction setup allocates nothing (the level is part of
    /// the constant, not interpolated at runtime).
    fn set_statement(self) -> &'static str {
        match self {
            TxIsolation::Serializable => "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE",
            TxIsolation::SerializableReadOnlyDeferrable => {
                "SET TRANSACTION ISOLATION LEVEL SERIALIZABLE READ ONLY DEFERRABLE"
            }
            TxIsolation::RepeatableRead => "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ",
            TxIsolation::RepeatableReadReadOnly => {
                "SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY"
            }
        }
    }
}

/// Begin a transaction and set its isolation level - the ritual every
/// adapter entry point opens with, in one auditable place. The `SET`
/// stays raw control SQL (not a `query!` macro); the statement is a
/// `'static` literal because [`TxIsolation`] is closed, so this hot path
/// allocates nothing.
pub(crate) async fn begin_isolated_tx(
    pool: &PgPool,
    isolation: TxIsolation,
) -> Result<Transaction<'_, Postgres>, PgError> {
    let mut tx = pool.begin().await.map_err(classify)?;
    sqlx::query(isolation.set_statement())
        .execute(&mut *tx)
        .await
        .map_err(classify)?;
    Ok(tx)
}

/// Begin the proposal transaction, resolve the connection's
/// authenticated identity, and settle whether that identity may
/// propose as this actor - before anything is loaded or evaluated.
///
/// One seam for every durable proposal path. The traced and untraced
/// paths each open their own transaction, and a policy check wired
/// into only one of them would be a gate you could walk around by
/// asking for a trace.
///
/// `session_user` is the role PostgreSQL authenticated at login: it
/// is immune to `SET ROLE`, so a caller cannot shed or borrow an
/// identity through this adapter. A superuser can still change it
/// with `SET SESSION AUTHORIZATION` - the same accepted residue as a
/// superuser writing audit rows directly.
///
/// The role travels back with the transaction because the audit row
/// records it too, and reading it twice would leave room for the
/// identity that was CHECKED and the identity that is RECORDED to
/// differ.
pub(crate) async fn begin_authorised_proposal_tx<'a>(
    pool: &'a PgPool,
    actor: &Subject,
) -> Result<(Transaction<'a, Postgres>, String), PgError> {
    let mut tx = begin_isolated_tx(pool, TxIsolation::Serializable).await?;
    let login_role = sqlx::query_scalar!(r#"SELECT session_user AS "session_user!""#)
        .fetch_one(&mut *tx)
        .await
        .map_err(classify_checked_query)?;
    crate::actor_policy::authorise(&mut tx, actor, &login_role).await?;
    Ok((tx, login_role))
}
