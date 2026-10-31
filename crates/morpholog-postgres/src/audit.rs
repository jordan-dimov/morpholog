use crate::activation::BASELINE;
use crate::attestation::AuditAttestation;
use crate::audit_pages::AuditPages;
use crate::error::{PgError, classify, classify_checked_query};
use crate::propose::AuditedInvariantCheck;
use crate::txn::{TxIsolation, begin_isolated_tx};
use jiff::Timestamp;
use jiff_sqlx::ToSqlx;
use morpholog_core::{
    ClaimInstance, EvalValue, IntentInstance, PredicateName, Subject, TransformationName,
};
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;
/// One row of `morpholog.audit` decoded into typed runtime values.
///
/// One row per committed transformation. JSONB columns decode through the
/// codec that wrote them, so the round-trip is exact.
///
/// Unknown fields are refused: a row arrives in packs as hostile input,
/// and a field this build cannot place in the leaf encoding must make the
/// row malformed, never silently drop out of the hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditRow {
    pub transition_id: Uuid,
    pub transformation_name: TransformationName,
    pub arguments: Vec<EvalValue>,
    #[serde(with = "morpholog_core::actor_repr")]
    pub actor: Subject,
    pub invariant_epoch: i32,
    pub invariants_checked: Vec<AuditedInvariantCheck>,
    pub asserted_claims: Vec<ClaimInstance>,
    pub retracted_claims: Vec<ClaimInstance>,
    pub emitted_intents: Vec<IntentInstance>,
    #[serde(with = "crate::wire_time")]
    pub committed_at: Timestamp,
    /// How the actor identity was established. Absent on rows written
    /// before attestation existed; those rows keep the original Merkle
    /// leaf encoding, so the field's presence selects the leaf version.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub attestation: Option<AuditAttestation>,
    /// The transformation's parameter names in declaration order, one per
    /// argument, stamped at commit so the row stays readable after the
    /// transformation is retired. Absent on older rows; presence selects
    /// the self-describing leaf encoding.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub parameters: Option<Vec<String>>,
    /// The canonical hash of the whole programme that admitted the row,
    /// as `morpholog hash` prints it. Absent on older rows; presence on a
    /// row that also carries an attestation and parameter names selects
    /// the leaf encoding that commits to it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub model_hash: Option<String>,
    /// The version of the kernel's semantics that decided the row
    /// (`morpholog_core::SEMANTICS_VERSION` when it committed). Absent on
    /// older rows; presence on a row that names its programme selects the
    /// leaf encoding that commits to it. Evidence of which contract made
    /// the decision, not something an integrity check compares with its
    /// own version.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub semantics_version: Option<u32>,
    /// The subjects the act drew from `new Subject()`, in draw order:
    /// with the rest of the row and the history before it, every input
    /// the decision read. Empty when it drew none; absent on older rows,
    /// where presence on a row that names its semantics selects the leaf
    /// encoding that commits to it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub drawn_subjects: Option<Vec<Subject>>,
    /// The row's place in the canonical order: the order its transaction
    /// drew, the act's place within that transaction, and the ordered
    /// timestamp. The four ordering fields come together, absent on rows
    /// from before the order existed and present on every row after,
    /// where they select the leaf encoding that commits to them.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub audit_order: Option<u64>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub act_ordinal: Option<u32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "crate::wire_time::option::serialize",
        deserialize_with = "present_instant"
    )]
    pub ordered_at: Option<Timestamp>,
    /// What the commit did to the claims table, in execution order.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub operations: Option<Vec<Operation>>,
}

/// One row operation a commit performed on the claims table: a delete,
/// an insert, or an insert that found the claim already present.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub op: OperationKind,
    pub predicate: PredicateName,
    pub args: Vec<EvalValue>,
    /// On the activation record's baseline only: the transition that
    /// admitted the claim.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub asserted_in: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OperationKind {
    Del,
    Ins,
    Noop,
}

impl Operation {
    pub(crate) fn new(op: OperationKind, claim: &ClaimInstance) -> Self {
        Operation {
            op,
            predicate: claim.predicate.clone(),
            args: claim.args.clone(),
            asserted_in: None,
        }
    }

