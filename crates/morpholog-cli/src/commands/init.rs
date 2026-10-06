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

use anyhow::{Context, anyhow};
use morpholog_postgres::{
    DeploymentRoles, InitOutcome, deployment_roles, drop_schema, initialise_schema,
    provision_least_privilege, rebind_least_privilege, redact_database_url,
    require_deployment_roles,
};

use crate::InitArgs;
use crate::commands::{AlreadyReported, connect_unchecked, print_json, warn_if_roles_shared};
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
    let mut dropped = if args.reset {
        // A reset binds the roles this database recorded again, so another
        // prefix is refused while there is still something to keep.
        if args.least_privilege
            && let Some(recorded) = deployment_roles(&pool).await?
            && recorded != roles
        {
            return Err(anyhow!(
                "this database records `{}` and `{}` as its least-privilege roles; \
                 --reset --least-privilege binds them again, so pass their prefix, or \
                 move the deployment to new roles first (docs/install.md, \"Several \
                 deployments on one cluster\"). Nothing was dropped",
                recorded.writer(),
                recorded.reader()
            ));
        }
        // A recorded role that is gone stays a refusal, decided while the
        // record that names it still exists.
        if args.least_privilege {
            require_deployment_roles(&pool)
                .await
                .context("nothing was dropped")?;
        }
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
            .filter(|r| *r.roles() == roles);
        match recorded {
            Some(recorded) => rebind_least_privilege(&pool, recorded).await,
            None => provision_least_privilege(&pool, &roles).await,
        }
        .context("least-privilege provisioning failed")?;
        warn_if_roles_shared(&pool, &roles).await?;
        Some(LeastPrivilegeReport::applied(&roles))
    } else {
        None
    };
    // Say whether there was actually a schema to drop.
    if let Some(dropped) = dropped {
        eprintln!(
            "{} the pre-existing `morpholog` schema before provisioning",
            if dropped.existed {
                "dropped"
            } else {
                "found no"
            }
        );
    }
    print_json(&InitReport {
        least_privilege,
        schema: "morpholog",
        status,
    })
}
