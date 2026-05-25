//! `morpholog inspect` - read-only inspection of the durable substrate
//! (claims, audit rows, pending outbox intents, derived-claim
//! enumerations, declared predicate vocabulary).

use anyhow::{Context, anyhow};
use morpholog_postgres::{
    list_audit_rows, list_claims, list_claims_at, list_derived, list_derived_at, list_outbox_rows,
};

use crate::Inspect;
use crate::commands::{connect, print_json};

/// Dispatch every `inspect` variant. Each variant either runs inline
/// (the simple list-claims/audit/outbox ones) or delegates to a
/// focused helper (derived, predicates) that needs more setup.
pub(crate) async fn run(what: Inspect) -> anyhow::Result<()> {
    match what {
        Inspect::Claims(args) => {
            let pool = connect(&args.database_url).await?;
            let claims = match args.as_of {
                Some(tid) => list_claims_at(&pool, tid)
                    .await
                    .context("list_claims_at failed")?,
                None => list_claims(&pool).await.context("list_claims failed")?,
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
/// - Unknown program: surfaces the list of available built-in programs
///   in the error message.
/// - Unknown derived claim: surfaces the list of derived predicates
///   declared on the matched program.
/// - Connection failure or kernel error: propagated via anyhow context.
async fn inspect_derived(args: crate::InspectDerivedArgs) -> anyhow::Result<()> {
    let programs = morpholog_examples::all_programs();
    let program = programs
        .iter()
        .find(|p| p.name == args.program)
        .ok_or_else(|| {
            let available = programs
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow!(
                "program `{}` not found. Available built-in programs: {}",
                args.program,
                available
            )
        })?;

    let derived = program.derived_claim(&args.derived).ok_or_else(|| {
        let available = program
            .derived_claims
            .iter()
            .map(|d| d.predicate.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if available.is_empty() {
            anyhow!("program `{}` declares no derived claims", args.program)
        } else {
            anyhow!(
                "derived claim `{}` not found in program `{}`. Available: {}",
                args.derived,
                args.program,
                available
            )
        }
    })?;

    let pool = connect(&args.database_url).await?;
    let rows = match args.as_of {
        Some(tid) => list_derived_at(&pool, derived, tid)
            .await
            .context("list_derived_at failed")?,
        None => list_derived(&pool, derived)
            .await
            .context("list_derived failed")?,
    };
    print_json(&rows)
}

/// Run `inspect predicates <program>`. Looks up the program by name
/// in the built-in registry, then prints its declared predicates as
/// JSON. Read-only and synchronous; no database connection.
fn inspect_predicates(args: crate::InspectPredicatesArgs) -> anyhow::Result<()> {
    let programs = morpholog_examples::all_programs();
    let program = programs
        .iter()
        .find(|p| p.name == args.program)
        .ok_or_else(|| {
            let available = programs
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow!(
                "program `{}` not found. Available built-in programs: {}",
                args.program,
                available
            )
        })?;
    print_json(&program.predicates)
}

/// Show what a built-in program makes impossible: its guarantees, one per
/// invariant. Static and read-only - no database. Prose by default;
/// `--json` emits the structured form.
fn inspect_guarantees(args: crate::InspectGuaranteesArgs) -> anyhow::Result<()> {
    let programs = morpholog_examples::all_programs();
    let program = programs
        .iter()
        .find(|p| p.name == args.program)
        .ok_or_else(|| {
            let available = programs
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow!(
                "program `{}` not found. Available built-in programs: {}",
                args.program,
                available
            )
        })?;
    let guarantees = morpholog_core::guarantees(program);
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
