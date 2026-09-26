//! `morpholog provision indexes` - reconcile the indexes a programme's
//! compiled invariants can use. The compiler says what is needed and the
//! adapter reconciles; this prints the plan it acted on and exits non-zero
//! when a conflict needs an operator.

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
        match (report.applied, args.dry_run, report.has_conflict()) {
            (true, _, _) => "applied; morpholog.claims analyzed",
            (false, true, _) => "dry run: nothing changed",
            (false, false, true) => "not applied: a conflict needs an operator first",
            (false, false, false) => "not applied",
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