    pub fn claim(&self) -> ClaimInstance {
        ClaimInstance {
            predicate: self.predicate.clone(),
            args: self.args.clone(),
        }
    }
}

fn present_instant<'de, D>(d: D) -> Result<Option<Timestamp>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    crate::wire_time::deserialize(d).map(Some)
}

/// A leaf-rung field that is optional by omission only: absent is
/// `None`, and a present JSON `null` is refused rather than read as
/// absent. Presence selects the leaf encoding, so a row has one spelling
/// for "not on this rung".
fn present<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(d).map(Some)
}

/// A row's place in the audit log's one order. Every walk, cursor, sort
/// and latest-row read takes its order from here. Rows from before the
/// canonical order have order and ordinal 0, so they come first, in the
/// order they always had.
// The derived ordering follows field order, which is the log order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AuditPosition {
    pub(crate) audit_order: u64,
    pub(crate) act_ordinal: u32,
    pub(crate) committed_at: Timestamp,
    pub(crate) transition_id: Uuid,
}

impl std::fmt::Display for AuditPosition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "({}, {}, {}, {})",
            self.audit_order, self.act_ordinal, self.committed_at, self.transition_id
        )
    }
}

impl AuditRow {
    /// This row's place in the log order.
    pub fn position(&self) -> AuditPosition {
        AuditPosition {
            audit_order: self.audit_order.unwrap_or(0),
            act_ordinal: self.act_ordinal.unwrap_or(0),
            committed_at: self.committed_at,
            transition_id: self.transition_id,
        }
    }

