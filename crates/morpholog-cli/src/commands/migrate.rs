//! `morpholog migrate` - bring an existing database up to the schema this
//! binary expects.
//!
//! The migrations ship inside the binary, since a release carries no SQL
//! files. `--check` answers, before any workload runs: is this database
//! ready for this binary?

use anyhow::Context;
use morpholog_postgres::{
    BackfillOutcome, BackfillPhase, MigrationReport, PgPool, apply_migrations, deployment_roles,
    migration_check, migration_status, other_sessions, single_connection_pool, with_default_user,
};

use crate::MigrateArgs;
use crate::commands::{AlreadyReported, print_json, warn_if_roles_shared};

pub(crate) async fn run(args: MigrateArgs) -> anyhow::Result<()> {
    // One connection, so the sessions census never counts this command's
    // own second one.
    let pool = single_connection_pool(&with_default_user(&args.db.database_url))
        .await
        .context("failed to connect to PostgreSQL")?;

    if args.check {
        let report = migration_check(&pool)
            .await
            .context("reading the database's migration state failed")?;
        if !report.pending.is_empty() {
            warn_about_other_sessions(&pool).await?;
        }
        note_backfill_preview(&report);
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

    let before = migration_status(&pool)
        .await
        .context("reading the database's migration state failed")?;
    if !before.pending.is_empty() {
        warn_about_other_sessions(&pool).await?;
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

/// Other logins on the database while an upgrade is outstanding. A
/// warning, never a refusal: an idle client is not a hazard, and the
/// census cannot prove every process has stopped.
async fn warn_about_other_sessions(pool: &PgPool) -> anyhow::Result<()> {
    let sessions = other_sessions(pool).await?;
    if sessions.is_empty() {
        return Ok(());
    }
    let listed = sessions
        .iter()
        .map(|s| format!("`{}` ({})", s.role, s.sessions))
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!(
        "warning: other sessions are connected to this database while migrations are \
         pending: {listed}. Stop every process first; one on the old binary keeps writing \
         until it is restarted"
    );
    Ok(())
}

/// The one forecast an operator acts on before migrating: the shared pair
/// is about to be recorded as this deployment's own.
fn note_backfill_preview(report: &MigrationReport) {
    if let Some(backfill) = &report.role_backfill
        && backfill.phase == BackfillPhase::Preview
        && backfill.outcome == BackfillOutcome::RecordPair
        && let Some((writer, reader)) = backfill.roles()
    {
        eprintln!(
            "note: from the grants it finds now, migration 023 would record `{writer}` and \
             `{reader}` as this deployment's roles. A deployment isolated by hand should \
             move to its own roles after migrating: see docs/install.md, \"Moving a \
             deployment to its own roles\""
        );
    }
}
