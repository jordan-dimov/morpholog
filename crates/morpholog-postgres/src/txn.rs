use crate::error::{PgError, classify, classify_checked_query};
use morpholog_core::Subject;
use sqlx::{PgPool, Postgres, Transaction};

/// The transaction isolation levels the adapter opens. A closed enum, so
/// no caller can pick an arbitrary level.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TxIsolation {
    Serializable,
    SerializableReadOnlyDeferrable,
    RepeatableRead,
    RepeatableReadReadOnly,
}

impl TxIsolation {
    /// The full `SET TRANSACTION` statement, as a `'static` literal so
    /// per-transaction setup allocates nothing.
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

/// Begin a transaction and set its isolation level. Every adapter entry
/// point opens this way.
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
/// Every durable proposal path goes through here, traced or not, so the
/// policy check cannot be skipped by asking for a trace.
///
/// `session_user` is the role PostgreSQL authenticated at login. `SET ROLE`
/// cannot change it, so a caller cannot shed or borrow an identity. A
/// superuser still can, via `SET SESSION AUTHORIZATION`; that is accepted,
/// like a superuser writing audit rows directly.
///
/// The role is returned with the transaction for the audit row, so the
/// identity checked and the identity recorded are the same read.
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
