//! `morpholog check` - parse + validate a `.morph` source file.

use crate::CheckArgs;
use crate::commands::parse_or_exit;
use morpholog_core::Program;
use std::path::Path;

/// Run the `check` subcommand. Parse + validate the source file,
/// surface diagnostics with a uniform shape from either layer.
///
/// - Parse failure: render parse diagnostics via ariadne, exit 1.
/// - Validation failure: render validation errors as plain
///   `error: <message>` lines (the kernel's `ValidationError`
///   carries no source span), exit 1.
/// - Both clean: print nothing and exit 0, or print a one-screen
///   summary under `--verbose`. Scripts rely on the silent default;
///   the summary is for a human who wants confirmation of what was
///   just validated.
pub(crate) fn run(args: CheckArgs) -> anyhow::Result<()> {
    let (program, _source, _source_name) = parse_or_exit(&args.file)?;

    if let Err(errors) = program.validate() {
        for err in &errors {
            // `ValidationError` carries a `Display` impl with the
            // canonical phrasing; no per-variant rewording here.
            eprintln!("error: {err}");
        }
        std::process::exit(1);
    }

    if args.verbose {
        print!("{}", summary(&program, &args.file));
    }

    Ok(())
}

/// The `--verbose` success summary: programme name and a count per
/// declaration kind, echoing the file path the caller passed.
fn summary(program: &Program, file: &Path) -> String {
    format!(
        "ok: {}\nprogram: {}\n  predicates: {}\n  definitions: {}\n  invariants: {}\n  transformations: {}\n  intents: {}\n  derived claims: {}\n",
        file.display(),
        program.name,
        program.predicates.len(),
        program.definitions.len(),
        program.invariants.len(),
        program.transformations.len(),
        program.intents.len(),
        program.derived_claims.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use morpholog_core::ir_builder::program;

    #[test]
    fn summary_names_the_program_and_counts_each_declaration_kind() {
        let p = program("demo").build();
        let s = summary(&p, Path::new("demo.morph"));
        assert_eq!(
            s,
            "ok: demo.morph\nprogram: demo\n  predicates: 0\n  definitions: 0\n  invariants: 0\n  transformations: 0\n  intents: 0\n  derived claims: 0\n"
        );
    }
}
