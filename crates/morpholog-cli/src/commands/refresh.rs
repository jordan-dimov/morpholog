//! `morpholog refresh derived` - recompute every derived claim with the
//! kernel and publish a new generation of the `morpholog_read` read
//! model that derived SQL views read.
//!
//! Never part of `propose`: how fresh the read model is is an operational
//! matter, not part of the rules. The stored rows are exactly what
//! `enumerate_derived` produced; SQL never recomputes them. Stdout carries
//! the typed report, stderr a human summary with timings.

use morpholog_cli::envelopes::RefreshDerivedReport;
use morpholog_postgres::refresh_derived;

use crate::RefreshDerivedArgs;
use crate::commands::{
    connect, hash::canonical_hash, parse_or_report, print_json, validate_or_report,
};

pub(crate) async fn run(args: &RefreshDerivedArgs) -> anyhow::Result<()> {
    let parsed = parse_or_report(&args.file)?;
    // Validate before touching the database, so the read model is only
    // built for a sound programme.
    let validated = validate_or_report(&parsed)?;
    let model_hash = canonical_hash(&parsed.program);

    let pool = connect(&args.db.database_url).await?;
    let summary = refresh_derived(&pool, validated, &model_hash).await?;

    let snapshot = summary.source_snapshot_transition_id.map_or_else(
        || "(no committed transitions)".to_string(),
        |t| t.to_string(),
    );
    print_json(&RefreshDerivedReport::from(&summary))?;
    eprintln!(
        "refreshed {} derived claim(s) from {} derived predicate(s)\n  \
         source claims loaded: {}\n  \
         model: {}\n  \
         snapshot through (latest visible transition): {}\n  \
         generation: {}\n  \
         timings: {:?} read / {:?} compute / {:?} write",
        summary.derived_claim_count,
        summary.derived_predicate_count,
        summary.source_claim_count,
        summary.model_hash,
        snapshot,
        summary.refresh_id,
        summary.read,
        summary.compute,
        summary.write,
    );
    Ok(())
}
