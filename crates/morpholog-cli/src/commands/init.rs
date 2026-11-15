//! `morpholog init` - provision the Morpholog schema, embedded in the
//! binary, in an existing PostgreSQL database. Day-zero only: it refuses an
//! initialised database (or exits zero under `--skip-if-exists`, for
//! entrypoints that re-run). It never migrates; that is `morpholog
//! migrate`. It drops the schema only under an acknowledged `--reset`.
//!
//! `--least-privilege` also provisions the writer and reader roles, so the
//! governed path is the only way in from the start. It is idempotent, so
//! with `--skip-if-exists` it can retrofit an existing database. Each
//! deployment on a cluster names its own roles with `--role-prefix`.
//!
//! `--root-actor` with `--root-login` closes the deployment, a new one or,
//! with `--skip-if-exists`, an existing one. A deployment left open says
//! so on every run.

use anyhow::{Context, anyhow};
use morpholog_core::Subject;
use morpholog_postgres::PgPool;
use morpholog_postgres::{
    DeploymentRoles, InitOutcome, actors_without_grant, close_deployment, drop_schema,
    initialise_schema, provision_least_privilege, rebind_least_privilege, redact_database_url,
    require_can_provision_least_privilege,
};

use crate::InitArgs;
use crate::commands::{
    AlreadyReported, connect_unchecked, print_json, warn_if_open, warn_if_roles_shared,
};
use morpholog_cli::envelopes::{InitReport, LeastPrivilegeReport};

pub(crate) async fn run(args: InitArgs) -> anyhow::Result<()> {
    // Check the acknowledgement before connecting, so a mistyped
    // production URL is refused without touching that database.
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

    let roles = DeploymentRoles::with_prefix(&args.role_prefix)?;

    // Unchecked: this is the command that provisions a database.
    let pool = connect_unchecked(&args.db.database_url).await?;
    if args.least_privilege {
        require_can_provision_least_privilege(&pool, &roles)
            .await
            .context(if args.reset {
                "nothing was dropped"
            } else {
                "nothing was provisioned"
            })?;
    }
    let mut dropped = if args.reset {
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
        // The floor is applied to an existing schema too, so that schema
        // must be one this binary serves.
        if status == "already-initialised" {
            morpholog_postgres::require_current_schema(&pool).await?;
        }
        let recorded = dropped
            .as_mut()
            .and_then(|d| d.roles.take())
            .filter(|r| r.roles().same_deployment(&roles));
        // A rebind restores the roles recorded before the reset; provisioning
        // then adds the executor to a record made before it existed.
        if let Some(recorded) = recorded {
            rebind_least_privilege(&pool, recorded)
                .await
                .context("least-privilege provisioning failed")?;
        }
        provision_least_privilege(&pool, &roles)
            .await
            .context("least-privilege provisioning failed")?;
        warn_if_roles_shared(&pool, &roles).await?;
        Some(LeastPrivilegeReport::applied(&roles))
    } else {
        None
    };
    if let (Some(actor), Some(login)) = (&args.root_actor, &args.root_login) {
        if status == "already-initialised" {
            morpholog_postgres::require_current_schema(&pool).await?;
        }
        close(&pool, &Subject::from(actor.as_str()), login, args.confirm).await?;
    } else {
        warn_if_open(&pool).await?;
    }
    // Say whether there was actually a schema to drop.
    if let Some(dropped) = dropped {
        eprintln!(
            "{} `morpholog` schema before provisioning",
            if dropped.existed {
                "dropped the pre-existing"
            } else {
                "found no pre-existing"
            }
        );
    }
    print_json(&InitReport {
        least_privilege,
        schema: "morpholog",
        status,
    })
}

/// Close the deployment under the root, first naming the recorded actors
/// no grant covers: closing locks them out, so it stops unless confirmed.
async fn close(pool: &PgPool, root: &Subject, login: &str, confirm: bool) -> anyhow::Result<()> {
    let ungranted = actors_without_grant(pool, root).await?;
    if !ungranted.is_empty() && !confirm {
        eprintln!(
            "error: closing would refuse {} actor(s) the audit log records, as no \
             ActorAssertionAuthority claim names them:",
            ungranted.len()
        );
        for actor in &ungranted {
            eprintln!("  {actor}");
        }
        eprintln!(
            "Admit their grants first, or re-run with --confirm to close anyway. \
             This lists the actors seen so far, not every actor to come."
        );
        return Err(AlreadyReported.into());
    }
    close_deployment(pool, root, login)
        .await
        .context("closing the deployment failed")?;
    Ok(())
}
