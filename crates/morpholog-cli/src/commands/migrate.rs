//! `morpholog migrate` - bring an existing database up to the schema this
//! binary expects.
//!
//! The gap this closes: `init` provisions and never migrates, so upgrading
//! was an instruction naming a path inside the source tree - and the release
//! artifact contains the binary and a licence, no SQL. An embedder following
//! the versioning policy had to fetch migrations out of a git tag. The
//! binary already detected an out-of-date database and named the remedy; it
//! simply could not apply it.
//!
//! `--check` exists for the question that has to be answerable *before* a
//! workload: is this database ready for this binary?

use anyhow::Context;
use morpholog_postgres::{apply_migrations, migration_status};

use crate::MigrateArgs;
use crate::commands::{connect, print_json};

pub(crate) async fn run(args: MigrateArgs) -> anyhow::Result<()> {
    let pool = connect(&args.db.database_url).await?;

    if args.check {
        let report = migration_status(&pool)
            .await
            .context("reading the database's migration state failed")?;
        let behind = !report.pending.is_empty();
        print_json(&report)?;
        if behind {
            // Non-zero so a deploy step can gate on it. The report is on
            // stdout either way, so the caller reads what is pending rather
            // than only that something is.
            std::process::exit(1);
        }
        return Ok(());
    }

    let report = apply_migrations(&pool)
        .await
        .context("applying migrations failed")?;
    print_json(&report)
}
