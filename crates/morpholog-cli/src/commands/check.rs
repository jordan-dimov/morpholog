//! `morpholog check` - parse + validate + lint a `.morph` source file.

use crate::CheckArgs;
use crate::commands::{AlreadyReported, colour, print_json};
use anyhow::Context;
use morpholog_cli::envelopes::{CheckDiagnostic, CheckRefusal, CheckReport, CheckedInvariant};
use morpholog_core::{PreparedProgram, Program};
use morpholog_postgres::{CompileReason, InvariantPlan, PgProgram};
use morpholog_surface::{Diagnostic, Span, parse_program_with_sources};
use std::path::Path;

/// Run the `check` subcommand: collect every finding once, then render
/// them the way the caller asked.
///
/// - Parse failure: parse diagnostics, exit 1.
/// - Validation failure: each error caret-located when the source map
///   places it (its declaration, or the exact statement), a plain
///   `error: <message>` line when it has no source anchor. Exit 1.
/// - Both clean: declaration policy, then lints, then `--against`
///   collisions. A lint is a hint and the check still passes; under
///   `--strict` it is an error and the check fails.
/// - Fully clean: print nothing and exit 0, or a short summary under
///   `--verbose`. Findings always go to stderr, so stdout stays silent
///   for scripts. `--json` prints every finding to stdout as one object,
///   with byte offsets and line/column; exit codes are the same.
pub(crate) fn run(args: CheckArgs) -> anyhow::Result<()> {
    let collected = collect(&args)?;
    let routes = match &collected.prepared {
        Some(prepared) if args.json || args.verbose => {
            Some(invariant_routes(&PgProgram::new(prepared.clone()))?)
        }
        _ => None,
    };
    let verbose_summary = match (&collected.prepared, &routes) {
        (Some(prepared), Some(routes)) if args.verbose => {
            Some(summary(prepared.program(), routes, &args.file))
        }
        _ => None,
    };
    if args.json {
        let payload = CheckReport {
            diagnostics: collected
                .findings
                .iter()
                .map(|f| {
                    CheckDiagnostic::new(
                        f.severity,
                        f.message.clone(),
                        f.diagnostic
                            .as_ref()
                            .filter(|_| f.foreign.is_none())
                            .map(|d| d.primary.clone()),
                        &collected.source,
                    )
                })
                .collect(),
            file: args.file.display().to_string(),
            invariants: routes,
        };
        print_json(&payload)?;
    } else {
        for f in &collected.findings {
            match (&f.diagnostic, &f.foreign) {
                (Some(d), Some((name, source))) => eprint!("{}", d.render(name, source, colour())),
                (Some(d), None) => {
                    eprint!(
                        "{}",
                        d.render(&collected.source_name, &collected.source, colour())
                    );
                }
                (None, _) => eprintln!("{}: {}", f.severity, f.message),
            }
        }
    }
    if collected.failed {
        return Err(AlreadyReported.into());
    }
    if let Some(text) = verbose_summary {
        print!("{text}");
    }
    if let (Some(prepared), true) = (&collected.prepared, args.ir) {
        return print_ir(prepared.program());
    }
    Ok(())
}

/// One finding, with its caret-located diagnostic when a source map places
/// it. A finding about another file (a broken `--against` programme)
/// carries that file's source for its carets. The JSON report has one
/// `file`, so there such a finding has no location rather than a wrong one.
struct Finding {
    severity: &'static str,
    message: String,
    diagnostic: Option<Diagnostic>,
    /// `(source_name, source)` of the file the diagnostic points into,
    /// when that is not the file being checked.
    foreign: Option<(String, String)>,
}

impl Finding {
    fn error(message: String, span: Option<Span>) -> Self {
        Self {
            severity: "error",
            message: message.clone(),
            diagnostic: span.map(|s| Diagnostic::error(message, s)),
            foreign: None,
        }
    }
    fn foreign_error(
        message: String,
        diagnostic: Option<Diagnostic>,
        path: &Path,
        source: &str,
    ) -> Self {
        Self {
            severity: "error",
            message,
            foreign: diagnostic
                .as_ref()
                .map(|_| (path.display().to_string(), source.to_string())),
            diagnostic,
        }
    }
    fn lint(message: String, span: Option<Span>, strict: bool) -> Self {
        let (severity, build): (&'static str, fn(String, Span) -> Diagnostic) = if strict {
            ("error", Diagnostic::error)
        } else {
            ("hint", Diagnostic::hint)
        };
        Self {
            severity,
            message: message.clone(),
            diagnostic: span.map(|s| build(message, s)),
            foreign: None,
        }
    }
}

