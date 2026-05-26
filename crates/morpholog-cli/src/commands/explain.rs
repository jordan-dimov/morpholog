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

use anyhow::{Context, anyhow};
use morpholog_core::{EvalValue, Subject, Transition, explain};
use morpholog_postgres::load_scoped_state;

use crate::ExplainArgs;
use crate::commands::{connect, parse_or_exit, print_json};

pub(crate) async fn run(args: ExplainArgs) -> anyhow::Result<()> {
    // Same parse + validate front-end as `run`: a malformed programme
    // never reaches the explanation path.
    let (program, _source, _source_name) = parse_or_exit(&args.file)?;
    if let Err(errors) = program.validate() {
        for err in &errors {
            eprintln!("error: {err}");
        }
        std::process::exit(1);
    }

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

    let eval_args: Vec<EvalValue> = serde_json::from_str(&args.args).context(
        "failed to parse --args as a JSON array of EvalValues \
         (each element must be a tagged object such as \
         `{\"type\":\"subject\",\"value\":\"...\"}` or \
         `{\"type\":\"decimal\",\"value\":\"100\"}`)",
    )?;

    let pool = connect(&args.database_url).await?;
    let state = load_scoped_state(&pool, transformation, &program.invariants)
        .await
        .context("failed to load scoped pre-state")?;

    let transition = Transition {
        transformation_name: transformation.name.clone(),
        args: eval_args,
        actor: Subject::from(args.actor.clone()),
    };

    let explanation = explain(&program, &transition, &state);
    if args.json {
        print_json(&explanation)
    } else {
        println!("{}", explanation.render());
        Ok(())
    }
}