    /// Check the row is a shape the runtime writes: nothing extra, an
    /// attestation, an attestation plus one name per argument, those plus
    /// the model hash, those plus the semantics version, those plus the
    /// drawn subjects, or those plus the four ordering fields. Each rung
    /// needs the one below it.
    ///
    /// Checked at the database boundary and before hashing, because packs
    /// carry rows as hostile input.
    pub fn validate_shape(&self) -> Result<(), String> {
        let ordering = [
            self.audit_order.is_some(),
            self.act_ordinal.is_some(),
            self.ordered_at.is_some(),
            self.operations.is_some(),
        ];
        if ordering.contains(&true) {
            if ordering.contains(&false) {
                return Err(format!(
                    "audit row {} carries some of its ordering fields but not all",
                    self.transition_id
                ));
            }
            if self.drawn_subjects.is_none() {
                return Err(format!(
                    "audit row {} carries an order without drawn subjects",
                    self.transition_id
                ));
            }
            if self.audit_order == Some(0) {
                return Err(format!(
                    "audit row {} carries order 0; orders start at 1",
                    self.transition_id
                ));
            }
        }
        if self.drawn_subjects.is_some() && self.semantics_version.is_none() {
            return Err(format!(
                "audit row {} records drawn subjects without a semantics version",
                self.transition_id
            ));
        }
        if let Some(version) = self.semantics_version {
            if self.model_hash.is_none() {
                return Err(format!(
                    "audit row {} carries a semantics version without a model hash",
                    self.transition_id
                ));
            }
            if version == 0 {
                return Err(format!(
                    "audit row {} carries semantics version 0; versions start at 1",
                    self.transition_id
                ));
            }
        }
        if let Some(hash) = &self.model_hash {
            if self.attestation.is_none() || self.parameters.is_none() {
                return Err(format!(
                    "audit row {} carries a model hash without an attestation and parameter names",
                    self.transition_id
                ));
            }
            if !is_model_hash(hash) {
                return Err(format!(
                    "audit row {} carries a malformed model hash {hash:?}",
                    self.transition_id
                ));
            }
        }
        match (&self.attestation, &self.parameters) {
            (_, None) => Ok(()),
            (None, Some(_)) => Err(format!(
                "audit row {} carries parameter names but no attestation",
                self.transition_id
            )),
            (Some(_), Some(names)) if names.len() != self.arguments.len() => Err(format!(
                "audit row {} carries {} parameter names for {} arguments",
                self.transition_id,
                names.len(),
                self.arguments.len()
            )),
            (Some(_), Some(_)) => Ok(()),
        }
    }
}
/// Page size for every keyset read over the replay order. One chunk sits
/// in memory at a time.
pub(crate) const REPLAY_CHUNK: i64 = 1024;
/// One raw `morpholog.audit` row as `query_as!` decodes it; turned into
/// an [`AuditRow`] by [`decode_audit_row`].
pub(crate) struct AuditRowRaw {
    transition_id: Uuid,
    transformation_name: String,
    arguments: serde_json::Value,
    actor: serde_json::Value,
    invariant_epoch: i32,
    invariants_checked: serde_json::Value,
    asserted_claims: serde_json::Value,
    retracted_claims: serde_json::Value,
    emitted_intents: serde_json::Value,
    committed_at: Timestamp,
    // Nullable even though a constraint refuses new NULLs: upgraded
    // databases hold NULL on historical rows, and fresh and upgraded
    // databases must describe the column alike for the query checks.
    attestation: Option<serde_json::Value>,
    // Nullable for the same reason: historical rows carry no names.
    parameters: Option<serde_json::Value>,
    // Nullable for the same reason: historical rows name no programme.
    model_hash: Option<String>,
    // Nullable for the same reason; bigint holds the whole u32 range.
    semantics_version: Option<i64>,
    // Nullable for the same reason: historical rows recorded no draws.
    drawn_subjects: Option<serde_json::Value>,
    // 0 on rows from before the canonical order.
    audit_order: i64,
    act_ordinal: i32,
    ordered_at: Option<jiff_sqlx::Timestamp>,
    operations: Option<serde_json::Value>,
}
// The canonical column order, shared by `AuditRowRaw` and every listing
// SELECT (each `query_as!` must spell it out literally):
//   transition_id, transformation_name, arguments, actor,
//   invariant_epoch, invariants_checked,
//   asserted_claims, retracted_claims, emitted_intents, committed_at,
//   attestation, parameters, model_hash, semantics_version, drawn_subjects,
//   audit_order, act_ordinal, ordered_at, operations
pub(crate) fn decode_audit_row(row: AuditRowRaw) -> Result<AuditRow, PgError> {
    let decoded = AuditRow {
        transition_id: row.transition_id,
        transformation_name: TransformationName::from(row.transformation_name),
        arguments: serde_json::from_value(row.arguments)?,
        actor: match serde_json::from_value::<EvalValue>(row.actor)? {
            EvalValue::Subject(s) => s,
            other => {
                return Err(PgError::InvalidState(format!(
                    "audit actor is not a subject: {other:?}"
                )));
            }
        },
        invariant_epoch: row.invariant_epoch,
        invariants_checked: serde_json::from_value(row.invariants_checked)?,
        asserted_claims: serde_json::from_value(row.asserted_claims)?,
        retracted_claims: serde_json::from_value(row.retracted_claims)?,
        emitted_intents: serde_json::from_value(row.emitted_intents)?,
        committed_at: row.committed_at,
        // Strict, like the actor: an unrecognised attestation shape is
        // an error at this boundary, never a value that hashes on.
        attestation: row
            .attestation
            .map(serde_json::from_value::<AuditAttestation>)
            .transpose()?,
        parameters: row
            .parameters
            .map(serde_json::from_value::<Vec<String>>)
            .transpose()?,
        model_hash: row.model_hash,
        semantics_version: row
            .semantics_version
            .map(|v| {
                u32::try_from(v).map_err(|_| {
                    PgError::InvalidState(format!("audit semantics version {v} is not a u32"))
                })
            })
            .transpose()?,
        drawn_subjects: row
            .drawn_subjects
            .map(serde_json::from_value::<Vec<Subject>>)
            .transpose()?,
        audit_order: ordering_field(row.audit_order, row.operations.is_some())?,
        act_ordinal: ordering_field(i64::from(row.act_ordinal), row.operations.is_some())?,
        ordered_at: row.ordered_at.map(jiff_sqlx::Timestamp::to_jiff),
        operations: row
            .operations
            .map(serde_json::from_value::<Vec<Operation>>)
            .transpose()?,
    };
    decoded.validate_shape().map_err(PgError::InvalidState)?;
    Ok(decoded)
}
/// An ordering column as the row carries it: the table stores 0 on a
/// legacy row, which the row leaves absent.
fn ordering_field<T: TryFrom<i64>>(value: i64, ordered: bool) -> Result<Option<T>, PgError> {
    if !ordered {
        return Ok(None);
    }
    T::try_from(value)
        .map(Some)
        .map_err(|_| PgError::InvalidState(format!("audit ordering value {value} is out of range")))
}
/// Return every committed audit row, ordered by `(committed_at,
/// transition_id)`: commit order, with the UUIDv7 key as tie-break.
///
/// A decoding error is [`PgError::Encoding`], which on a database the
/// runtime wrote means corruption or tampering.
///
/// A whole-table fetch for tests and small histories, every page of it.
/// To tail, use [`begin_audit_tail`].
pub async fn list_audit_rows(pool: &PgPool) -> Result<Vec<AuditRow>, PgError> {
    let mut conn = pool.acquire().await.map_err(classify)?;
    let mut pages = AuditPages::new(None);
    let mut rows = Vec::new();
    loop {
        let page = pages.next(&mut conn).await?;
        if page.is_empty() {
            return Ok(rows);
        }
        rows.extend(page);
    }
}
/// One keyset page of audit rows in log order ([`AuditPosition`]):
/// strictly after `cursor` and strictly below `horizon`, each when given.
/// Takes a connection so the caller can hold one snapshot across pages.
///
/// A page holds at most one baseline chunk: it ends before the second,
/// however many rows were asked for, so a short page does not mean the
/// log is exhausted. Only an empty page does.
///
/// `horizon` comes from [`audit_resume_watermark`]; `None` reads to the
/// snapshot's end and loses the lossless-resume guarantee.
// These query texts are mirrored in tests/plan_shapes.rs, which pins their
// plans; a change here belongs there too.
pub async fn list_audit_rows_page(
    conn: &mut sqlx::PgConnection,
    cursor: Option<AuditPosition>,
    horizon: Option<u64>,
    limit: i64,
) -> Result<Vec<AuditRow>, PgError> {
    let rows = match (&cursor, &horizon) {
        (None, None) => {
            sqlx::query_as!(
                AuditRowRaw,
                "SELECT transition_id, transformation_name, arguments, actor,
                        invariant_epoch, invariants_checked,
                        asserted_claims, retracted_claims, emitted_intents, committed_at,
                attestation, parameters, model_hash, semantics_version, drawn_subjects,
                audit_order, act_ordinal, ordered_at, operations
                  FROM (SELECT transition_id, transformation_name, arguments, actor,
                               invariant_epoch, invariants_checked,
                               asserted_claims, retracted_claims, emitted_intents, committed_at,
                       attestation, parameters, model_hash, semantics_version, drawn_subjects,
                       audit_order, act_ordinal, ordered_at, operations,
                               sum((transformation_name = $2)::int) OVER w AS chunks
                        FROM morpholog.audit
                        WINDOW w AS (ORDER BY audit_order, act_ordinal, committed_at, transition_id
                                     ROWS UNBOUNDED PRECEDING)
                        ORDER BY audit_order, act_ordinal, committed_at, transition_id
                        LIMIT $1) s
                  WHERE chunks <= 1
                  ORDER BY audit_order, act_ordinal, committed_at, transition_id",
                limit,
                BASELINE,
            )
            .fetch_all(&mut *conn)
            .await
        }
        (
            Some(AuditPosition {
                audit_order: o,
                act_ordinal: k,
                committed_at: at,
                transition_id: id,
            }),
            None,
        ) => {
            sqlx::query_as!(
                AuditRowRaw,
                "SELECT transition_id, transformation_name, arguments, actor,
                        invariant_epoch, invariants_checked,
                        asserted_claims, retracted_claims, emitted_intents, committed_at,
                attestation, parameters, model_hash, semantics_version, drawn_subjects,
                audit_order, act_ordinal, ordered_at, operations
                  FROM (SELECT transition_id, transformation_name, arguments, actor,
                               invariant_epoch, invariants_checked,
                               asserted_claims, retracted_claims, emitted_intents, committed_at,
                       attestation, parameters, model_hash, semantics_version, drawn_subjects,
                       audit_order, act_ordinal, ordered_at, operations,
                               sum((transformation_name = $6)::int) OVER w AS chunks
                        FROM morpholog.audit
                        WHERE (audit_order, act_ordinal, committed_at, transition_id) > ($4, $5, $2, $3)
                        WINDOW w AS (ORDER BY audit_order, act_ordinal, committed_at, transition_id
                                     ROWS UNBOUNDED PRECEDING)
                        ORDER BY audit_order, act_ordinal, committed_at, transition_id
                        LIMIT $1) s
                  WHERE chunks <= 1
                  ORDER BY audit_order, act_ordinal, committed_at, transition_id",
                limit,
                at.to_sqlx(),
                *id,
                o.cast_signed(),
                k.cast_signed(),
                BASELINE,
            )
            .fetch_all(&mut *conn)
            .await
        }
        (None, Some(h)) => {
            sqlx::query_as!(
                AuditRowRaw,
                "SELECT transition_id, transformation_name, arguments, actor,
                        invariant_epoch, invariants_checked,
                        asserted_claims, retracted_claims, emitted_intents, committed_at,
                attestation, parameters, model_hash, semantics_version, drawn_subjects,
                audit_order, act_ordinal, ordered_at, operations
                  FROM (SELECT transition_id, transformation_name, arguments, actor,
                               invariant_epoch, invariants_checked,
                               asserted_claims, retracted_claims, emitted_intents, committed_at,
                       attestation, parameters, model_hash, semantics_version, drawn_subjects,
                       audit_order, act_ordinal, ordered_at, operations,
                               sum((transformation_name = $3)::int) OVER w AS chunks
                        FROM morpholog.audit
                        WHERE audit_order < $2
                        WINDOW w AS (ORDER BY audit_order, act_ordinal, committed_at, transition_id
                                     ROWS UNBOUNDED PRECEDING)
                        ORDER BY audit_order, act_ordinal, committed_at, transition_id
                        LIMIT $1) s
                  WHERE chunks <= 1
                  ORDER BY audit_order, act_ordinal, committed_at, transition_id",
                limit,
                h.cast_signed(),
                BASELINE,
            )
            .fetch_all(&mut *conn)
            .await
        }
        (
            Some(AuditPosition {
                audit_order: o,
                act_ordinal: k,
                committed_at: at,
                transition_id: id,
            }),
            Some(h),
        ) => {
            sqlx::query_as!(
                AuditRowRaw,
                "SELECT transition_id, transformation_name, arguments, actor,
                        invariant_epoch, invariants_checked,
                        asserted_claims, retracted_claims, emitted_intents, committed_at,
                attestation, parameters, model_hash, semantics_version, drawn_subjects,
                audit_order, act_ordinal, ordered_at, operations
                  FROM (SELECT transition_id, transformation_name, arguments, actor,
                               invariant_epoch, invariants_checked,
                               asserted_claims, retracted_claims, emitted_intents, committed_at,
                       attestation, parameters, model_hash, semantics_version, drawn_subjects,
                       audit_order, act_ordinal, ordered_at, operations,
                               sum((transformation_name = $7)::int) OVER w AS chunks
                        FROM morpholog.audit
                        WHERE (audit_order, act_ordinal, committed_at, transition_id) > ($5, $6, $2, $3)
                          AND audit_order < $4
                        WINDOW w AS (ORDER BY audit_order, act_ordinal, committed_at, transition_id
                                     ROWS UNBOUNDED PRECEDING)
                        ORDER BY audit_order, act_ordinal, committed_at, transition_id
                        LIMIT $1) s
                  WHERE chunks <= 1
                  ORDER BY audit_order, act_ordinal, committed_at, transition_id",
                limit,
                at.to_sqlx(),
                *id,
                h.cast_signed(),
                o.cast_signed(),
                k.cast_signed(),
                BASELINE,
            )
            .fetch_all(&mut *conn)
            .await
        }
    }
    .map_err(classify)?;
    rows.into_iter().map(decode_audit_row).collect()
}

