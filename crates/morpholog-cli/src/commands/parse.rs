//! `morpholog parse` - source file to `Program` JSON.

use crate::SourceFileArgs;
use crate::commands::{parse_or_exit, print_json};

/// Run the `parse` subcommand. Reads the `.morph` file at the given
/// path, parses it, and either prints the resulting `Program` as
/// pretty JSON (on success) or renders each diagnostic to stderr via
/// ariadne (on failure, exiting non-zero).
///
/// `Program` does not derive `Serialize` directly today, so the CLI
/// emits a small projection rather than the full IR. When the IR
/// types pick up `Serialize`, this can collapse to a direct
/// `print_json(&program)` call.
pub(crate) fn run(args: SourceFileArgs) -> anyhow::Result<()> {
    let (program, _source, _source_name) = parse_or_exit(&args.file)?;

    // Invariant bodies are projected as rendered strings because
    // `Prop` doesn't derive `Serialize`. Predicate declarations carry
    // `Serialize` and roundtrip into structured JSON as before.
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

    // Definition bodies are projected as rendered strings like
    // invariant bodies; parameters are bare names.
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

    // Transformation bodies are projected as rendered strings for the
    // same reason as invariant bodies: `Stmt`, `Prop`, and `ValueExpr`
    // don't yet derive `Serialize`.
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

    // Derived claims are projected with their rendered domain and
    // values for the same reason.
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
