//! `morpholog refresh derived` - recompute every derived claim with the
//! kernel and publish a new generation of the `morpholog_read` read
//! model that derived SQL views read.
//!
//! Out-of-band by design: never part of `propose`, so read-model
//! freshness is operational, not semantic. The exact `enumerate_derived`
//! output is stored - SQL never recomputes a derived value. The summary
//! prints what the refresh cost so an operator sees it.

use morpholog_postgres::refresh_derived;

use crate::RefreshDerivedArgs;
use crate::commands::{connect, hash::canonical_hash, parse_or_exit, validate_or_exit};

pub(crate) async fn run(args: &RefreshDerivedArgs) -> anyhow::Result<()> {
    let parsed = parse_or_exit(&args.file)?;
    // Validate before touching the database - the same vocabulary gate
    // `schema`, `hash`, and `generate views` apply.
    validate_or_exit(&parsed);
    let model_hash = canonical_hash(&parsed.program);

    let pool = connect(&args.db.database_url).await?;
    let summary = refresh_derived(&pool, &parsed.program, &model_hash).await?;

    let high_water = summary.source_highwater_transition_id.map_or_else(
        || "(no committed transitions)".to_string(),
        |t| t.to_string(),
    );
    println!(
        "refreshed {} derived claim(s) from {} derived predicate(s)\n  \
         source claims loaded: {}\n  \
         model: {}\n  \
         high-water: {}\n  \
         generation: {}\n  \
         timings: {:?} read / {:?} compute / {:?} write",
        summary.derived_claim_count,
        summary.derived_predicate_count,
        summary.source_claim_count,
        summary.model_hash,
        high_water,
        summary.refresh_id,
        summary.read,
        summary.compute,
        summary.write,
    );
    Ok(())
}
