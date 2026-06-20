use crate::error::{PgError, classify};
use chrono::{DateTime, Utc};
use morpholog_core::{EvalValue, Subject, TransformationName};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// One row of `morpholog.rejections` decoded into typed runtime
/// values: who proposed what, and which rule refused it.
///
/// Operational evidence, not part of the legitimacy-grade audit
/// record - written after each rollback, at-most-once. `rule` is the
/// refusing invariant's name for `kind = "invariant"` and the
/// rendered gate expression for the gate kinds;
/// `invariant_version` is `None` for gates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RejectionRow {
    pub rejection_id: Uuid,
    pub transformation_name: TransformationName,
    pub arguments: Vec<EvalValue>,
    #[serde(with = "morpholog_core::actor_repr")]
    pub actor: Subject,
    pub kind: String,
    pub rule: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invariant_version: Option<i64>,
    pub reason: String,
    pub rejected_at: DateTime<Utc>,
}

/// Return every recorded rejection from `morpholog.rejections`,
/// ordered by `(rejected_at, rejection_id)` - the same keyset order
/// coverage replays in.
pub async fn list_rejection_rows(pool: &PgPool) -> Result<Vec<RejectionRow>, PgError> {
    let rows = sqlx::query!(
        "SELECT rejection_id, transformation_name, arguments, actor,
                kind, rule, invariant_version, reason, rejected_at
         FROM morpholog.rejections
         ORDER BY rejected_at, rejection_id",
    )
    .fetch_all(pool)
    .await
    .map_err(classify)?;

    rows.into_iter()
        .map(|row| {
            Ok(RejectionRow {
                rejection_id: row.rejection_id,
                transformation_name: TransformationName::from(row.transformation_name),
                arguments: serde_json::from_value(row.arguments)?,
                actor: match serde_json::from_value::<EvalValue>(row.actor)? {
                    EvalValue::Subject(s) => s,
                    other => {
                        return Err(PgError::InvalidState(format!(
                            "rejection actor is not a subject: {other:?}"
                        )));
                    }
                },
                kind: row.kind,
                rule: row.rule,
                invariant_version: row.invariant_version,
                reason: row.reason,
                rejected_at: row.rejected_at,
            })
        })
        .collect()
}
