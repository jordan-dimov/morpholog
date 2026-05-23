//! `morpholog run` - propose a transformation defined in a `.morph`
//! source file against a Morpholog PostgreSQL database.
//!
//! The non-built-in counterpart of `propose`: where `propose` resolves
//! its program from the built-in registry (`morpholog_examples::all_programs`),
//! `run` parses and validates a user-supplied source file. From that
//! point the two subcommands share the same shape: JSON-encoded args,
//! a required `--actor`, optional `--trace`, identical exit-code
//! semantics, identical output JSON.
//!
//! This closes the input boundary of the compute/commit/outbox split:
//! an external system can now propose against its own programme
//! without forking the CLI or compiling Rust.

use anyhow::{Context, anyhow};
use morpholog_core::{EvalValue, Transition};
use morpholog_postgres::{
    PgProposalOutcome, PgTracedOutcome, propose_against_pg, propose_against_pg_with_trace,
};

use crate::RunArgs;
use crate::commands::{connect, parse_or_exit, print_json};

pub(crate) async fn run(args: RunArgs) -> anyhow::Result<()> {
    // 1. Parse the source file. Exits on parse failure with rendered
    //    diagnostics (same path `check` and `parse` use).
    let (program, _source, _source_name) = parse_or_exit(&args.file)?;

    // 2. Validate. Same error shape as `check`; exits non-zero on
    //    validation failure so a malformed programme never reaches
    //    the proposal path.
    if let Err(errors) = program.validate() {
        for err in &errors {
            eprintln!("error: {err}");
        }
        std::process::exit(1);
    }

    // 3. Resolve the transformation.
    let transformation = program
        .transformation(&args.transformation)
        .ok_or_else(|| {
            let available = program
                .transformations
                .iter()
                .map(|t| t.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow!(
                "transformation `{}` not found in `{}`. Available: {}",
                args.transformation,
                args.file.display(),
                available
            )
        })?;

    // 4. Parse --args as `Vec<EvalValue>`. Same codec as `propose`.
    let eval_args: Vec<EvalValue> = serde_json::from_str(&args.args).context(
        "failed to parse --args as a JSON array of EvalValues \
         (each element must be a tagged object such as \
         `{\"type\":\"subject\",\"value\":\"...\"}` or \
         `{\"type\":\"decimal\",\"value\":\"100\"}`)",
    )?;

    // 5. Connect and propose. Same retry caveat as `propose`:
    //    `PgError::SerializationFailure` is the caller's to retry.
    let pool = connect(&args.database_url).await?;
    let transition = Transition {
        transformation_name: transformation.name.clone(),
        args: eval_args,
        actor: EvalValue::Subject(args.actor.clone()),
    };

    if args.trace {
        let traced =
            propose_against_pg_with_trace(&pool, transformation, &transition, &program.invariants)
                .await
                .context("propose_against_pg_with_trace failed")?;
        match traced {
            PgTracedOutcome::Outcome { outcome, trace } => {
                print_json(&serde_json::json!({
                    "result": &outcome,
                    "trace": &trace,
                }))?;
                if matches!(outcome, PgProposalOutcome::Rejected { .. }) {
                    std::process::exit(1);
                }
            }
            PgTracedOutcome::KernelErrored { error, trace } => {
                print_json(&serde_json::json!({
                    "result": {
                        "status": "errored",
                        "error": format!("{error}"),
                    },
                    "trace": &trace,
                }))?;
                std::process::exit(1);
            }
        }
    } else {
        let outcome = propose_against_pg(&pool, transformation, &transition, &program.invariants)
            .await
            .context("propose_against_pg failed")?;
        print_json(&outcome)?;
        if matches!(outcome, PgProposalOutcome::Rejected { .. }) {
            std::process::exit(1);
        }
    }
    Ok(())
}
