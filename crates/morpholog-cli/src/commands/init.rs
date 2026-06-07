//! `morpholog init` - provision the Morpholog schema in an existing
//! PostgreSQL database, from the canonical schema embedded in the
//! binary. Day-zero provisioning only: it refuses an
//! already-initialised database (or reports and exits zero under
//! `--skip-if-exists`, for deployment entrypoints that may re-run),
//! and it never drops or migrates - schema evolution is the deferred
//! migrations story, not this command's thin edge.

use anyhow::Context;
use morpholog_postgres::{InitOutcome, initialise_schema};

use crate::InitArgs;
use crate::commands::{connect, print_json};

pub(crate) async fn run(args: InitArgs) -> anyhow::Result<()> {
    let pool = connect(&args.database_url).await?;
    match initialise_schema(&pool)
        .await
        .context("initialise_schema failed")?
    {
        InitOutcome::Initialised => print_json(&serde_json::json!({
            "status": "initialised",
            "schema": "morpholog",
        })),
        InitOutcome::AlreadyInitialised if args.skip_if_exists => print_json(&serde_json::json!({
            "status": "already-initialised",
            "schema": "morpholog",
        })),
        InitOutcome::AlreadyInitialised => {
            eprintln!(
                "error: the `morpholog` schema already exists in this database. \
                 init provisions once and never drops or migrates; if this is a \
                 deployment entrypoint that may re-run, pass --skip-if-exists."
            );
            std::process::exit(1);
        }
    }
}
