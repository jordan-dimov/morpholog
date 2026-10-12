use crate::error::{PgError, classify_checked_query};
use crate::propose::{Reads, compute_load_scope};
use morpholog_core::{
    ClaimInstance, PredicateArgKind, PredicateName, PreparedProgram, State, TransformationName,
};
use sqlx::PgPool;
use uuid::Uuid;

/// A claim as a read reports it: the claim, and the transition whose
/// commit made it present. Admitting a claim already present leaves it
/// alone; a retraction ends it, and a later admission starts another.
///
/// At the head this is the `claims` table's own column, which no audit
/// leaf covers. Read as of a transition, it is taken from the audit
/// replay instead.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ClaimRead {
    #[serde(flatten)]
    pub claim: ClaimInstance,
    pub asserted_in: Uuid,
}

pub(crate) fn claims_of(reads: Vec<ClaimRead>) -> Vec<ClaimInstance> {
    reads.into_iter().map(|read| read.claim).collect()
}

/// Return every currently-admitted claim from `morpholog.claims`.
///
/// Ordered by `(asserted_at, predicate_name, arguments::text)`: admission
/// order with a stable tie-break, so the result is deterministic.
///
/// Reads the whole table; meant for tests, demos and small states.
pub async fn list_claims(pool: &PgPool) -> Result<Vec<ClaimInstance>, PgError> {
    read_claims(pool).await.map(claims_of)
}

/// [`list_claims`] with each claim's [`ClaimRead::asserted_in`].
pub async fn read_claims(pool: &PgPool) -> Result<Vec<ClaimRead>, PgError> {
    let rows = sqlx::query!(
        "SELECT predicate_name, arguments, asserted_in
         FROM morpholog.claims
         ORDER BY asserted_at, predicate_name, arguments::text",
    )
    .fetch_all(pool)
    .await
    .map_err(classify_checked_query)?;
    decode_claim_reads(
        rows.into_iter()
            .map(|r| (r.predicate_name, r.arguments, r.asserted_in))
            .collect(),
    )
}

/// Current claims of one predicate that share a shape: the kind at each
/// position, a quantity with its unit, and how many claims have it.
#[derive(Debug)]
pub struct StoredShape {
    pub predicate: PredicateName,
    pub kinds: Vec<PredicateArgKind>,
    pub claims: i64,
}

/// Every distinct shape among the current claims. The grouping runs in
/// SQL: one scan of the claims table, one row back per shape.
pub async fn stored_claim_shapes(pool: &PgPool) -> Result<Vec<StoredShape>, PgError> {
    // Non-null overrides: `predicate_name` is a NOT NULL column, the shape
    // is coalesced, and `count(*)` is never null.
    let rows = sqlx::query!(
        r#"SELECT predicate_name AS "predicate!", shape AS "shape!", count(*) AS "claims!"
           FROM (
               SELECT c.predicate_name,
                      coalesce(
                          (SELECT jsonb_agg(
                                      jsonb_build_array(a.e ->> 'type', a.e -> 'value' ->> 'unit')
                                      ORDER BY a.i)
                           FROM jsonb_array_elements(c.arguments) WITH ORDINALITY AS a(e, i)),
                          '[]'::jsonb) AS shape
               FROM morpholog.claims c
           ) s
           GROUP BY predicate_name, shape
           ORDER BY predicate_name, shape"#,
    )
    .fetch_all(pool)
    .await
    .map_err(classify_checked_query)?;
    rows.into_iter()
        .map(|r| {
            let positions: Vec<(Option<String>, Option<String>)> = serde_json::from_value(r.shape)?;
            let kinds = positions
                .into_iter()
                .map(|(tag, unit)| stored_kind(&r.predicate, tag.as_deref(), unit))
                .collect::<Result<_, _>>()?;
            Ok(StoredShape {
                predicate: PredicateName::from(r.predicate),
                kinds,
                claims: r.claims,
            })
        })
        .collect()
}

/// The declarable kind a stored value's tag stands for.
fn stored_kind(
    predicate: &str,
    tag: Option<&str>,
    unit: Option<String>,
) -> Result<PredicateArgKind, PgError> {
    Ok(match (tag, unit) {
        (Some("subject"), _) => PredicateArgKind::Subject,
        (Some("decimal"), _) => PredicateArgKind::Decimal,
        (Some("date"), _) => PredicateArgKind::Date,
        (Some("timestamp"), _) => PredicateArgKind::Timestamp,
        (Some("duration"), _) => PredicateArgKind::Duration,
        (Some("bool"), _) => PredicateArgKind::Bool,
        (Some("collection"), _) => PredicateArgKind::Collection,
        (Some("quantity"), Some(unit)) => PredicateArgKind::Quantity(unit.into()),
        (tag, _) => {
            return Err(PgError::InvalidState(format!(
                "a stored `{predicate}` claim holds a value tagged {tag:?}, which is no stored kind"
            )));
        }
    })
}