struct Collected {
    findings: Vec<Finding>,
    failed: bool,
    source: String,
    source_name: String,
    /// The prepared programme, for the invariant routes, `--verbose` and
    /// `--ir`; absent when parsing or validation failed.
    prepared: Option<PreparedProgram>,
}

/// Every finding for the file, in the order the layers run.
fn collect(args: &CheckArgs) -> anyhow::Result<Collected> {
    let source = std::fs::read_to_string(&args.file)
        .with_context(|| format!("read source file {}", args.file.display()))?;
    let mut out = Collected {
        findings: Vec::new(),
        failed: false,
        source,
        source_name: args.file.display().to_string(),
        prepared: None,
    };
    let (program, map) = match parse_program_with_sources(&out.source) {
        Ok(parsed) => parsed,
        Err(diagnostics) => {
            out.failed = true;
            out.findings
                .extend(diagnostics.into_iter().map(|d| Finding {
                    severity: "error",
                    message: d.message.clone(),
                    diagnostic: Some(d),
                    foreign: None,
                }));
            return Ok(out);
        }
    };
    // Building the `PreparedProgram` validates: `Err` holds the same
    // errors `program.validate()` would.
    let prepared = match PreparedProgram::new(program) {
        Ok(prepared) => prepared,
        Err(errors) => {
            out.failed = true;
            out.findings.extend(
                errors
                    .iter()
                    .map(|e| Finding::error(e.to_string(), map.span_for_error(e))),
            );
            return Ok(out);
        }
    };
    for finding in &morpholog_postgres::validate_declarations(prepared.program()) {
        out.failed = true;
        out.findings.push(Finding::error(finding.to_string(), None));
    }
    for lint in &morpholog_core::lints(&prepared) {
        out.failed |= args.strict;
        out.findings.push(Finding::lint(
            lint.to_string(),
            map.span_for_lint(lint),
            args.strict,
        ));
    }
    for path in &args.against {
        if let Err(e) = refuse_self_comparison(&args.file, path) {
            out.failed = true;
            out.findings.push(Finding::error(e.to_string(), None));
            continue;
        }
        match load_against(path) {
            Err(findings) => {
                out.failed = true;
                out.findings.extend(findings);
            }
            Ok(other) => {
                for lint in &morpholog_core::shared_writer_lints(prepared.program(), &other) {
                    out.failed |= args.strict;
                    out.findings.push(Finding::lint(
                        format!("against {}: {lint}", path.display()),
                        map.span_for_lint(lint),
                        args.strict,
                    ));
                }
            }
        }
    }
    out.prepared = Some(prepared);
    Ok(out)
}

/// Refuse `--against` the file being checked: every write would collide
/// with itself.
fn refuse_self_comparison(file: &Path, against: &Path) -> anyhow::Result<()> {
    let same = match (file.canonicalize(), against.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => file == against,
    };
    if same {
        anyhow::bail!(
            "--against {} is the file being checked; a programme cannot collide with itself",
            against.display()
        );
    }
    Ok(())
}

/// The programme behind an `--against` path, held to the same parse,
/// validation and declaration-policy floor as the checked file (not its
/// lints). On failure, the findings name the path and carry its source.
fn load_against(path: &Path) -> Result<Program, Vec<Finding>> {
    let against = path.display();
    let source = std::fs::read_to_string(path).map_err(|e| {
        vec![Finding::error(
            format!("against {against}: read source file: {e}"),
            None,
        )]
    })?;
    let (program, map) = parse_program_with_sources(&source).map_err(|diagnostics| {
        diagnostics
            .into_iter()
            .map(|d| {
                Finding::foreign_error(
                    format!("against {against}: {}", d.message),
                    Some(d),
                    path,
                    &source,
                )
            })
            .collect::<Vec<_>>()
    })?;
    let prepared = PreparedProgram::new(program).map_err(|errors| {
        errors
            .iter()
            .map(|e| {
                let message = format!("against {against}: {e}");
                let diagnostic = map
                    .span_for_error(e)
                    .map(|span| Diagnostic::error(message.clone(), span));
                Finding::foreign_error(message, diagnostic, path, &source)
            })
            .collect::<Vec<_>>()
    })?;
    let policy = morpholog_postgres::validate_declarations(prepared.program());
    if !policy.is_empty() {
        return Err(policy
            .iter()
            .map(|f| Finding::error(format!("against {against}: {f}"), None))
            .collect());
    }
    Ok(prepared.program().clone())
}

