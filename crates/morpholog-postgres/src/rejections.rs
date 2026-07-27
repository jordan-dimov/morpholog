use crate::error::{PgError, classify_checked_query};
use chrono::{DateTime, Utc};
use morpholog_core::{EvalValue, Subject, TransformationName, WitnessBinding};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// One row of `morpholog.rejections` decoded into typed runtime
/// values: who proposed what, and which rule refused it.
///
/// Operational evidence, not part of the legitimacy-grade audit
/// record - written after each rollback, at-most-once. `rule` is the
/// refusing invariant's name for `kind = "invariant"`, and for a gate it
/// is the gate's own name when it has one, falling back to the rendered
/// expression when it does not; `invariant_version` is `None` for gates.
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
    /// The values the refused rule was reading. Absent when the kernel
    /// could not pin the failure to one iteration, and for rows written
    /// before the column existed. Diagnostic: it inherits this log's
    /// at-most-once operational standing, so it is a lead to follow, never
    /// proof of what a refusal saw.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub witness: Option<Vec<WitnessBinding>>,
    pub rejected_at: DateTime<Utc>,
}

/// Return every recorded rejection from `morpholog.rejections`,
/// ordered by `(rejected_at, rejection_id)` - the same keyset order
/// coverage replays in.
/// The most recent `limit` refusals, newest first.
///
/// Bounded on purpose. The question this log answers is "what just refused",
/// and it grows with every refusal - so an unbounded read is the query that
/// fails hardest exactly when a storm makes it interesting. Deeper history
/// comes from a larger limit; a keyset cursor over the
/// `(rejected_at, rejection_id)` index is the shape to add when paging is
/// forced.
pub async fn list_rejection_rows(pool: &PgPool, limit: u32) -> Result<Vec<RejectionRow>, PgError> {
    let rows = sqlx::query!(
        "SELECT rejection_id, transformation_name, arguments, actor,
                kind, rule, invariant_version, reason, witness, rejected_at
         FROM morpholog.rejections
         ORDER BY rejected_at DESC, rejection_id DESC
         LIMIT $1",
        i64::from(limit),
    )
    .fetch_all(pool)
    .await
    .map_err(classify_checked_query)?;

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
                witness: row
                    .witness
                    .map(serde_json::from_value::<Vec<WitnessBinding>>)
                    .transpose()?,
                rejected_at: row.rejected_at,
            })
        })
        .collect()
}
