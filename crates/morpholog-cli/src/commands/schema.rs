//! `morpholog schema` - emit the JSON Schema describing a
//! transformation's argument object, or (with `--intent <Type>`) an
//! emitted intent's payload object.
//!
//! A thin wrapper over [`morpholog_core::transformation_arg_schema`] and
//! [`morpholog_core::intent_arg_schema`], so a non-Rust embedder can fetch
//! the typed contract: the transformation schema to build a request, the
//! intent schema to decode an outbox payload by name.
//!
//! Parse and validation failures render as caret blocks (via
//! [`parse_or_report`] and [`validate_or_report`]); an unknown
//! transformation or intent is one `error:` line. Any error exits non-zero,
//! and the schema itself never carries an error field.

use crate::SchemaArgs;
use crate::commands::{AlreadyReported, parse_or_report, print_json, validate_or_report};
use anyhow::Context;
use morpholog_core::{
    AnalysisError, IntentName, TransformationName, intent_arg_schema, transformation_arg_schema,
};

pub(crate) fn run(args: SchemaArgs) -> anyhow::Result<()> {
    if args.result {
        // The result contract is the same for every programme and built
        // into the binary. Parsing it first means a broken document cannot
        // ship, and the output is formatted like all the rest.
        let document: serde_json::Value =
            serde_json::from_str(include_str!("../schemas/result.json")).context(
                "the embedded result schema failed to parse; the contract test pins its validity",
            )?;
        return print_json(&document);
    }

    let Some(file) = &args.file else {
        // Clap makes this unreachable; bail rather than panic.
        anyhow::bail!("a .morph file is required for every mode except --result");
    };
    let parsed = parse_or_report(file)?;
    let validated = validate_or_report(&parsed)?;
    let program = &parsed.program;

    // Clap enforces exactly-one-of `transformation` / `--intent` /
    // `--all`.
    if args.all {
        // Every contract in one document, stamped with the model hash so
        // generated code records which rules it was built against. The
        // `*_order` arrays carry declaration order, since JSON key order
        // is not a contract.
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
                Err(AlreadyReported.into())
            }
        }
    } else if let Some(transformation) = &args.transformation {
        let name = TransformationName::from(transformation.as_str());
        match transformation_arg_schema(&validated, &name) {
            Ok(schema) => print_json(&schema),
            Err(AnalysisError::UnknownTransformation { name }) => {
                eprintln!("error: unknown transformation `{name}`");
                Err(AlreadyReported.into())
            }
        }
    } else {
        unreachable!("clap enforces exactly-one-of `transformation`, `--intent`, and `--all`");
    }
}
