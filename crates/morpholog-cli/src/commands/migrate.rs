//! `morpholog migrate` - bring an existing database up to the schema this
//! binary expects.
//!
//! The migrations ship inside the binary, since a release carries no SQL
//! files. `--check` answers, before any workload runs: is this database
//! ready for this binary?

use anyhow::Context;
use morpholog_postgres::{apply_migrations, deployment_roles, migration_status};

use crate::MigrateArgs;
use crate::commands::{AlreadyReported, connect_unchecked, print_json, warn_if_roles_shared};

pub(crate) async fn run(args: MigrateArgs) -> anyhow::Result<()> {
    let pool = connect_unchecked(&args.db.database_url).await?;

    if args.check {
        let report = migration_status(&pool)
            .await
            .context("reading the database's migration state failed")?;
        // Ask the whole report, not just `pending`. A database ahead of
        // this binary has nothing pending but is not ready: an older binary
        // cannot know whether a newer migration still fits it.
        let ready = report.is_current();
        print_json(&report)?;
        if !ready {
            // The report is already on stdout, so the caller sees what is
            // outstanding.
            return Err(AlreadyReported.into());
        }
        return Ok(());
    }

    let report = apply_migrations(&pool)
        .await
        .context("applying migrations failed")?;
    // Also with nothing to apply: a restore can land a current database
    // whose recorded roles already serve another.
    if let Some(roles) = deployment_roles(&pool).await? {
        warn_if_roles_shared(&pool, &roles).await?;
    }
    print_json(&report)
}