/// One keyset page of the rows from before the canonical order, in their
/// order `(committed_at, transition_id)`: strictly after `cursor`, strictly
/// below `below`. Below a start-time horizon every row is final, so each
/// statement's own snapshot is enough.
pub(crate) async fn list_legacy_rows_page<'e, E>(
    executor: E,
    cursor: Option<(Timestamp, Uuid)>,
    below: Timestamp,
    limit: i64,
) -> Result<Vec<AuditRow>, PgError>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let (at, id) = cursor.map_or((None, None), |(at, id)| (Some(at.to_sqlx()), Some(id)));
    let rows = sqlx::query_as!(
        AuditRowRaw,
        "SELECT transition_id, transformation_name, arguments, actor,
                invariant_epoch, invariants_checked,
                asserted_claims, retracted_claims, emitted_intents, committed_at,
                attestation, parameters, model_hash, semantics_version, drawn_subjects,
                audit_order, act_ordinal, ordered_at, operations
         FROM morpholog.audit
         WHERE audit_order = 0 AND act_ordinal = 0
           AND committed_at < $1
           AND (committed_at, transition_id) > (
               coalesce($2, '-infinity'::timestamptz),
               coalesce($3, '00000000-0000-0000-0000-000000000000'::uuid))
         ORDER BY committed_at, transition_id
         LIMIT $4",
        below.to_sqlx(),
        at,
        id,
        limit,
    )
    .fetch_all(executor)
    .await
    .map_err(classify)?;
    rows.into_iter().map(decode_audit_row).collect()
}

