//! `morpholog explain` - a deterministic, structured account of why a
//! proposed transition would be admitted or rejected against live state.
//!
//! The read-only counterpart of `run`: the same `.morph` parse/validate
//! path and the same `Transition` codec, but instead of proposing it
//! loads the scoped pre-state, runs the kernel in-memory via
//! `morpholog_core::explain`, and renders the resulting `Explanation` as
//! claim-shaped prose (default) or as the structured JSON object.
//!
//! It never writes, and the verdict never changes the exit code: explain
//! exits zero on both admissible and rejected verdicts, because explaining
//! a transition is answering a question, not taking an action. Only
//! operational failures - a parse or validation error, malformed `--args`,
//! an unknown transformation, a database failure - exit non-zero. A script
//! that wants the gate uses `run`.

use anyhow::Context;
use morpholog_core::{Subject, Transition, explain};
use morpholog_postgres::load_scoped_state;

use crate::ExplainArgs;
use crate::commands::args::{CliArgs, decode_args};
use crate::commands::{
    connect, lookup_transformation, parse_or_exit, print_json, validate_or_exit,
};

pub(crate) async fn run(args: ExplainArgs) -> anyhow::Result<()> {
    // Same parse + validate front-end as `run`: a malformed programme
    // never reaches the explanation path. The returned
    // `ValidatedProgram` handle threads through to the codec.
    let parsed = parse_or_exit(&args.file)?;
    let validated = validate_or_exit(&parsed);
    let program = &parsed.program;

    let transformation = lookup_transformation(program, &args.transformation, &args.file)?;

    // Decode --args or --args-named via the same shared codec `run`
    // uses, so the two paths cannot drift on what is a valid input.
    let codec_input = match (&args.args, &args.args_named) {
        (Some(tagged), None) => CliArgs::Tagged(tagged.as_str()),
        (None, Some(named)) => CliArgs::Named(named.as_str()),
        _ => unreachable!("clap enforces exactly-one-of `--args` and `--args-named`"),
    };
    let eval_args = decode_args(&validated, transformation, &args.file, codec_input)?;

    let pool = connect(&args.db.database_url).await?;
    let state = load_scoped_state(
        &pool,
        transformation,
        &program.invariants,
        &program.definitions,
    )
    .await
    .context("failed to load scoped pre-state")?;

    let transition = Transition {
        transformation_name: transformation.name.clone(),
        args: eval_args,
        actor: Subject::from(args.actor.clone()),
    };

    let explanation = explain(program, &transition, &state);
    if args.json {
        print_json(&explanation)
    } else {
        println!("{}", explanation.render());
        Ok(())
    }
}