/// Decode `(predicate_name, arguments)` rows into `ClaimInstance`s.
pub(crate) fn decode_claim_rows(
    rows: Vec<(String, serde_json::Value)>,
) -> Result<Vec<ClaimInstance>, PgError> {
    rows.into_iter()
        .map(|(predicate, args_json)| {
            Ok(ClaimInstance {
                predicate: PredicateName::from(predicate),
                args: serde_json::from_value(args_json)?,
            })
        })
        .collect()
}

fn decode_claim_reads(
    rows: Vec<(String, serde_json::Value, Uuid)>,
) -> Result<Vec<ClaimRead>, PgError> {
    rows.into_iter()
        .map(|(predicate, args_json, asserted_in)| {
            Ok(ClaimRead {
                claim: ClaimInstance {
                    predicate: PredicateName::from(predicate),
                    args: serde_json::from_value(args_json)?,
                },
                asserted_in,
            })
        })
        .collect()
}

/// Return every currently-admitted claim whose `predicate_name` is in
/// `predicates`. Empty `predicates` returns `Ok(vec![])` without a query:
/// an empty footprint is meaningful, not an error.
///
/// Order matches [`list_claims`].
pub async fn list_claims_for_predicates(
    pool: &PgPool,
    predicates: &[String],
) -> Result<Vec<ClaimInstance>, PgError> {
    read_claims_for_predicates(pool, predicates)
        .await
        .map(claims_of)
}

/// [`list_claims_for_predicates`] with each claim's
/// [`ClaimRead::asserted_in`].
pub async fn read_claims_for_predicates(
    pool: &PgPool,
    predicates: &[String],
) -> Result<Vec<ClaimRead>, PgError> {
    if predicates.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query!(
        "SELECT predicate_name, arguments, asserted_in
         FROM morpholog.claims
         WHERE predicate_name = ANY($1)
         ORDER BY asserted_at, predicate_name, arguments::text",
        predicates,
    )
    .fetch_all(pool)
    .await
    .map_err(classify_checked_query)?;
    decode_claim_reads(
        rows.into_iter()
            .map(|r| (r.predicate_name, r.arguments, r.asserted_in))
            .collect(),
    )
}

/// One argument-level equality test: which position to compare, the
/// value to compare against, and whether the comparison is numeric.
///
/// One record per filter, not parallel slices, so the parts cannot arrive
/// at different lengths and silently drop conditions.
#[derive(Debug, Clone)]
pub struct ClaimFilter {
    pub position: i32,
    pub value: serde_json::Value,
    pub numeric: bool,
}

/// Claims of one predicate whose arguments match every filter, filtered in
/// the database.
///
/// This saves transfer, not scanning: rows that cannot match never cross
/// the wire. The index seeks to the predicate, but the argument test still
/// runs over every row of it, since no index covers argument positions.
///
/// The filters are passed as arrays, not built into `AND` clauses, so this
/// stays one static statement checked against the schema at build time.
///
/// A row whose arity disagrees with the declaration is returned whatever
/// the filters say. A filter on a missing position yields NULL and would
/// hide the row; returning it lets the decoder refuse the skew as a hard
/// error.
///
/// No filters means every row of the predicate: `bool_and` over zero rows
/// is NULL, so the `COALESCE` makes the empty conjunction true.
///
/// `numeric` marks filters that compare as numbers rather than as JSON.
/// Decimals are stored as strings, so `13.5` and `13.50` differ as JSON;
/// a JSON comparison would miss a row that exists.
///
/// Order matches [`list_claims`].
pub async fn list_claims_where(
    pool: &PgPool,
    predicate: &str,
    filters: &[ClaimFilter],
    declared_arity: i32,
) -> Result<Vec<ClaimInstance>, PgError> {
    read_claims_where(pool, predicate, filters, declared_arity)
        .await
        .map(claims_of)
}

