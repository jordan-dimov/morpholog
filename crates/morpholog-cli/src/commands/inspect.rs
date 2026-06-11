//! `morpholog inspect` - read-only inspection of the durable substrate
//! (claims, audit rows, pending outbox intents, derived-claim
//! enumerations, declared predicate vocabulary).

use anyhow::{Context, anyhow, bail};
use morpholog_core::ClaimInstance;
use morpholog_postgres::{
    PgPool, list_audit_rows, list_claims, list_claims_at, list_claims_at_for_predicates,
    list_claims_for_predicates, list_derived, list_derived_at, list_outbox_rows,
    resolve_transition_at_or_before,
};
use std::path::Path;
use uuid::Uuid;

use crate::commands::args::eval_value_to_bare_json;

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
            // With `--named`, the programme is the authority - so it is
            // parsed and validated before any database work, and a
            // requested `--predicate` the file does not declare is a
            // hard error: the typo this mode exists to catch. (The bare
            // read keeps the opposite contract: claims table as
            // authority, an unknown predicate matches nothing.)
            let named_program = match &args.named {
                Some(file) => {
                    let parsed = parse_or_exit(file)?;
                    validate_or_exit(&parsed);
                    for requested in &args.predicate {
                        if !parsed
                            .program
                            .predicates
                            .iter()
                            .any(|d| d.name.as_str() == requested.as_str())
                        {
                            bail!(
                                "requested predicate `{requested}` is not declared in `{}`; \
                                 declared: {}",
                                file.display(),
                                declared_predicates(&parsed.program),
                            );
                        }
                    }
                    Some((parsed.program, file))
                }
                None => None,
            };
            let pool = connect(&args.db.database_url).await?;
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
            match named_program {
                Some((program, file)) => print_json(&decode_claims_named(&program, file, &claims)?),
                None => print_json(&claims),
            }
        }
        Inspect::Audit(args) => {
            let pool = connect(&args.database_url).await?;
            let rows = list_audit_rows(&pool)
                .await
                .context("list_audit_rows failed")?;
            print_json(&rows)
        }
        Inspect::Outbox(args) => {
            let pool = connect(&args.db.database_url).await?;
            let rows =
                list_outbox_rows(&pool, args.status.db_filter(), args.intent_type.as_deref())
                    .await
                    .context("list_outbox_rows failed")?;
            print_json(&rows)
        }
        Inspect::Derived(args) => inspect_derived(args).await,
        Inspect::Predicates(args) => inspect_predicates(args),
        Inspect::Guarantees(args) => inspect_guarantees(args),
        Inspect::Controls(args) => inspect_controls(args),
    }
}

/// The declared predicate names of a programme, comma-joined for the
/// hard-error messages that list what the file does declare.
fn declared_predicates(program: &morpholog_core::Program) -> String {
    program
        .predicates
        .iter()
        .map(|d| d.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Decode positional, tagged claims into bare named objects using the
/// declared vocabulary of the already-parsed programme - the
/// read-side mirror of `--args-named`. With `--named`, the programme
/// is the authority: an undeclared predicate or an arity mismatch is
/// programme/database skew and a hard error naming both sides, never
/// a silent skip. (The bare read keeps the opposite contract - claims
/// table as authority - which is why decoding requires the file.)
fn decode_claims_named(
    program: &morpholog_core::Program,
    file: &Path,
    claims: &[ClaimInstance],
) -> anyhow::Result<Vec<serde_json::Value>> {
    // The vocabulary is static for the whole invocation; index it once
    // rather than scanning the declarations per returned claim.
    let decls: std::collections::HashMap<&str, &morpholog_core::PredicateDecl> = program
        .predicates
        .iter()
        .map(|d| (d.name.as_str(), d))
        .collect();
    let mut rows = Vec::with_capacity(claims.len());
    for claim in claims {
        let Some(decl) = decls.get(claim.predicate.as_str()) else {
            bail!(
                "claim predicate `{}` is not declared in `{}` \
                 (programme/database skew); declared: {}",
                claim.predicate,
                file.display(),
                declared_predicates(program),
            );
        };
        if decl.args.len() != claim.args.len() {
            bail!(
                "claim `{}` has arity {} but `{}` declares arity {} \
                 (programme/database skew)",
                claim.predicate,
                claim.args.len(),
                file.display(),
                decl.args.len(),
            );
        }
        let fields: serde_json::Map<String, serde_json::Value> = decl
            .args
            .iter()
            .zip(claim.args.iter())
            .map(|(arg, value)| (arg.name.clone(), eval_value_to_bare_json(value)))
            .collect();
        rows.push(serde_json::json!({
            "predicate": claim.predicate,
            "args": fields,
        }));
    }
    Ok(rows)
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
    let parsed = parse_or_exit(&args.file)?;
    validate_or_exit(&parsed);
    let program = &parsed.program;

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

    let pool = connect(&args.db.database_url).await?;
    let rows = match resolve_as_of(&pool, args.as_of).await? {
        Some(tid) => list_derived_at(&pool, derived, &program.definitions, tid)
            .await
            .context("list_derived_at failed")?,
        None => list_derived(&pool, derived, &program.definitions)
            .await
            .context("list_derived failed")?,
    };
    print_json(&rows)
}

/// Run `inspect predicates <file.morph>`. Parses the source file, then
/// prints its declared predicates as JSON. Read-only and synchronous; no
/// database connection.
fn inspect_predicates(args: crate::InspectPredicatesArgs) -> anyhow::Result<()> {
    let parsed = parse_or_exit(&args.file)?;
    print_json(&parsed.program.predicates)
}

/// Show a parsed programme's control matrix: per-transformation
/// preconditions plus the invariant guarantees. Static and read-only -
/// no database. Prose by default; `--json` emits the structured form.
fn inspect_controls(args: crate::InspectGuaranteesArgs) -> anyhow::Result<()> {
    let parsed = parse_or_exit(&args.file)?;
    validate_or_exit(&parsed);
    let matrix = morpholog_core::controls(&parsed.program);
    if args.json {
        print_json(&matrix)
    } else {
        println!("{}", morpholog_core::render_controls(&matrix));
        Ok(())
    }
}

/// Show what a parsed programme makes impossible: its guarantees, one per
/// invariant. Static and read-only - no database. Prose by default;
/// `--json` emits the structured form.
fn inspect_guarantees(args: crate::InspectGuaranteesArgs) -> anyhow::Result<()> {
    let parsed = parse_or_exit(&args.file)?;
    validate_or_exit(&parsed);
    let program = &parsed.program;
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
