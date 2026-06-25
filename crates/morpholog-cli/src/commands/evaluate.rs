//! `morpholog evaluate` - score a candidate programme against committed
//! history. The evaluator pointed backward: replay the audit log under a
//! candidate's invariants (which are NOT deployed) and report which
//! already-admitted commits each would have refused. Output is JSON - the
//! fitness contract a discovery loop consumes.

use anyhow::Context;
use morpholog_core::invariants_using_pre;
use morpholog_postgres::score_candidate;

use crate::EvaluateArgs;
use crate::commands::{connect, parse_or_exit, print_json, validate_or_exit};

pub(crate) async fn run(args: EvaluateArgs) -> anyhow::Result<()> {
    let parsed = parse_or_exit(&args.file)?;
    validate_or_exit(&parsed);

    // Fail fast, before any database connection: v1 scores state
    // invariants only, so a transition-relational candidate is rejected
    // here rather than after a round-trip.
    let pre = invariants_using_pre(&parsed.program);
    if !pre.is_empty() {
        eprintln!(
            "error: `evaluate` v1 scores state invariants only; \
             these use pre(...) (transition-relational, deferred): {}",
            pre.join(", ")
        );
        std::process::exit(1);
    }

    let pool = connect(&args.db.database_url).await?;
    let report = score_candidate(&pool, &parsed.program)
        .await
        .context("score_candidate failed")?;
    print_json(&report)
}