/// [`list_claims_where`] with each claim's [`ClaimRead::asserted_in`].
pub async fn read_claims_where(
    pool: &PgPool,
    predicate: &str,
    filters: &[ClaimFilter],
    declared_arity: i32,
) -> Result<Vec<ClaimRead>, PgError> {
    // Built from whole filters so the arrays cannot differ in length:
    // `unnest` pads a short one with NULLs, which `bool_and` ignores,
    // silently widening the answer.
    let positions: Vec<i32> = filters.iter().map(|f| f.position).collect();
    let values: Vec<serde_json::Value> = filters.iter().map(|f| f.value.clone()).collect();
    let numeric: Vec<bool> = filters.iter().map(|f| f.numeric).collect();
    let rows = sqlx::query!(
        "SELECT predicate_name, arguments, asserted_in
         FROM morpholog.claims
         WHERE predicate_name = $1
           AND (jsonb_array_length(arguments) <> $5 OR COALESCE(
                (SELECT bool_and(
                   CASE WHEN f.numeric
                        THEN (arguments -> f.position ->> 'value')::numeric
                             = (f.value ->> 'value')::numeric
                        ELSE arguments -> f.position = f.value
                   END)
                 FROM unnest($2::int[], $3::jsonb[], $4::bool[])
                   AS f(position, value, numeric)),
                true))
         ORDER BY asserted_at, predicate_name, arguments::text",
        predicate,
        &positions,
        &values,
        &numeric,
        declared_arity,
    )
    .fetch_all(pool)
    .await
    .map_err(classify_checked_query)?;
    decode_claim_reads(
        rows.into_iter()
            .map(|r| (r.predicate_name, r.arguments, r.asserted_in))
            .collect(),
    )
}

/// Load the current pre-state a transformation would see: the read-only
/// counterpart of the load inside [`crate::propose_against_pg`]. Only the
/// predicates the body reads and the invariants reference are fetched;
/// nothing else can affect the verdict.
///
/// A plain pooled read, not a SERIALIZABLE transaction: `explain` shows
/// what would happen and commits nothing, so a point-in-time snapshot is
/// enough.
///
/// The transformation is `prepared`'s own, by name, so the scope always
/// comes from the same programme's rules. `Ok(None)` when no
/// transformation has that name.
pub async fn load_scoped_state(
    pool: &PgPool,
    prepared: &PreparedProgram,
    name: &TransformationName,
) -> Result<Option<State>, PgError> {
    let Some(transformation) = prepared.transformation(name) else {
        return Ok(None);
    };
    let program = prepared.program();
    // A diagnostic read: the explanation runs the interpreter, so the
    // invariants' predicates are loaded whatever the programme's plan.
    let scope: Vec<String> = compute_load_scope(
        transformation,
        None,
        &program.invariants,
        &program.definitions,
        Reads::BodyAndInvariants,
    )
    .predicates()
    .into_iter()
    .map(|p| p.to_string())
    .collect();
    let claims = list_claims_for_predicates(pool, &scope).await?;
    Ok(Some(State::from_claims(claims)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::stored_kind;
    use morpholog_core::{EvalValue, PredicateArgKind};

    /// Every value the codec can store reads back as the kind that
    /// declares it, through the same JSON the shape query reads.
    #[test]
    fn every_stored_value_reads_back_as_its_declared_kind() {
        let values = [
            EvalValue::Subject("s".into()),
            EvalValue::Decimal(rust_decimal::Decimal::ONE),
            EvalValue::Date(jiff::civil::date(2026, 10, 12)),
            EvalValue::Timestamp(jiff::Timestamp::UNIX_EPOCH),
            EvalValue::Duration(jiff::SignedDuration::from_hours(6)),
            EvalValue::Bool(true),
            EvalValue::Collection(vec![]),
            EvalValue::Quantity {
                amount: rust_decimal::Decimal::ONE,
                unit: "USD".into(),
            },
        ];
        for value in values {
            let expected = match &value {
                EvalValue::Subject(_) => PredicateArgKind::Subject,
                EvalValue::Decimal(_) => PredicateArgKind::Decimal,
                EvalValue::Date(_) => PredicateArgKind::Date,
                EvalValue::Timestamp(_) => PredicateArgKind::Timestamp,
                EvalValue::Duration(_) => PredicateArgKind::Duration,
                EvalValue::Bool(_) => PredicateArgKind::Bool,
                EvalValue::Collection(_) => PredicateArgKind::Collection,
                EvalValue::Quantity { unit, .. } => PredicateArgKind::Quantity(unit.clone()),
                EvalValue::CalendarSpan(_) => unreachable!("never stored"),
            };
            let json = serde_json::to_value(&value).unwrap();
            let tag = json["type"].as_str();
            let unit = json["value"]["unit"].as_str().map(str::to_string);
            assert_eq!(stored_kind("P", tag, unit).unwrap(), expected, "{json}");
        }
    }
}