/// [`list_audit_rows_page`] restricted to the transitions that mention a
/// subject in their arguments, asserted or retracted claims, or emitted
/// intents: the tagged subject at any depth, a collection included, found
/// by a jsonpath whose subject is a bound variable, never spliced in. Its
/// own statements, so the unfiltered page keeps its plans; the bounds and
/// the order are the same, and the limit counts matching rows, so a short
/// page still means the bound was reached.
pub async fn list_audit_rows_page_mentioning(
    conn: &mut sqlx::PgConnection,
    cursor: Option<AuditPosition>,
    horizon: Option<u64>,
    limit: i64,
    subject: &str,
) -> Result<Vec<AuditRow>, PgError> {
    if subject.is_empty() {
        return Err(PgError::InvalidState(
            "--mentions needs a subject; an empty one names nothing".into(),
        ));
    }
    let vars = serde_json::json!({ "s": subject });
    let rows = match (&cursor, &horizon) {
        (None, None) => {
            sqlx::query_as!(
                AuditRowRaw,
                r#"SELECT transition_id, transformation_name, arguments, actor,
                        invariant_epoch, invariants_checked,
                        asserted_claims, retracted_claims, emitted_intents, committed_at,
                attestation, parameters, model_hash, semantics_version, drawn_subjects,
                audit_order, act_ordinal, ordered_at, operations
                 FROM morpholog.audit
                 WHERE jsonb_path_exists(
                     jsonb_build_array(arguments, asserted_claims, retracted_claims, emitted_intents),
                     '$.** ? (@.type == "subject" && @.value == $s)', $2)
                 ORDER BY audit_order, act_ordinal, committed_at, transition_id
                 LIMIT $1"#,
                limit,
                vars,
            )
            .fetch_all(&mut *conn)
            .await
        }
        (Some(AuditPosition {
            audit_order: o,
            act_ordinal: k,
            committed_at: at,
            transition_id: id,
        }), None) => {
            sqlx::query_as!(
                AuditRowRaw,
                r#"SELECT transition_id, transformation_name, arguments, actor,
                        invariant_epoch, invariants_checked,
                        asserted_claims, retracted_claims, emitted_intents, committed_at,
                attestation, parameters, model_hash, semantics_version, drawn_subjects,
                audit_order, act_ordinal, ordered_at, operations
                 FROM morpholog.audit
                 WHERE (audit_order, act_ordinal, committed_at, transition_id) > ($5, $6, $2, $3)
                   AND jsonb_path_exists(
                     jsonb_build_array(arguments, asserted_claims, retracted_claims, emitted_intents),
                     '$.** ? (@.type == "subject" && @.value == $s)', $4)
                 ORDER BY audit_order, act_ordinal, committed_at, transition_id
                 LIMIT $1"#,
                limit,
                at.to_sqlx(),
                *id,
                vars,
                o.cast_signed(),
                k.cast_signed(),
            )
            .fetch_all(&mut *conn)
            .await
        }
        (None, Some(h)) => {
            sqlx::query_as!(
                AuditRowRaw,
                r#"SELECT transition_id, transformation_name, arguments, actor,
                        invariant_epoch, invariants_checked,
                        asserted_claims, retracted_claims, emitted_intents, committed_at,
                attestation, parameters, model_hash, semantics_version, drawn_subjects,
                audit_order, act_ordinal, ordered_at, operations
                 FROM morpholog.audit
                 WHERE audit_order < $2
                   AND jsonb_path_exists(
                     jsonb_build_array(arguments, asserted_claims, retracted_claims, emitted_intents),
                     '$.** ? (@.type == "subject" && @.value == $s)', $3)
                 ORDER BY audit_order, act_ordinal, committed_at, transition_id
                 LIMIT $1"#,
                limit,
                h.cast_signed(),
                vars,
            )
            .fetch_all(&mut *conn)
            .await
        }
        (Some(AuditPosition {
            audit_order: o,
            act_ordinal: k,
            committed_at: at,
            transition_id: id,
        }), Some(h)) => {
            sqlx::query_as!(
                AuditRowRaw,
                r#"SELECT transition_id, transformation_name, arguments, actor,
                        invariant_epoch, invariants_checked,
                        asserted_claims, retracted_claims, emitted_intents, committed_at,
                attestation, parameters, model_hash, semantics_version, drawn_subjects,
                audit_order, act_ordinal, ordered_at, operations
                 FROM morpholog.audit
                 WHERE (audit_order, act_ordinal, committed_at, transition_id) > ($6, $7, $2, $3)
                   AND audit_order < $4
                   AND jsonb_path_exists(
                     jsonb_build_array(arguments, asserted_claims, retracted_claims, emitted_intents),
                     '$.** ? (@.type == "subject" && @.value == $s)', $5)
                 ORDER BY audit_order, act_ordinal, committed_at, transition_id
                 LIMIT $1"#,
                limit,
                at.to_sqlx(),
                *id,
                h.cast_signed(),
                vars,
                o.cast_signed(),
                k.cast_signed(),
            )
            .fetch_all(&mut *conn)
            .await
        }
    }
    .map_err(classify)?;
    rows.into_iter().map(decode_audit_row).collect()
}

