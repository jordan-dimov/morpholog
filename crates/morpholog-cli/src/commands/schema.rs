//! `morpholog schema` - emit the JSON Schema describing a
//! transformation's argument object, or (with `--intent <Type>`) an
//! emitted intent's payload object.
//!
//! Thin wrapper over [`morpholog_core::transformation_arg_schema`] and
//! [`morpholog_core::intent_arg_schema`]. Reads the `.morph`, validates,
//! calls the function, prints the resulting JSON Schema. The library
//! surface made the kernel self-describing; this subcommand carries the
//! same contract through the CLI so a non-Rust embedder can fetch a
//! typed contract via `subprocess.run` - the transformation schema to
//! build a request, the intent schema to decode an outbox payload by
//! name instead of by hand-coded position.
//!
//! No `--json` flag: a JSON Schema is JSON by definition. Errors
//! follow the existing diagnostic style - parse failures via
//! ariadne (handled by [`parse_or_exit`]), validation failures via
//! plain `error: <message>` lines (handled by [`validate_or_exit`]),
//! unknown transformation / intent as a single `error:` line.
//!
//! Exits zero on success; non-zero on any error path. The schema
//! itself never carries an error field - operational failures are
//! distinct from a valid schema and should not require the embedder
//! to discriminate at parse time.

use crate::SchemaArgs;
use crate::commands::{parse_or_exit, print_json, validate_or_exit};
use morpholog_core::{
    AnalysisError, IntentName, TransformationName, intent_arg_schema, transformation_arg_schema,
};

pub(crate) fn run(args: SchemaArgs) -> anyhow::Result<()> {
    let (program, _source, _source_name) = parse_or_exit(&args.file)?;
    let validated = validate_or_exit(&program);

    // Clap enforces exactly-one-of `transformation` / `--intent`.
    if let Some(intent) = &args.intent {
        match intent_arg_schema(&validated, &IntentName::from(intent.as_str())) {
            Some(schema) => print_json(&schema),
            None => {
                eprintln!("error: unknown intent `{intent}`");
                std::process::exit(1);
            }
        }
    } else if let Some(transformation) = &args.transformation {
        let name = TransformationName::from(transformation.as_str());
        match transformation_arg_schema(&validated, &name) {
            Ok(schema) => print_json(&schema),
            Err(AnalysisError::UnknownTransformation { name }) => {
                eprintln!("error: unknown transformation `{name}`");
                std::process::exit(1);
            }
        }
    } else {
        unreachable!("clap enforces exactly-one-of `transformation` and `--intent`");
    }
}
