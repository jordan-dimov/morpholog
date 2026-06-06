//! `morpholog verify` - replay the audit log against the claims table.

use anyhow::Context;
use morpholog_postgres::{VerifyOutcome, verify_replay};

use crate::VerifyArgs;
use crate::commands::{connect, print_json};

/// Run the `verify` subcommand: replay, diff, report. The outcome JSON
/// goes to stdout either way; the exit code is the verdict (consistent
/// exits zero, divergent exits one) - the same data-on-stdout,
/// exit-code-as-verdict shape as `run`.
pub(crate) async fn run(args: VerifyArgs) -> anyhow::Result<()> {
    let pool = connect(&args.database_url).await?;
    let outcome = verify_replay(&pool).await.context("verify_replay failed")?;
    print_json(&outcome)?;
    if matches!(outcome, VerifyOutcome::Divergent { .. }) {
        std::process::exit(1);
    }
    Ok(())
}