/// A streaming audit tail with the lossless-resume order built in.
///
/// [`begin_audit_tail`] computes the horizon BEFORE the snapshot, opens
/// one `REPEATABLE READ READ ONLY` transaction, and resolves the cursor
/// inside it; [`AuditTail::next_page`] pages to the horizon. Rows from
/// writers in flight at the horizon are withheld for the next tail, never
/// skipped (see [`audit_resume_watermark`]).
pub struct AuditTail<'p> {
    tx: Transaction<'p, Postgres>,
    pages: AuditPages,
}
/// Open an audit tail, optionally resuming strictly after a seen
/// transition. An unknown id is [`PgError::TransitionNotFound`], never a
/// silent restart from zero, and one at or above the horizon is
/// [`PgError::TransitionNotSettled`].
pub async fn begin_audit_tail<'p>(
    pool: &'p PgPool,
    after: Option<Uuid>,
) -> Result<AuditTail<'p>, PgError> {
    // Horizon strictly before the snapshot - the ordering the
    // lossless-resume proof rests on.
    let horizon = audit_resume_watermark(pool).await?;
    let mut tx = begin_isolated_tx(pool, TxIsolation::RepeatableReadReadOnly).await?;
    let cursor = match after {
        Some(tid) => Some(settled(audit_cursor_for(&mut tx, tid).await?, horizon)?),
        None => None,
    };
    Ok(AuditTail {
        tx,
        pages: AuditPages::new(Some(horizon)).after(cursor),
    })
}
impl AuditTail<'_> {
    /// Only the transitions that mention this subject in their arguments,
    /// claims or intents. The cursor and the horizon are unchanged, so
    /// the filtered tail withholds and resumes exactly as the whole one.
    /// An empty subject is refused.
    pub fn mentioning(mut self, subject: &str) -> Result<Self, PgError> {
        if subject.is_empty() {
            return Err(PgError::InvalidState(
                "--mentions needs a subject; an empty one names nothing".into(),
            ));
        }
        self.pages = self.pages.mentioning(subject);
        Ok(self)
    }

    /// The next page of transitions in log order; empty once the tail reaches the horizon.
    pub async fn next_page(&mut self) -> Result<Vec<AuditRow>, PgError> {
        self.pages.next(&mut self.tx).await
    }

    /// [`AuditTail::next_page`], reading at most `max` transitions from the
    /// database, for a caller that holds its answer whole. The next page
    /// resumes after the last row returned.
    pub async fn next_page_up_to(&mut self, max: usize) -> Result<Vec<AuditRow>, PgError> {
        let max = i64::try_from(max).unwrap_or(i64::MAX);
        self.pages.next_up_to(&mut self.tx, max).await
    }
}
/// The last row in log order that this connection's snapshot sees, if
/// any.
pub(crate) async fn latest_audit_position(
    conn: &mut sqlx::PgConnection,
) -> Result<Option<AuditPosition>, PgError> {
    let row = sqlx::query!(
        "SELECT transition_id, committed_at, audit_order, act_ordinal FROM morpholog.audit
         ORDER BY audit_order DESC, act_ordinal DESC, committed_at DESC, transition_id DESC
         LIMIT 1",
    )
    .fetch_optional(&mut *conn)
    .await
    .map_err(classify_checked_query)?;
    Ok(row.map(|r| AuditPosition {
        audit_order: r.audit_order.cast_unsigned(),
        act_ordinal: r.act_ordinal.cast_unsigned(),
        committed_at: r.committed_at.into(),
        transition_id: r.transition_id,
    }))
}
/// Resolve a transition id to its [`AuditPosition`], the keyset cursor
/// every audit read orders by. An unknown id is
/// [`PgError::TransitionNotFound`], so a typo never restarts a tail.
pub async fn audit_cursor_for(
    conn: &mut sqlx::PgConnection,
    transition_id: Uuid,
) -> Result<AuditPosition, PgError> {
    let row = sqlx::query!(
        "SELECT committed_at, audit_order, act_ordinal FROM morpholog.audit
         WHERE transition_id = $1",
        transition_id,
    )
    .fetch_optional(&mut *conn)
    .await
    .map_err(classify_checked_query)?;
    match row {
        Some(row) => Ok(AuditPosition {
            audit_order: row.audit_order.cast_unsigned(),
            act_ordinal: row.act_ordinal.cast_unsigned(),
            committed_at: row.committed_at.into(),
            transition_id,
        }),
        None => Err(PgError::TransitionNotFound(transition_id)),
    }
}
/// A cursor below `horizon`, or [`PgError::TransitionNotSettled`]. Only
/// then is every row before it final, so that a read up to or after it
/// gives the same answer each time.
pub(crate) fn settled(cursor: AuditPosition, horizon: u64) -> Result<AuditPosition, PgError> {
    if cursor.audit_order < horizon {
        Ok(cursor)
    } else {
        Err(PgError::TransitionNotSettled(cursor.transition_id))
    }
}

