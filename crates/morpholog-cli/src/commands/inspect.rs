//! `morpholog inspect` - read-only inspection of the durable substrate
//! (claims, audit rows, pending outbox intents, derived-claim
//! enumerations, declared predicate vocabulary).

use anyhow::{Context, anyhow};
use morpholog_postgres::{
    PgPool, list_audit_rows, list_claims, list_claims_at, list_claims_at_for_predicates,
    list_claims_for_predicates, list_derived, list_derived_at, list_outbox_rows,
    resolve_transition_at_or_before,
};
use uuid::Uuid;

use crate::commands::{connect, parse_or_exit, print_json, validate_or_exit};
use crate::{AsOf, Inspect};

/// Resolve an `--as-of` argument to a concrete transition id,
/// translating the timestamp form through the audit log. `None` stays
/// `None`: it means "current state", not a coordinate.
async fn resolve_as_of(pool: &PgPool, as_of: Option<AsOf>) -> anyhow::Result<Option<Uuid>> {
    Ok(match as_of {
        None => None,
        Some(AsOf::Transition(tid)) => Some(tid),
        Some(AsOf::AtOrBefore(at)) => Some(
            resolve_transition_at_or_before(pool, at)
                .await
                .context("resolving --as-of timestamp")?,
        ),
    })
}

/// Dispatch every `inspect` variant. Each variant either runs inline
/// (the simple list-claims/audit/outbox ones) or delegates to a
/// focused helper (derived, predicates) that needs more setup.
pub(crate) async fn run(what: Inspect) -> anyhow::Result<()> {
    match what {
        Inspect::Claims(args) => {
            let pool = connect(&args.database_url).await?;
            let as_of = resolve_as_of(&pool, args.as_of).await?;
            // Four paths, one rule: `--as-of` picks current-vs-replay,
            // `--predicate` picks full-vs-scoped. The scoped replay
            // filters during reconstruction, not after, so a targeted
            // historical read never materialises the full past state.
            let claims = match (as_of, args.predicate.as_slice()) {
                (Some(tid), []) => list_claims_at(&pool, tid)
                    .await
                    .context("list_claims_at failed")?,
                (Some(tid), preds) => list_claims_at_for_predicates(&pool, tid, preds)
                    .await
                    .context("list_claims_at_for_predicates failed")?,
                (None, []) => list_claims(&pool).await.context("list_claims failed")?,
                (None, preds) => list_claims_for_predicates(&pool, preds)
                    .await
                    .context("list_claims_for_predicates failed")?,
            };
            print_json(&claims)
        }
        Inspect::Audit(args) => {
            let pool = connect(&args.database_url).await?;
            let rows = list_audit_rows(&pool)
                .await
                .context("list_audit_rows failed")?;
            print_json(&rows)
        }
        Inspect::Outbox(args) => {
            let pool = connect(&args.database_url).await?;
            let rows =
                list_outbox_rows(&pool, args.status.db_filter(), args.intent_type.as_deref())
                    .await
                    .context("list_outbox_rows failed")?;
            print_json(&rows)
        }
        Inspect::Derived(args) => inspect_derived(args).await,
        Inspect::Predicates(args) => inspect_predicates(args),
        Inspect::Guarantees(args) => inspect_guarantees(args),
    }
}

/// Run the `inspect derived` subcommand end-to-end: look up the named
/// program and derived claim, connect, and enumerate the derived
/// extension against the current durable state (or against a past
/// state via `--as-of`).
///
/// Errors:
/// - Parse failure: rendered diagnostics, exits non-zero (the `check`/`run` path).
/// - Unknown derived claim: surfaces the list of derived predicates
///   declared on the parsed programme.
/// - Connection failure or kernel error: propagated via anyhow context.
async fn inspect_derived(args: crate::InspectDerivedArgs) -> anyhow::Result<()> {
    let (program, _source, _name) = parse_or_exit(&args.file)?;
    validate_or_exit(&program);

    let derived = program.derived_claim(&args.derived).ok_or_else(|| {
        let available = program
            .derived_claims
            .iter()
            .map(|d| d.predicate.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if available.is_empty() {
            anyhow!("`{}` declares no derived claims", args.file.display())
        } else {
            anyhow!(
                "derived claim `{}` not found in `{}`. Available: {}",
                args.derived,
                args.file.display(),
                available
            )
        }
    })?;

    let pool = connect(&args.database_url).await?;
    let rows = match resolve_as_of(&pool, args.as_of).await? {
        Some(tid) => list_derived_at(&pool, derived, tid)
            .await
            .context("list_derived_at failed")?,
        None => list_derived(&pool, derived)
            .await
            .context("list_derived failed")?,
    };
    print_json(&rows)
}

/// Run `inspect predicates <file.morph>`. Parses the source file, then
/// prints its declared predicates as JSON. Read-only and synchronous; no
/// database connection.
fn inspect_predicates(args: crate::InspectPredicatesArgs) -> anyhow::Result<()> {
    let (program, _source, _name) = parse_or_exit(&args.file)?;
    print_json(&program.predicates)
}

/// Show what a parsed programme makes impossible: its guarantees, one per
/// invariant. Static and read-only - no database. Prose by default;
/// `--json` emits the structured form.
fn inspect_guarantees(args: crate::InspectGuaranteesArgs) -> anyhow::Result<()> {
    let (program, _source, _name) = parse_or_exit(&args.file)?;
    validate_or_exit(&program);
    let guarantees = morpholog_core::guarantees(&program);
    if args.json {
        print_json(&guarantees)
    } else {
        println!(
            "{}",
            morpholog_core::render_guarantees(&program.name, &guarantees)
        );
        Ok(())
    }
}
