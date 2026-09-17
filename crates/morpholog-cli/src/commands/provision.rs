//! `morpholog provision indexes` - reconcile the indexes a programme's
//! compiled invariants can seek on. Thin over the adapter: the compiler
//! says what is required, the adapter reconciles, this prints the plan
//! in the same words the executor acted on and exits non-zero when a
//! conflict needs an operator.

use anyhow::bail;
use morpholog_postgres::{IndexAction, PgProgram, plan_indexes, provision_indexes};

use crate::ProvisionIndexesArgs;
use crate::commands::{compile_or_report, connect, parse_or_report};

pub(crate) async fn indexes(args: ProvisionIndexesArgs) -> anyhow::Result<()> {
    let parsed = parse_or_report(&args.file)?;
    let program = PgProgram::new(compile_or_report(&parsed)?);
    let pool = connect(&args.db.database_url).await?;
    let report = if args.dry_run {
        plan_indexes(&pool, &program).await?
    } else {
        provision_indexes(&pool, &program, args.prune).await?
    };
    println!(
        "program: {} ({})",
        report.program_identity, report.program_hash
    );
    if report.entries.is_empty() {
        println!("no compiled invariants: nothing to provision");
    }
    for entry in &report.entries {
        let detail = if entry.detail.is_empty() {
            String::new()
        } else {
            format!("  - {}", entry.detail)
        };
        println!(
            "{:<20} {}  {}[{}] {}{detail}",
            entry.action.to_string(),
            entry.index_name,
            entry.predicate,
            entry.position,
            entry.representation
        );
    }
    println!(
        "{}",
        if report.applied {
            "applied"
        } else {
            "dry run: nothing changed"
        }
    );
    if report.has_conflict() {
        let names: Vec<&str> = report
            .entries
            .iter()
            .filter(|e| e.action == IndexAction::Conflict)
            .map(|e| e.index_name.as_str())
            .collect();
        bail!(
            "an index under Morpholog's name has another definition and was left alone: {}",
            names.join(", ")
        );
    }
    Ok(())
}