/// The resume horizon for a lossless audit tail: every audit row whose
/// order is strictly below the returned one is final, committed or never
/// to commit, and so visible to a snapshot taken after this call returns.
///
/// A writer draws its order under the ordering lock and holds it, as a
/// transaction lock, until it commits or rolls back. The horizon is the
/// least order still held in this database, a prepared transaction's
/// included, or else the next order to be drawn, read under the same
/// lock, so no order below it can still be drawn or be in flight. Rows
/// from before the canonical order have order 0 and are always below it.
///
/// No session view is read, so the horizon needs no visibility into
/// other roles' sessions. A prepared transaction left unresolved holds
/// the horizon back until it is committed or rolled back.
pub async fn audit_resume_watermark<'e, E>(executor: E) -> Result<u64, PgError>
where
    E: sqlx::Executor<'e, Database = sqlx::Postgres>,
{
    let horizon = sqlx::query_scalar!(r#"SELECT morpholog.audit_watermark() AS "horizon!""#)
        .fetch_one(executor)
        .await
        .map_err(classify_checked_query)?;
    Ok(horizon.cast_unsigned())
}

/// `sha256:` and 64 lowercase hex digits: the canonical hash as
/// `morpholog hash` prints it.
fn is_model_hash(text: &str) -> bool {
    text.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
