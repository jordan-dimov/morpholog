//! `morpholog check` - parse + validate + lint a `.morph` source file.

use crate::CheckArgs;
use crate::commands::{ParsedSource, compile_or_exit, parse_or_exit, print_json};
use morpholog_core::{CompiledProgram, Program};
use morpholog_surface::{Diagnostic, line_col, parse_program_with_sources};
use serde::Serialize;
use std::path::Path;

/// Run the `check` subcommand. Parse + validate the source file,
/// surface diagnostics with a uniform shape from either layer.
///
/// - Parse failure: render parse diagnostics via ariadne, exit 1.
/// - Validation failure: render each error as an ariadne caret block
///   when the source map places it (its declaration, or the exact
///   statement), as a plain `error: <message>` line when it has no
///   source anchor. Exit 1.
/// - Both clean: lints run next. A finding renders at hint severity
///   and the check still passes - lints flag shapes with a deliberate
///   reading. Under `--strict` the same finding renders as an error
///   and the check fails.
/// - Fully clean: print nothing and exit 0, or print a one-screen
///   summary under `--verbose`. Scripts rely on the silent stdout
///   default; findings go to stderr, so that contract holds either
///   way. `--json` is the opt-in machine-readable stdout shape: one
///   object carrying every finding with byte offsets and line/column,
///   same exit semantics.
pub(crate) fn run(args: CheckArgs) -> anyhow::Result<()> {
    if args.json {
        return run_json(&args);
    }

    let parsed = parse_or_exit(&args.file)?;
    let compiled = compile_or_exit(&parsed);

    let lints = morpholog_core::lints(&compiled);
    if !lints.is_empty() {
        for lint in &lints {
            render_lint(lint, args.strict, &parsed);
        }
        if args.strict {
            std::process::exit(1);
        }
    }

    if args.verbose {
        print!("{}", summary(&parsed.program, &args.file));
    }
    if args.ir {
        return print_ir(&parsed.program);
    }

    Ok(())
}

/// Print the validated programme's internal representation as pretty
/// JSON - `check --ir`, the debugging view (formerly the `parse`
/// subcommand; now behind validation, so only a sound programme
/// renders).
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
                "body": morpholog_core::format::format_prop_inline(&inv.body),
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
                "body": morpholog_core::format::format_prop_inline(&d.body),
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
                .map(|s| morpholog_core::format::format_stmt(s, 0))
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
                        "expr": morpholog_core::format::format_value_inline(&v.expr),
                    })
                })
                .collect();
            serde_json::json!({
                "predicate": &d.predicate,
                "keys": &d.keys,
                "over": morpholog_core::format::format_prop_inline(&d.domain),
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

/// Render one lint to stderr: a caret block at hint severity (error
/// under `--strict`) when the source map places it, the plain
/// `hint: ...` / `error: ...` line otherwise.
fn render_lint(lint: &morpholog_core::Lint, strict: bool, parsed: &ParsedSource) {
    match parsed.map.span_for_lint(lint) {
        Some(span) => {
            let diagnostic = if strict {
                Diagnostic::error(lint.to_string(), span)
            } else {
                Diagnostic::hint(lint.to_string(), span)
            };
            eprint!("{}", diagnostic.render(&parsed.source_name, &parsed.source));
        }
        None => {
            let label = if strict { "error" } else { "hint" };
            eprintln!("{label}: {lint}");
        }
    }
}

/// One finding in the `--json` output. Byte offsets and 1-based
/// line/column are present when the finding has a source anchor;
/// a finding without one (a generated discipline invariant) carries
/// only severity and message.
#[derive(Serialize)]
struct JsonDiagnostic {
    severity: &'static str,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    start: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    column: Option<usize>,
}

impl JsonDiagnostic {
    fn new(
        severity: &'static str,
        message: String,
        span: Option<morpholog_surface::Span>,
        source: &str,
    ) -> Self {
        let (line, column) = match &span {
            Some(s) => {
                let (l, c) = line_col(source, s.start);
                (Some(l), Some(c))
            }
            None => (None, None),
        };
        Self {
            severity,
            message,
            start: span.as_ref().map(|s| s.start),
            end: span.as_ref().map(|s| s.end),
            line,
            column,
        }
    }
}

/// `check --json`: every finding - parse errors, validation errors,
/// lints - in one stdout object, uniform across layers. Exit
/// semantics match the plain form (`--strict` promotes hints).
fn run_json(args: &CheckArgs) -> anyhow::Result<()> {
    let source = std::fs::read_to_string(&args.file)
        .map_err(|e| anyhow::anyhow!("read source file {}: {e}", args.file.display()))?;

    let mut findings = Vec::new();
    let mut failed = false;

    match parse_program_with_sources(&source) {
        Err(diagnostics) => {
            failed = true;
            for d in diagnostics {
                findings.push(JsonDiagnostic::new(
                    "error",
                    d.message,
                    Some(d.primary),
                    &source,
                ));
            }
        }
        Ok((program, map)) => {
            // Constructing the `CompiledProgram` is the validation gate:
            // `Err` carries the same errors `program.validate()` would, and
            // `Ok` is the compiled programme the lints run against - so the
            // programme is validated once, not twice.
            match CompiledProgram::new(program) {
                Err(errors) => {
                    failed = true;
                    for err in &errors {
                        findings.push(JsonDiagnostic::new(
                            "error",
                            err.to_string(),
                            map.span_for_error(err),
                            &source,
                        ));
                    }
                }
                Ok(compiled) => {
                    for lint in &morpholog_core::lints(&compiled) {
                        let severity = if args.strict { "error" } else { "hint" };
                        failed |= args.strict;
                        findings.push(JsonDiagnostic::new(
                            severity,
                            lint.to_string(),
                            map.span_for_lint(lint),
                            &source,
                        ));
                    }
                }
            }
        }
    }

    let payload = serde_json::json!({
        "file": args.file.display().to_string(),
        "diagnostics": findings,
    });
    println!("{}", serde_json::to_string_pretty(&payload)?);
    if failed {
        std::process::exit(1);
    }
    Ok(())
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
