use crate::error::{PgError, classify_checked_query};
use jiff::Timestamp;
use morpholog_core::{EvalValue, Subject, TransformationName, WitnessBinding};
use serde::Serialize;
use sqlx::PgPool;
use uuid::Uuid;

/// One row of `morpholog.rejections` decoded into typed runtime
/// values: who proposed what, and which rule refused it.
///
/// Operational evidence, not part of the audit record: written after each
/// rollback, at most once. `rule` is the invariant's name for
/// `kind = "invariant"`; for a gate it is the gate's name, or its rendered
/// expression when it has none. `invariant_version` is `None` for gates.
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
    /// could not pin the failure to one iteration, and on older rows. A
    /// lead to follow, never proof of what a refusal saw.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub witness: Option<Vec<WitnessBinding>>,
    #[serde(with = "crate::wire_time")]
    pub rejected_at: Timestamp,
}

/// The most recent `limit` refusals, newest first.
///
/// Bounded on purpose: the log grows with every refusal, so an unbounded
/// read would fail hardest during a storm of refusals, just when it matters.
/// Deeper history comes from a larger limit.
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
                rejected_at: row.rejected_at.into(),
            })
        })
        .collect()
}
