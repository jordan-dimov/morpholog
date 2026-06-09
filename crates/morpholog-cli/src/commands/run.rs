//! `morpholog run` - parse and validate a `.morph` source file, then
//! propose a named transformation against a Morpholog PostgreSQL
//! database. The CLI's commit path: JSON-encoded args, a required
//! `--actor`, optional `--trace`, committed/rejected JSON output and the
//! matching exit code.
//!
//! This closes the input boundary of the compute/commit/outbox split:
//! an external system proposes against its own `.morph` programme by
//! path, without forking the CLI or compiling Rust.

use anyhow::Context;
use morpholog_core::{Subject, Transition, explain};
use morpholog_postgres::{
    PgProposalOutcome, PgTracedOutcome, propose_against_pg,
    propose_against_pg_with_rejection_state, propose_against_pg_with_trace,
};

use crate::RunArgs;
use crate::commands::args::{CliArgs, decode_args};
use crate::commands::{
    connect, lookup_transformation, parse_or_exit, print_json, validate_or_exit,
};

pub(crate) async fn run(args: RunArgs) -> anyhow::Result<()> {
    // 1. Parse the source file. Exits on parse failure with rendered
    //    diagnostics (same path `check` and `parse` use).
    let (program, _source, _source_name) = parse_or_exit(&args.file)?;

    // 2. Validate. Same error shape as `check`; exits non-zero on
    //    validation failure so a malformed programme never reaches
    //    the proposal path. The returned `ValidatedProgram` handle
    //    threads through to the codec so it does not re-validate.
    let validated = validate_or_exit(&program);

    // 3. Resolve the transformation.
    let transformation = lookup_transformation(&program, &args.transformation, &args.file)?;

    // 4. Decode --args or --args-named into `Vec<EvalValue>`. Clap
    //    has already enforced exactly-one-of via `conflicts_with` +
    //    `required_unless_present`, so `unwrap_either` would be
    //    safe; the explicit match keeps the intent clear and gives
    //    the codec a typed handle.
    let codec_input = match (&args.args, &args.args_named) {
        (Some(tagged), None) => CliArgs::Tagged(tagged.as_str()),
        (None, Some(named)) => CliArgs::Named(named.as_str()),
        _ => unreachable!("clap enforces exactly-one-of `--args` and `--args-named`"),
    };
    let eval_args = decode_args(&validated, transformation, &args.file, codec_input)?;

    // 5. Connect and propose. Same retry caveat as `propose`:
    //    `PgError::SerializationFailure` is the caller's to retry.
    let pool = connect(&args.db.database_url).await?;
    let transition = Transition {
        transformation_name: transformation.name.clone(),
        args: eval_args,
        actor: Subject::from(args.actor.clone()),
    };

    if args.trace {
        let traced = propose_against_pg_with_trace(
            &pool,
            transformation,
            &transition,
            &program.invariants,
            &program.definitions,
        )
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
    } else if args.explain_on_reject {
        // Same-snapshot diagnosis: the variant hands back the exact
        // pre-state the gates evaluated, and the explanation engine
        // (pure, in-memory) runs against it - never a second read
        // that could describe different state than the one that
        // refused.
        let (outcome, rejection_state) = propose_against_pg_with_rejection_state(
            &pool,
            transformation,
            &transition,
            &program.invariants,
            &program.definitions,
        )
        .await
        .context("propose_against_pg_with_rejection_state failed")?;
        match (&outcome, rejection_state) {
            (PgProposalOutcome::Rejected { reason }, Some(state)) => {
                let explanation = explain(&program, &transition, &state);
                print_json(&serde_json::json!({
                    "status": "rejected",
                    "reason": reason,
                    "explanation": explanation,
                }))?;
                std::process::exit(1);
            }
            _ => print_json(&outcome)?,
        }
    } else {
        let outcome = propose_against_pg(
            &pool,
            transformation,
            &transition,
            &program.invariants,
            &program.definitions,
        )
        .await
        .context("propose_against_pg failed")?;
        print_json(&outcome)?;
        if matches!(outcome, PgProposalOutcome::Rejected { .. }) {
            std::process::exit(1);
        }
    }
    Ok(())
}
