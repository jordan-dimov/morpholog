use crate::error::{PgError, classify};
use crate::propose::compute_load_scope;
use morpholog_core::{ClaimInstance, CompiledProgram, PredicateName, State, Transformation};
use sqlx::PgPool;
/// Return every currently-admitted claim from `morpholog.claims`.
///
/// Order is `(asserted_at, predicate_name, arguments::text)`: causal
/// admission order with predicate-then-args as the stable tie-break,
/// so the result is deterministic across runs.
///
/// A `SELECT *` over the entire table, intended for tests, demos, and
/// small-state inspection; large states should query SQL directly.
pub async fn list_claims(pool: &PgPool) -> Result<Vec<ClaimInstance>, PgError> {
    let rows = sqlx::query!(
        "SELECT predicate_name, arguments
         FROM morpholog.claims
         ORDER BY asserted_at, predicate_name, arguments::text",
    )
    .fetch_all(pool)
    .await
    .map_err(classify)?;
    decode_claim_rows(
        rows.into_iter()
            .map(|r| (r.predicate_name, r.arguments))
            .collect(),
    )
}
/// Decode `(predicate_name, arguments)` rows into `ClaimInstance`s -
/// the shared tail of the current-claims listings.
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
/// `predicates`. Empty `predicates` short-circuits to `Ok(vec![])`
/// without a query: an empty footprint is meaningful (e.g. a derived
/// claim whose domain is a no-op), not an error.
///
/// Used by [`crate::list_derived`] to load only the claims a derived claim's
/// footprint references, avoiding the rest of `morpholog.claims`.
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
    .map_err(classify)?;
    decode_claim_rows(
        rows.into_iter()
            .map(|r| (r.predicate_name, r.arguments))
            .collect(),
    )
}
/// One argument-level equality test: which position to compare, the
/// value to compare against, and whether the comparison is numeric.
///
/// One record per filter rather than parallel slices, so the three parts
/// cannot be handed over at different lengths - a shape that would have
/// dropped conditions silently rather than failing.
#[derive(Debug, Clone)]
pub struct ClaimFilter {
    pub position: i32,
    pub value: serde_json::Value,
    pub numeric: bool,
}

/// Claims of one predicate whose arguments match every filter -
/// argument-level selection, evaluated in the database.
///
/// **What this saves is transfer, not scanning.** The comparison runs
/// here rather than in the caller, so rows that cannot match never cross
/// the wire, are never decoded, and never occupy client memory. The index
/// on `(predicate_name, arguments)` seeks to the predicate; the argument
/// comparison is then a filter over every row of it, so database effort
/// still scales with the predicate rather than with the answer. Making
/// lookup cost follow the answer needs an index over argument positions,
/// which does not exist.
///
/// The filters arrive as parallel arrays rather than as generated `AND`
/// clauses so this stays one static statement, checked against the schema
/// at build time like every other query in this crate. A dynamically
/// assembled `WHERE` would have needed an `AssertSqlSafe` escape and lost
/// that.
///
/// A row whose arity disagrees with the declaration comes back whatever
/// the filters say. The named read promises that programme/database skew
/// is a hard error, and a filter on a position a short row does not have
/// yields NULL - so the row would be quietly excluded and the skew the
/// unfiltered read refuses would pass unnoticed. Returning it lets the
/// decoder refuse it, as it would have anyway.
///
/// No filters means every row of the predicate, not none: `bool_and`
/// over zero rows is NULL, and `AND NULL` would quietly return an empty
/// set for a caller who asked for everything. The `COALESCE` makes the
/// empty conjunction true, which is what an empty conjunction means.
///
/// `numeric` marks the filters whose values must compare as numbers
/// rather than as JSON. Decimals are stored as strings to keep them
/// exact, so `13.5` and `13.50` are equal numbers and different strings -
/// comparing the JSON would answer "no such row" for a row that exists,
/// which is the worst thing a filter can do.
///
/// Order matches [`list_claims`].
pub async fn list_claims_where(
    pool: &PgPool,
    predicate: &str,
    filters: &[ClaimFilter],
    declared_arity: i32,
) -> Result<Vec<ClaimInstance>, PgError> {
    // Built here, from whole filters, so the three arrays cannot
    // disagree in length. `unnest` pads the short one with NULLs and
    // `bool_and` ignores nulls, so a mismatched call would have dropped
    // part of the conjunction and returned a plausible over-broad answer
    // - worse than an error, because nothing looks wrong.
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
    .map_err(classify)?;
    decode_claim_rows(
        rows.into_iter()
            .map(|r| (r.predicate_name, r.arguments))
            .collect(),
    )
}

/// Load the current scoped pre-state a transformation would see, the
/// read-only counterpart of the load inside [`crate::propose_against_pg`].
/// Scopes to exactly the predicates the transformation body reads and
/// the invariants reference (see `compute_load_scope`); claims outside
/// that scope cannot affect the verdict, so they are not fetched.
///
/// Unlike `propose_against_pg`, this issues a plain pooled read, not a
/// SERIALIZABLE transaction: the caller is explaining what *would*
/// happen, not committing a decision, so the right semantics is a
/// point-in-time snapshot, not a serialization point.
///
/// Used by `morpholog explain` to run the kernel in-memory against live
/// state without opening a write transaction.
///
/// `transformation` is passed explicitly (rather than resolved by name,
/// as the `propose_against_pg*` facade does) because `explain` has not
/// built a transition at this point. It must belong to `compiled`: the
/// scope is computed from its body together with `compiled`'s invariants
/// and definitions, so an unrelated transformation would scope the read
/// to the wrong predicates.
pub async fn load_scoped_state(
    pool: &PgPool,
    compiled: &CompiledProgram,
    transformation: &Transformation,
) -> Result<State, PgError> {
    let program = compiled.program();
    let scope: Vec<String> =
        compute_load_scope(transformation, &program.invariants, &program.definitions)
            .into_iter()
            .map(|p| p.to_string())
            .collect();
    let claims = list_claims_for_predicates(pool, &scope).await?;
    Ok(State::from_claims(claims))
}
