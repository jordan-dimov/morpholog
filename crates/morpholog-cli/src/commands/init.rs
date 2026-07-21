//! `morpholog init` - provision the Morpholog schema in an existing
//! PostgreSQL database, from the canonical schema embedded in the
//! binary. Day-zero provisioning only: it refuses an
//! already-initialised database (or reports and exits zero under
//! `--skip-if-exists`, for deployment entrypoints that may re-run),
//! and it never drops or migrates - schema evolution is the deferred
//! migrations story, not this command's thin edge.
//!
//! `--least-privilege` additionally provisions the writer/reader role
//! floor, so "the governed path is the only way in" holds on a fresh
//! database by default. Idempotent, so it composes with
//! `--skip-if-exists` to retrofit an existing database.

use anyhow::Context;
use morpholog_postgres::{InitOutcome, initialise_schema, provision_least_privilege};

use crate::InitArgs;
use crate::commands::{connect, print_json};
use morpholog_cli::envelopes::{InitReport, LeastPrivilegeReport};

pub(crate) async fn run(args: InitArgs) -> anyhow::Result<()> {
    let pool = connect(&args.db.database_url).await?;
    let status = match initialise_schema(&pool)
        .await
        .context("schema provisioning failed")?
    {
        InitOutcome::Initialised => "initialised",
        InitOutcome::AlreadyInitialised if args.skip_if_exists => "already-initialised",
        InitOutcome::AlreadyInitialised => {
            eprintln!(
                "error: the `morpholog` schema already exists in this database. \
                 init provisions once and never drops or migrates; if this is a \
                 deployment entrypoint that may re-run, pass --skip-if-exists."
            );
            std::process::exit(1);
        }
    };
    let least_privilege = if args.least_privilege {
        provision_least_privilege(&pool)
            .await
            .context("least-privilege provisioning failed")?;
        Some(LeastPrivilegeReport::applied())
    } else {
        None
    };
    print_json(&InitReport {
        least_privilege,
        schema: "morpholog",
        status,
    })
}
