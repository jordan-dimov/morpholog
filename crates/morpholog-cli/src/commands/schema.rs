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
//! follow the existing diagnostic style - parse and validation
//! failures via ariadne caret blocks (handled by [`parse_or_exit`]
//! and [`validate_or_exit`]), unknown transformation / intent as a
//! single `error:` line.
//!
//! Exits zero on success; non-zero on any error path. The schema
//! itself never carries an error field - operational failures are
//! distinct from a valid schema and should not require the embedder
//! to discriminate at parse time.

use crate::SchemaArgs;
use crate::commands::{parse_or_exit, print_json, validate_or_exit};
use anyhow::Context;
use morpholog_core::{
    AnalysisError, IntentName, TransformationName, intent_arg_schema, transformation_arg_schema,
};

pub(crate) fn run(args: SchemaArgs) -> anyhow::Result<()> {
    if args.result {
        // The outcome-envelope contract is programme-independent and
        // pinned in the binary. Parse-then-print rather than printing
        // the raw bytes: the binary cannot ship a syntactically broken
        // document, and the output stays print_json-canonical.
        let document: serde_json::Value =
            serde_json::from_str(include_str!("../schemas/result.json")).context(
                "the embedded result schema failed to parse; the contract test pins its validity",
            )?;
        return print_json(&document);
    }

    let Some(file) = &args.file else {
        // Clap's required_unless_present("result") makes this
        // unreachable; the bail keeps the invariant honest without a
        // panic path in the binary.
        anyhow::bail!("a .morph file is required for every mode except --result");
    };
    let parsed = parse_or_exit(file)?;
    let validated = validate_or_exit(&parsed);
    let program = &parsed.program;

    // Clap enforces exactly-one-of `transformation` / `--intent` /
    // `--all`.
    if args.all {
        // The manifest: every contract in one artefact, stamped with
        // the canonical model hash so generated code can record
        // exactly which rules it was built against. Keyed objects give
        // codegen lookup by name; the *_order arrays carry declaration
        // order explicitly, because JSON object key order is not a
        // contract (serde_json's map is sorted, and embedders should
        // not rely on object ordering anyway) - the manifest-level
        // analogue of x-morpholog-arg-order.
        let transformation_order: Vec<String> = program
            .transformations
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        let intent_order: Vec<String> =
            program.intents.iter().map(|i| i.name.to_string()).collect();
        let mut transformations = serde_json::Map::new();
        for t in &program.transformations {
            let Ok(schema) = transformation_arg_schema(&validated, &t.name) else {
                unreachable!(
                    "declared transformation `{}` missing from its own programme",
                    t.name
                )
            };
            transformations.insert(t.name.to_string(), schema);
        }
        let mut intents = serde_json::Map::new();
        for i in &program.intents {
            let Some(schema) = intent_arg_schema(&validated, &i.name) else {
                unreachable!(
                    "declared intent `{}` missing from its own programme",
                    i.name
                )
            };
            intents.insert(i.name.to_string(), schema);
        }
        return print_json(&serde_json::json!({
            "program": program.name,
            "hash": crate::commands::hash::canonical_hash(program),
            "predicates": program.predicates,
            "transformation_order": transformation_order,
            "transformations": transformations,
            "intent_order": intent_order,
            "intents": intents,
        }));
    }
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
        unreachable!("clap enforces exactly-one-of `transformation`, `--intent`, and `--all`");
    }
}
