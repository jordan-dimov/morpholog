//! `morpholog schema` - emit the JSON Schema describing a
//! transformation's argument object.
//!
//! Thin wrapper over [`morpholog_core::transformation_arg_schema`].
//! Reads the `.morph`, validates, calls the function, prints the
//! resulting JSON Schema. The library surface from PR #112 made
//! the kernel self-describing; this subcommand carries the same
//! contract through the CLI so a non-Rust embedder can fetch a
//! typed input contract via `subprocess.run` without needing a
//! Rust toolchain.
//!
//! No `--json` flag: a JSON Schema is JSON by definition. Errors
//! follow the existing diagnostic style - parse failures via
//! ariadne (handled by [`parse_or_exit`]), validation failures via
//! plain `error: <message>` lines (handled by [`validate_or_exit`]),
//! unknown transformation as a single `error:` line.
//!
//! Exits zero on success; non-zero on any error path. The schema
//! itself never carries an error field - operational failures are
//! distinct from a valid schema and should not require the embedder
//! to discriminate at parse time.

use crate::SchemaArgs;
use crate::commands::{parse_or_exit, print_json, validate_or_exit};
use morpholog_core::{AnalysisError, TransformationName, transformation_arg_schema};

pub(crate) fn run(args: SchemaArgs) -> anyhow::Result<()> {
    let (program, _source, _source_name) = parse_or_exit(&args.file)?;
    validate_or_exit(&program);

    let transformation = TransformationName::from(args.transformation.as_str());
    match transformation_arg_schema(&program, &transformation) {
        Ok(schema) => print_json(&schema),
        Err(AnalysisError::UnknownTransformation { name }) => {
            eprintln!("error: unknown transformation `{name}`");
            std::process::exit(1);
        }
        Err(AnalysisError::ProgramInvalid(errors)) => {
            // Defensive: validate_or_exit already returned for an
            // invalid programme, so this arm is unreachable today.
            // Surfacing the errors instead of panicking keeps the
            // CLI honest if a future refactor changes the
            // analysis-vs-validation contract.
            for err in &errors {
                eprintln!("error: {err}");
            }
            std::process::exit(1);
        }
    }
}
