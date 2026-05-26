//! `morpholog propose` - run a transformation against a Morpholog
//! PostgreSQL database.

use anyhow::{Context, anyhow};
use morpholog_core::{EvalValue, Subject, Transition};
use morpholog_postgres::{
    PgProposalOutcome, PgTracedOutcome, propose_against_pg, propose_against_pg_with_trace,
};

use crate::ProposeArgs;
use crate::commands::{connect, print_json};

/// Run the `propose` subcommand end-to-end: look up the named program
/// and transformation, parse the JSON `--args` as a `Vec<EvalValue>`,
/// open a SERIALIZABLE PostgreSQL transaction via `propose_against_pg`,
/// and print the outcome as JSON.
///
/// Exit-code semantics:
/// - The outcome is *always* printed to stdout as JSON, both on commit
///   and on business rejection. Callers can distinguish by reading the
///   `"status"` field.
/// - On `Committed`, the function returns `Ok(())` and the process
///   exits 0 via `main`'s default.
/// - On `Rejected`, the function calls `std::process::exit(1)` after
///   printing, so scripts can detect business rejection without parsing
///   JSON.
/// - On any earlier error (unknown program/transformation, malformed
///   `--args` JSON, connection failure), the function returns `Err`
///   and anyhow's default exit path prints the error chain to stderr
///   and exits 1. Stdout vs stderr distinguishes the two failure
///   modes; in both cases the exit code is 1.
pub(crate) async fn run(args: ProposeArgs) -> anyhow::Result<()> {
    // 1. Resolve the program from the built-in registry.
    let programs = morpholog_examples::all_programs();
    let program = programs
        .iter()
        .find(|p| p.name == args.program)
        .ok_or_else(|| {
            let available = programs
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow!(
                "program `{}` not found. Available built-in programs: {}",
                args.program,
                available
            )
        })?;

    // 2. Resolve the transformation within the program.
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
                "transformation `{}` not found in program `{}`. Available: {}",
                args.transformation,
                args.program,
                available
            )
        })?;

    // 3. Parse --args as a JSON array of EvalValues using the same
    //    tagged codec that `morpholog inspect` emits, so output of one
    //    invocation can plausibly be piped into the input of another.
    let eval_args: Vec<EvalValue> = serde_json::from_str(&args.args).context(
        "failed to parse --args as a JSON array of EvalValues \
         (each element must be a tagged object such as \
         `{\"type\":\"subject\",\"value\":\"...\"}` or \
         `{\"type\":\"decimal\",\"value\":\"100\"}`)",
    )?;

    // 4. Connect and propose.
    //
    // Known limitation: `PgError::SerializationFailure` (PostgreSQL
    // SSI conflict; SQLSTATE 40001) is documented as caller-retryable,
    // but the CLI surfaces it as a single failed invocation instead of
    // retrying internally. Acceptable for the developer/operator use
    // case the CLI is built for; concurrent `morpholog propose`
    // pipelines should add their own retry wrapper.
    let pool = connect(&args.database_url).await?;
    let transition = Transition {
        transformation_name: transformation.name.clone(),
        args: eval_args,
        actor: Subject::from(args.actor.clone()),
    };
    // 5. Propose, with or without trace, and emit JSON accordingly.
    //    Trace branch emits `{"result": ..., "trace": [...]}` for every
    //    kernel-side outcome (Committed, Rejected, and Errored).
    //    Non-trace branch emits the bare outcome (its existing wire
    //    shape) so scripts that parse stdout don't break. PG-layer
    //    errors surface via anyhow on both branches.
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
                // Kernel error with structured trace preserved. Emit
                // a tagged "errored" result alongside the trace so
                // downstream JSON consumers can distinguish from a
                // lawful rejection, then exit non-zero.
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
