//! `morpholog roles move` - give a deployment a pair of roles of its own.

use crate::RolesMoveArgs;
use crate::commands::{connect_unchecked, print_json};
use morpholog_postgres::{DeploymentRoles, move_deployment_roles};

pub(crate) async fn run_move(args: RolesMoveArgs) -> anyhow::Result<()> {
    let to = DeploymentRoles::with_prefix(&args.role_prefix)?;
    let pool = connect_unchecked(&args.db.database_url).await?;
    let report = move_deployment_roles(&pool, &to, &args.login).await?;
    print_json(&report)
}
