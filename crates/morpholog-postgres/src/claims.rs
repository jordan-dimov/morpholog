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
