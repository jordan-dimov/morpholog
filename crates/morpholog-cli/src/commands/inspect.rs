//! `morpholog inspect` - read-only inspection of the durable substrate
//! (claims, audit rows, pending outbox intents, derived-claim
//! enumerations, declared predicate vocabulary).

use anyhow::{Context, anyhow, bail};
use morpholog_core::ClaimInstance;
use morpholog_postgres::{
    PgPool, begin_audit_tail, list_claims, list_claims_at, list_claims_at_for_predicates,
    list_claims_for_predicates, list_claims_where, list_derived, list_derived_at, list_outbox_rows,
    list_rejection_rows, resolve_transition_at_or_before,
};
use std::path::Path;
use uuid::Uuid;

use crate::commands::args::eval_value_to_bare_json;
use crate::commands::filter::FieldFilter;
use morpholog_postgres::ClaimFilter;

use crate::commands::{
    compile_or_report, connect, emit, parse_or_report, print_json, validate_or_report,
};
use crate::{AsOf, Inspect};

/// Resolve an `--as-of` argument to a concrete transition id,
/// translating the timestamp form through the audit log. `None` stays
/// `None`: it means "current state", not a coordinate.
pub(crate) async fn resolve_as_of(
    pool: &PgPool,
    as_of: Option<AsOf>,
) -> anyhow::Result<Option<Uuid>> {
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

/// The claims read, shared by `inspect claims` and the session. `as_of`
/// picks current state or a replay; `predicates` picks all or some. A
/// filtered current read compares in the database. Replayed reads filter
/// afterwards, since there is no table to push the comparison into.
pub(crate) async fn claims_rows(
    pool: &PgPool,
    as_of: Option<Uuid>,
    predicates: &[String],
    filters: &[FieldFilter],
    declared_arity: i32,
) -> anyhow::Result<Vec<ClaimInstance>> {
    let claims = match (as_of, predicates) {
        (Some(tid), []) => list_claims_at(pool, tid)
            .await
            .context("list_claims_at failed")?,
        (Some(tid), preds) => list_claims_at_for_predicates(pool, tid, preds)
            .await
            .context("list_claims_at_for_predicates failed")?,
        (None, []) => list_claims(pool).await.context("list_claims failed")?,
        (None, [predicate]) if !filters.is_empty() => {
            // The arity comes from the declaration, so a row that
            // disagrees still comes back and the decoder refuses it. The
            // filter must not hide a mismatch the plain read would catch.
            list_claims_where(pool, predicate, &pg_filters(filters)?, declared_arity)
                .await
                .context("list_claims_where failed")?
        }
        (None, preds) => list_claims_for_predicates(pool, preds)
            .await
            .context("list_claims_for_predicates failed")?,
    };
    Ok(if filters.is_empty() || as_of.is_none() {
        claims
    } else {
        claims
            .into_iter()
            .filter(|c| crate::commands::filter::matches(&c.args, filters))
            .collect()
    })
}

/// The derived read, shared by `inspect derived` and the session. It
/// filters here rather than in SQL, because derived rows are computed,
/// not stored.
pub(crate) async fn derived_rows(
    pool: &PgPool,
    definitions: &[morpholog_core::Definition],
    derived: &morpholog_core::DerivedClaim,
    as_of: Option<Uuid>,
    filters: &[FieldFilter],
) -> anyhow::Result<Vec<ClaimInstance>> {
    let rows = match as_of {
        Some(tid) => list_derived_at(pool, derived, definitions, tid)
            .await
            .context("list_derived_at failed")?,
        None => list_derived(pool, derived, definitions)
            .await
            .context("list_derived failed")?,
    };
    Ok(if filters.is_empty() {
        rows
    } else {
        rows.into_iter()
            .filter(|r| crate::commands::filter::matches(&r.args, filters))
            .collect()
    })
}

/// Dispatch every `inspect` variant.
pub(crate) async fn run(what: Inspect) -> anyhow::Result<()> {
    match what {
        Inspect::Claims(args) => {
            // With `--named`, the programme is the authority: it is
            // validated before any database work, and a `--predicate` it
            // does not declare is an error. Without it, the claims table is
            // the authority and an unknown predicate just matches nothing.
            let named_program = match &args.named {
                Some(file) => {
                    let parsed = parse_or_report(file)?;
                    validate_or_report(&parsed)?;
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
            let (filters, declared_arity) = crate::commands::filter::resolve_where(
                named_program.as_ref().map(|(program, _)| program),
                &args.predicate,
                &args.filter,
            )?;

            let pool = connect(&args.db.database_url).await?;
            let as_of = resolve_as_of(&pool, args.as_of).await?;
            let claims = claims_rows(
                &pool,
                as_of,
                args.predicate.as_slice(),
                &filters,
                declared_arity,
            )
            .await?;
            match named_program {
                Some((program, file)) => print_json(&decode_claims_named(&program, file, &claims)?),
                None => print_json(&claims),
            }
        }
        Inspect::Audit(args) => inspect_audit(args).await,
        Inspect::Rejections(args) => {
            let pool = connect(&args.db.database_url).await?;
            let rows = list_rejection_rows(&pool, args.limit)
                .await
                .context("list_rejection_rows failed")?;
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
        Inspect::Coverage(args) => inspect_coverage(args).await,
    }
}

/// Run `inspect audit`: stream committed transitions as NDJSON, one per
/// line, in `(committed_at, transition_id)` order, for downstream
/// projectors.
///
/// The adapter's `begin_audit_tail` does the lossless-resume work. Rows
/// whose writers were still in flight are held for the next run, never
/// skipped. Memory stays constant: one page at a time.
async fn inspect_audit(args: crate::InspectAuditArgs) -> anyhow::Result<()> {
    // With `--named`, validate the programme before any database work.
    let named_program = match &args.named {
        Some(file) => {
            let parsed = parse_or_report(file)?;
            validate_or_report(&parsed)?;
            Some((parsed.program, file))
        }
        None => None,
    };
    let pool = connect(&args.db.database_url).await?;
    let mut tail = begin_audit_tail(&pool, args.after, args.writers.as_writers())
        .await
        .context("opening the audit tail")?;
    loop {
        let page = tail.next_page().await.context("reading an audit page")?;
        if page.is_empty() {
            break;
        }
        for row in page {
            let line = match &named_program {
                Some((program, file)) => audit_row_named_json(program, file, &row)?,
                None => serde_json::to_value(&row)?,
            };
            println!("{}", serde_json::to_string(&line)?);
        }
    }
    Ok(())
}

/// Serialize one audit row with its asserted and retracted claims decoded
/// by field name. Everything else, including `arguments` and
/// `emitted_intents`, stays tagged: those follow parameter kinds, not
/// predicate declarations.
fn audit_row_named_json(
    program: &morpholog_core::Program,
    file: &std::path::Path,
    row: &morpholog_postgres::AuditRow,
) -> anyhow::Result<serde_json::Value> {
    Ok(morpholog_cli::envelopes::audit_row_named(
        row,
        decode_claims_named(program, file, &row.asserted_claims)?,
        decode_claims_named(program, file, &row.retracted_claims)?,
    )?)
}

/// Run `inspect coverage <file.morph>`: replay the audit log and report
/// which rules ever fired. Read-only; exits zero for any valid programme,
/// since it answers a question rather than enforcing.
async fn inspect_coverage(args: crate::InspectCoverageArgs) -> anyhow::Result<()> {
    let parsed = parse_or_report(&args.file)?;
    validate_or_report(&parsed)?;
    let pool = connect(&args.db.database_url).await?;
    let report = morpholog_postgres::coverage_replay(&pool, &parsed.program)
        .await
        .context("coverage_replay failed")?;
    emit(args.json, &report, || {
        morpholog_core::render_coverage(&report)
    })
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

/// Decode tagged claims into bare named objects using the programme's
/// declarations, mirroring `--args-named`. An undeclared predicate or an
/// arity mismatch means the programme and database disagree: an error
/// naming both sides, never a silent skip.
pub(crate) fn decode_claims_named(
    program: &morpholog_core::Program,
    file: &Path,
    claims: &[ClaimInstance],
) -> anyhow::Result<Vec<morpholog_cli::envelopes::NamedClaim>> {
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
        rows.push(morpholog_cli::envelopes::NamedClaim {
            args: fields,
            predicate: claim.predicate.clone(),
        });
    }
    Ok(rows)
}

/// Run `inspect derived`: find the derived claim in the programme and
/// enumerate it against current state, or a past state with `--as-of`.
///
/// Errors:
/// - Parse failure: rendered diagnostics, exits non-zero.
/// - Unknown derived claim: lists the derived predicates the programme
///   declares.
/// - Connection failure or kernel error: propagated with context.
async fn inspect_derived(args: crate::InspectDerivedArgs) -> anyhow::Result<()> {
    let parsed = parse_or_report(&args.file)?;
    validate_or_report(&parsed)?;
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

    // A derived predicate is declared like any other, so `--where`
    // resolves fields as `inspect claims` does.
    let (filters, _) = crate::commands::filter::resolve_where(
        Some(program),
        std::slice::from_ref(&args.derived),
        &args.filter,
    )?;
    let pool = connect(&args.db.database_url).await?;
    let as_of = resolve_as_of(&pool, args.as_of).await?;
    let rows = derived_rows(&pool, &program.definitions, derived, as_of, &filters).await?;
    if args.named {
        print_json(&decode_claims_named(program, &args.file, &rows)?)
    } else {
        print_json(&rows)
    }
}

/// Run `inspect predicates <file.morph>`: print the declared predicates as
/// JSON. No database.
fn inspect_predicates(args: crate::InspectPredicatesArgs) -> anyhow::Result<()> {
    let parsed = parse_or_report(&args.file)?;
    print_json(&parsed.program.predicates)
}

/// Show the control matrix: each transformation's preconditions plus the
/// invariant guarantees. No database. Prose, or JSON with `--json`.
fn inspect_controls(args: crate::InspectGuaranteesArgs) -> anyhow::Result<()> {
    let parsed = parse_or_report(&args.file)?;
    let compiled = compile_or_report(&parsed)?;
    let matrix = morpholog_core::controls(&compiled);
    emit(args.json, &matrix, || {
        morpholog_core::render_controls(&matrix)
    })
}

/// Show what a programme makes impossible: one guarantee per invariant.
/// No database. Prose, or JSON with `--json`.
fn inspect_guarantees(args: crate::InspectGuaranteesArgs) -> anyhow::Result<()> {
    let parsed = parse_or_report(&args.file)?;
    let compiled = compile_or_report(&parsed)?;
    let guarantees = morpholog_core::guarantees(&compiled);
    emit(args.json, &guarantees, || {
        morpholog_core::render_guarantees(&compiled.program().name, &guarantees)
    })
}

/// Translate resolved filters into the adapter's shape.
fn pg_filters(filters: &[FieldFilter]) -> anyhow::Result<Vec<ClaimFilter>> {
    filters
        .iter()
        .map(|f| {
            Ok(ClaimFilter {
                // A position that does not fit cannot come from a parsed
                // declaration: a bug, not a filter that matches nothing.
                position: i32::try_from(f.position)
                    .with_context(|| format!("argument position {} does not fit", f.position))?,
                value: serde_json::to_value(&f.value).context("encoding a --where value")?,
                numeric: f.is_numeric(),
            })
        })
        .collect()
}
