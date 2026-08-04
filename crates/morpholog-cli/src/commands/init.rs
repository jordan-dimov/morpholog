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

use anyhow::{Context, anyhow};
use morpholog_postgres::{
    InitOutcome, drop_schema, initialise_schema, provision_least_privilege, redact_database_url,
};

use crate::InitArgs;
use crate::commands::{AlreadyReported, connect, print_json};
use morpholog_cli::envelopes::{InitReport, LeastPrivilegeReport};

pub(crate) async fn run(args: InitArgs) -> anyhow::Result<()> {
    // The acknowledgement is checked before connecting: a mistyped
    // production URL should be refused without the binary having
    // touched that database at all.
    if args.i_know_this_deletes_data && !args.reset {
        return Err(anyhow!(
            "--i-know-this-deletes-data is only meaningful with --reset"
        ));
    }
    if args.reset && !args.i_know_this_deletes_data {
        return Err(anyhow!(
            "--reset DROPS the `morpholog` schema and every claim, audit row, and \
             outbox entry in it. Re-run with --i-know-this-deletes-data to \
             acknowledge. Target: {}",
            redact_database_url(&args.db.database_url)
        ));
    }

    let pool = connect(&args.db.database_url).await?;
    let dropped = if args.reset {
        Some(drop_schema(&pool).await.context("schema drop failed")?)
    } else {
        None
    };
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
            return Err(AlreadyReported.into());
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
    // `reset` distinguishes "there was a schema and it is gone" from
    // "there was nothing to drop" - reporting the drop unconditionally
    // would imply removing something that was never there.
    if let Some(existed) = dropped {
        eprintln!(
            "{} the pre-existing `morpholog` schema before provisioning",
            if existed { "dropped" } else { "found no" }
        );
    }
    print_json(&InitReport {
        least_privilege,
        schema: "morpholog",
        status,
    })
}
