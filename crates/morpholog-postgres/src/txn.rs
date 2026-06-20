use crate::error::{PgError, classify};
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