/// `check --ir`: print the validated programme's internal representation
/// as pretty JSON, for debugging.
///
/// `Program` does not derive `Serialize`, so this is a projection:
/// declarations are structural, and rule bodies render through the
/// canonical formatter.
fn print_ir(program: &Program) -> anyhow::Result<()> {
    let invariants_payload: Vec<serde_json::Value> = program
        .invariants
        .iter()
        .map(|inv| {
            serde_json::json!({
                "name": &inv.name,
                "version": inv.version,
                "body": morpholog_core::format::format_prop_source(program, &inv.body),
            })
        })
        .collect();

    let definitions_payload: Vec<serde_json::Value> = program
        .definitions
        .iter()
        .map(|d| {
            serde_json::json!({
                "name": &d.name,
                "parameters": &d.parameters,
                "body": morpholog_core::format::format_prop_source(program, &d.body),
            })
        })
        .collect();

    let transformations_payload: Vec<serde_json::Value> = program
        .transformations
        .iter()
        .map(|t| {
            let body_lines: Vec<String> = t
                .body
                .iter()
                .map(|s| morpholog_core::format::format_stmt_source(program, s, 0))
                .collect();
            serde_json::json!({
                "name": &t.name,
                "parameters": &t.parameters,
                "body": body_lines,
            })
        })
        .collect();

    let derived_payload: Vec<serde_json::Value> = program
        .derived_claims
        .iter()
        .map(|d| {
            let values: Vec<serde_json::Value> = d
                .values
                .iter()
                .map(|v| {
                    serde_json::json!({
                        "name": &v.name,
                        "expr": morpholog_core::format::format_value_source(program, &v.expr),
                    })
                })
                .collect();
            serde_json::json!({
                "predicate": &d.predicate,
                "keys": &d.keys,
                "over": morpholog_core::format::format_prop_source(program, &d.domain),
                "values": values,
            })
        })
        .collect();

    let payload = serde_json::json!({
        "name": program.name,
        "predicates": program.predicates,
        "definitions": definitions_payload,
        "invariants": invariants_payload,
        "transformations": transformations_payload,
        "derived_claims": derived_payload,
    });
    print_json(&payload)
}

/// Each invariant's route as the production plan (`PgProgram::new`)
/// decides it, in programme order: no refusal means SQL checks it, a
/// refusal keeps it with the interpreter. Not a rendering of every
/// `InvariantPlan`: a plan forced to the interpreter has no refusals to
/// report, and `check` never builds one.
fn invariant_routes(pg: &PgProgram) -> anyhow::Result<Vec<CheckedInvariant>> {
    let refusals = match pg.plan() {
        InvariantPlan::Compiled => &[][..],
        InvariantPlan::Interpreted { refusals } | InvariantPlan::Mixed { refusals, .. } => refusals,
    };
    pg.prepared()
        .program()
        .invariants
        .iter()
        .map(|inv| {
            Ok(match refusals.iter().find(|r| r.invariant == inv.name) {
                None => CheckedInvariant {
                    name: inv.name.to_string(),
                    refusal: None,
                    route: "compiled",
                },
                Some(r) => CheckedInvariant {
                    name: inv.name.to_string(),
                    refusal: Some(CheckRefusal {
                        kind: refusal_kind(&r.reason)?,
                        message: r.reason.to_string(),
                    }),
                    route: "interpreted",
                },
            })
        })
        .collect()
}

