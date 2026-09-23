use crate::error::{PgError, classify_checked_query};
use crate::propose::{Reads, compute_load_scope};
use morpholog_core::{ClaimInstance, CompiledProgram, PredicateName, State, Transformation};
use sqlx::PgPool;
/// Return every currently-admitted claim from `morpholog.claims`.
///
/// Ordered by `(asserted_at, predicate_name, arguments::text)`: admission
/// order with a stable tie-break, so the result is deterministic.
///
/// Reads the whole table; meant for tests, demos and small states.
pub async fn list_claims(pool: &PgPool) -> Result<Vec<ClaimInstance>, PgError> {
    let rows = sqlx::query!(
        "SELECT predicate_name, arguments
         FROM morpholog.claims
         ORDER BY asserted_at, predicate_name, arguments::text",
    )
    .fetch_all(pool)
    .await
    .map_err(classify_checked_query)?;
    decode_claim_rows(
        rows.into_iter()
            .map(|r| (r.predicate_name, r.arguments))
            .collect(),
    )
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
/// Return every currently-admitted claim whose `predicate_name` is in
/// `predicates`. Empty `predicates` returns `Ok(vec![])` without a query:
/// an empty footprint is meaningful, not an error.
///
/// Order matches [`list_claims`].
pub async fn list_claims_for_predicates(
    pool: &PgPool,
    predicates: &[String],
) -> Result<Vec<ClaimInstance>, PgError> {
    if predicates.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query!(
        "SELECT predicate_name, arguments
         FROM morpholog.claims
         WHERE predicate_name = ANY($1)
         ORDER BY asserted_at, predicate_name, arguments::text",
        predicates,
    )
    .fetch_all(pool)
    .await
    .map_err(classify_checked_query)?;
    decode_claim_rows(
        rows.into_iter()
            .map(|r| (r.predicate_name, r.arguments))
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
    // Built from whole filters so the arrays cannot differ in length:
    // `unnest` pads a short one with NULLs, which `bool_and` ignores,
    // silently widening the answer.
    let positions: Vec<i32> = filters.iter().map(|f| f.position).collect();
    let values: Vec<serde_json::Value> = filters.iter().map(|f| f.value.clone()).collect();
    let numeric: Vec<bool> = filters.iter().map(|f| f.numeric).collect();
    let rows = sqlx::query!(
        "SELECT predicate_name, arguments
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
    decode_claim_rows(
        rows.into_iter()
            .map(|r| (r.predicate_name, r.arguments))
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
/// `transformation` must belong to `compiled`: the scope comes from its
/// body plus `compiled`'s invariants and definitions, so a foreign
/// transformation would load the wrong predicates.
pub async fn load_scoped_state(
    pool: &PgPool,
    compiled: &CompiledProgram,
    transformation: &Transformation,
) -> Result<State, PgError> {
    let program = compiled.program();
    // A diagnostic read: the explanation runs the interpreter, so the
    // invariants' predicates are loaded whatever the programme's plan.
    let scope: Vec<String> = compute_load_scope(
        transformation,
        &program.invariants,
        &program.definitions,
        Reads::BodyAndInvariants,
    )
    .into_iter()
    .map(|p| p.to_string())
    .collect();
    let claims = list_claims_for_predicates(pool, &scope).await?;
    Ok(State::from_claims(claims))
}
