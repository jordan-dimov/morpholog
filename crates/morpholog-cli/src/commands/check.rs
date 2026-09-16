//! `morpholog check` - parse + validate + lint a `.morph` source file.

use crate::CheckArgs;
use crate::commands::{AlreadyReported, print_json};
use anyhow::Context;
use morpholog_cli::envelopes::{CheckDiagnostic, CheckReport};
use morpholog_core::{CompiledProgram, Program};
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
///   collisions. A lint renders at hint severity and the check still
///   passes - lints flag shapes with a deliberate reading. Under
///   `--strict` the same finding is an error and the check fails.
/// - Fully clean: print nothing and exit 0, or a one-screen summary
///   under `--verbose`. Scripts rely on the silent stdout default;
///   findings go to stderr, so that contract holds either way.
///   `--json` is the opt-in machine-readable stdout shape: one object
///   carrying every finding with byte offsets and line/column, same
///   exit semantics.
pub(crate) fn run(args: CheckArgs) -> anyhow::Result<()> {
    let collected = collect(&args)?;
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
        };
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        for f in &collected.findings {
            match (&f.diagnostic, &f.foreign) {
                (Some(d), Some((name, source))) => eprint!("{}", d.render(name, source)),
                (Some(d), None) => {
                    eprint!("{}", d.render(&collected.source_name, &collected.source));
                }
                (None, _) => eprintln!("{}: {}", f.severity, f.message),
            }
        }
    }
    if collected.failed {
        return Err(AlreadyReported.into());
    }
    if let Some(program) = &collected.program {
        if args.verbose {
            print!("{}", summary(program, &args.file));
        }
        if args.ir {
            return print_ir(program);
        }
    }
    Ok(())
}

/// One finding, with the caret-located diagnostic when a source map
/// places it. A finding about another file (an `--against` programme
/// that does not validate) carries that file's own source so the plain
/// renderer can still show its carets; the JSON report has one `file`,
/// so there it is reported without a location rather than a lying one.
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
    /// The validated programme, for `--verbose` and `--ir`; absent when
    /// parsing or validation failed.
    program: Option<Program>,
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
        program: None,
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
    // Constructing the `CompiledProgram` is the validation gate: `Err`
    // carries the same errors `program.validate()` would, and `Ok` is
    // the compiled programme the lints run against - validated once.
    let compiled = match CompiledProgram::new(program) {
        Ok(compiled) => compiled,
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
    for finding in &morpholog_postgres::validate_declarations(compiled.program()) {
        out.failed = true;
        out.findings.push(Finding::error(finding.to_string(), None));
    }
    for lint in &morpholog_core::lints(&compiled) {
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
                for lint in &morpholog_core::shared_writer_lints(compiled.program(), &other) {
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
    out.program = Some(compiled.program().clone());
    Ok(out)
}

/// `--against` the file being checked would report every write as a
/// collision with itself: nonsensical input, refused.
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

/// The programme behind an `--against` path, cleared to the floor
/// `check` holds the primary file to - parse, validation, declaration
/// policy; its own lints are its own `check`'s business - or every
/// reason it is not, as findings naming the path and carrying the
/// other file's source for their carets.
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
    let compiled = CompiledProgram::new(program).map_err(|errors| {
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
    let policy = morpholog_postgres::validate_declarations(compiled.program());
    if !policy.is_empty() {
        return Err(policy
            .iter()
            .map(|f| Finding::error(format!("against {against}: {f}"), None))
            .collect());
    }
    Ok(compiled.program().clone())
}

/// Print the validated programme's internal representation as pretty
/// JSON - `check --ir`, the debugging view. Behind validation, so only a
/// sound programme renders.
///
/// `Program` does not derive `Serialize` directly today, so the CLI
/// emits a projection: declarations roundtrip structurally, while
/// invariant, definition, transformation, and derived-claim bodies
/// render through the canonical formatter. When the IR types pick up
/// `Serialize`, this can collapse to a direct `print_json(&program)`.
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

/// The `--verbose` success summary: programme name and a count per
/// declaration kind, echoing the file path the caller passed.
fn summary(program: &Program, file: &Path) -> String {
    format!(
        "ok: {}\nprogram: {}\n  predicates: {}\n  definitions: {}\n  invariants: {}\n  transformations: {}\n  intents: {}\n  derived claims: {}\n",
        file.display(),
        program.name,
        program.predicates.len(),
        program.definitions.len(),
        program.invariants.len(),
        program.transformations.len(),
        program.intents.len(),
        program.derived_claims.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use morpholog_core::ir_builder::program;

    #[test]
    fn summary_names_the_program_and_counts_each_declaration_kind() {
        let p = program("demo").build();
        let s = summary(&p, Path::new("demo.morph"));
        assert_eq!(
            s,
            "ok: demo.morph\nprogram: demo\n  predicates: 0\n  definitions: 0\n  invariants: 0\n  transformations: 0\n  intents: 0\n  derived claims: 0\n"
        );
    }
}