/// The report's own name for a refusal. The list is part of the result
/// contract, so a compiler reason it does not name is an error until the
/// schema names it, never folded into a kind that already exists. A
/// validated programme cannot produce the defensive unvalidated-shape
/// reason, so it has no kind either.
fn refusal_kind(reason: &CompileReason) -> anyhow::Result<&'static str> {
    Ok(match reason {
        CompileReason::Construct { .. } => "construct",
        CompileReason::ComparisonDomain { .. } => "comparison_domain",
        CompileReason::ArgumentKind { .. } => "argument_kind",
        CompileReason::Literal { .. } => "literal",
        CompileReason::SumShape { .. } => "sum_shape",
        CompileReason::ComparisonShape { .. } => "comparison_shape",
        other => anyhow::bail!(
            "the check report has no kind for the refusal \"{other}\"; \
             the result schema must name it before check can report it"
        ),
    })
}

/// The `--verbose` success summary: the file path, programme name, a count
/// per declaration kind, and how invariants are checked (compiled to SQL,
/// interpreted, or mixed, with each refusal named).
fn summary(p: &Program, routes: &[CheckedInvariant], file: &Path) -> String {
    let mut out = format!(
        "ok: {}\nprogram: {}\n  predicates: {}\n  definitions: {}\n  invariants: {}\n  transformations: {}\n  intents: {}\n  derived claims: {}\n",
        file.display(),
        p.name,
        p.predicates.len(),
        p.definitions.len(),
        p.invariants.len(),
        p.transformations.len(),
        p.intents.len(),
        p.derived_claims.len(),
    );
    let refused: Vec<(&str, &str)> = routes
        .iter()
        .filter_map(|r| {
            r.refusal
                .as_ref()
                .map(|f| (r.name.as_str(), f.message.as_str()))
        })
        .collect();
    let compiled = routes.len() - refused.len();
    if refused.is_empty() {
        out.push_str("  invariant checks: compiled\n");
    } else if compiled == 0 {
        out.push_str("  invariant checks: interpreted\n");
    } else {
        out.push_str(&format!(
            "  invariant checks: mixed, {compiled} compiled, {} interpreted\n",
            refused.len()
        ));
    }
    for (name, message) in refused {
        out.push_str(&format!("    {name}: {message}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use morpholog_core::ir_builder::program;
    use morpholog_core::{OrderedDomain, PredicateArgKind};
    use std::collections::BTreeSet;

    /// The kinds `check` can emit are exactly the kinds the result schema
    /// publishes: one per reachable compiler reason, none stale in the
    /// schema, and the defensive unvalidated-shape reason has none.
    #[test]
    fn the_refusal_kinds_are_exactly_the_published_list() {
        let reachable = [
            CompileReason::Construct { construct: "or" },
            CompileReason::ComparisonDomain {
                domain: OrderedDomain::Duration,
            },
            CompileReason::ArgumentKind {
                kind: PredicateArgKind::Any,
            },
            CompileReason::Literal { kind: "duration" },
            CompileReason::SumShape { detail: "shape" },
            CompileReason::ComparisonShape { detail: "shape" },
        ];
        let emitted: BTreeSet<&str> = reachable.iter().map(|r| refusal_kind(r).unwrap()).collect();
        assert_eq!(emitted.len(), reachable.len(), "two reasons share a kind");
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../schemas/result.json")).unwrap();
        let published: BTreeSet<&str> =
            schema["$defs"]["check_refusal"]["properties"]["kind"]["enum"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
        assert_eq!(emitted, published);
        assert!(
            refusal_kind(&CompileReason::UnvalidatedShape {
                detail: String::new()
            })
            .is_err()
        );
    }

    #[test]
    fn summary_names_the_program_and_counts_each_declaration_kind() {
        let p = PgProgram::new(PreparedProgram::new(program("demo").build()).unwrap());
        let routes = invariant_routes(&p).unwrap();
        let s = summary(p.prepared().program(), &routes, Path::new("demo.morph"));
        assert_eq!(
            s,
            "ok: demo.morph\nprogram: demo\n  predicates: 0\n  definitions: 0\n  invariants: 0\n  transformations: 0\n  intents: 0\n  derived claims: 0\n  invariant checks: compiled\n"
        );
    }
}
