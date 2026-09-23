//! `morpholog explain` - a deterministic, structured account of why a
//! proposed transition would be admitted or rejected against live state.
//!
//! The read-only counterpart of `propose`: same parsing and argument
//! codec, but it loads the relevant state, runs `morpholog_core::explain`
//! in memory, and prints the `Explanation` as prose or JSON.
//!
//! It never writes, and it exits 0 whether the verdict is admit or reject:
//! it answers a question rather than acting. Only operational failures
//! (bad programme or `--args`, unknown transformation, database error) exit
//! non-zero. Scripts that want the gate use `propose`.

use anyhow::Context;
use morpholog_core::{Subject, Transition, explain};
use morpholog_postgres::load_scoped_state;

use crate::ExplainArgs;
use crate::commands::args::{CliArgs, decode_args};
use crate::commands::{
    compile_or_report, connect, lookup_transformation, parse_or_report, print_json,
};

pub(crate) async fn run(args: ExplainArgs) -> anyhow::Result<()> {
    // Same front-end as `propose`: a malformed programme stops here.
    let parsed = parse_or_report(&args.file)?;
    let compiled = compile_or_report(&parsed)?;

    let transformation = lookup_transformation(&compiled, &args.transformation, &args.file)?;

    // The same codec as `propose`, so both accept the same input.
    let codec_input = match (&args.args, &args.args_named) {
        (Some(tagged), None) => CliArgs::Tagged(tagged.as_str()),
        (None, Some(named)) => CliArgs::Named(named.as_str()),
        _ => unreachable!("clap enforces exactly-one-of `--args` and `--args-named`"),
    };
    let eval_args = decode_args(
        &compiled.validated(),
        transformation,
        &args.file,
        codec_input,
    )?;

    let pool = connect(&args.db.database_url).await?;
    let state = load_scoped_state(&pool, &compiled, transformation)
        .await
        .context("failed to load scoped pre-state")?;

    let transition = Transition {
        transformation_name: transformation.name.clone(),
        args: eval_args,
        actor: Subject::from(args.actor.clone()),
    };

    let explanation = explain(compiled.program(), &transition, &state);
    if args.json {
        print_json(&explanation)
    } else {
        println!("{}", explanation.render());
        Ok(())
    }
}
