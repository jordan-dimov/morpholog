//! `morpholog check` - parse + validate a `.morph` source file.

use crate::SourceFileArgs;
use crate::commands::parse_or_exit;

/// Run the `check` subcommand. Parse + validate the source file,
/// surface diagnostics with a uniform shape from either layer.
///
/// - Parse failure: render parse diagnostics via ariadne, exit 1.
/// - Validation failure: render validation errors as plain
///   `error: <message>` lines (the kernel's `ValidationError`
///   carries no source span), exit 1.
/// - Both clean: print nothing, exit 0.
pub(crate) fn run(args: SourceFileArgs) -> anyhow::Result<()> {
    let (program, _source, _source_name) = parse_or_exit(&args.file)?;

    if let Err(errors) = program.validate() {
        for err in &errors {
            // `ValidationError` carries a `Display` impl with the
            // canonical phrasing; no per-variant rewording here.
            eprintln!("error: {err}");
        }
        std::process::exit(1);
    }

    Ok(())
}
