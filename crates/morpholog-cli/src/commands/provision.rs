//! `morpholog provision indexes` - reconcile the indexes the programmes'
//! loads and compiled invariants can use. The compiler says what is needed
//! and the adapter reconciles; this reports the plan it acted on and exits
//! non-zero when a conflict needs an operator.

use anyhow::bail;
use morpholog_postgres::{
    IndexAction, PgProgram, ProvisionReport, StatisticsAction, check_named_programs, plan_indexes,
    provision_indexes,
};

use crate::ProvisionIndexesArgs;
use crate::commands::{compile_or_report, connect, parse_or_report, print_json};
use morpholog_cli::envelopes;

pub(crate) async fn indexes(args: ProvisionIndexesArgs) -> anyhow::Result<()> {
    let mut programs = Vec::with_capacity(args.files.len());
    for file in &args.files {
        let parsed = parse_or_report(file)?;
        programs.push(PgProgram::new(compile_or_report(&parsed)?));
    }
    let programs: Vec<&PgProgram> = programs.iter().collect();
    // A usage error should not need a database to be reported.
    check_named_programs(&programs)?;
    let pool = connect(&args.db.database_url).await?;
    let report = if args.dry_run {
        plan_indexes(&pool, &programs, args.prune).await?
    } else {
        provision_indexes(&pool, &programs, args.prune).await?
    };
    if args.json {
        print_json(&envelopes::ProvisionReport::from(&report))?;
    } else {
        print_plan(&report);
    }
    if report.has_conflict() {
        let names: Vec<&str> = report
            .entries
            .iter()
            .filter(|e| e.action == IndexAction::Conflict)
            .map(|e| e.index_name.as_str())
            .chain(
                report
                    .statistics
                    .iter()
                    .filter(|s| s.action == StatisticsAction::Conflict)
                    .map(|s| s.statistics_name.as_str()),
            )
            .collect();
        bail!(
            "an index or statistics object under Morpholog's name has another definition and was left alone: {}",
            names.join(", ")
        );
    }
    Ok(())
}

/// The plan for a person. Not a machine surface: `--json` is.
fn print_plan(report: &ProvisionReport) {
    for program in &report.programs {
        println!("program: {} ({})", program.identity, program.hash);
    }
    if report.entries.is_empty() {
        println!("no compiled invariants: nothing to provision");
    }
    let detail = |detail: &str| {
        if detail.is_empty() {
            String::new()
        } else {
            format!("  - {detail}")
        }
    };
    for entry in &report.entries {
        println!(
            "{:<20} {}  {}[{}] {}{}",
            entry.action.to_string(),
            entry.index_name,
            entry.predicate,
            entry.position,
            entry.representation,
            detail(&entry.detail)
        );
    }
    for entry in &report.required_elsewhere {
        println!(
            "{:<20} {}  - required by {}",
            "REQUIRED ELSEWHERE",
            entry.index_name,
            entry.required_by.join(", ")
        );
    }
    for entry in &report.statistics {
        println!(
            "{:<20} {}  statistics[{}]{}{}",
            entry.action.to_string(),
            entry.statistics_name,
            entry.position,
            detail(&entry.detail),
            if entry.required_by.is_empty() {
                String::new()
            } else {
                format!("  - required by {}", entry.required_by.join(", "))
            }
        );
    }
    if !report.positions_unknown_for.is_empty() {
        println!(
            "positions unknown for: {}  - provision these programmes again; no statistics object is stale until then",
            report.positions_unknown_for.join(", ")
        );
    }
    println!(
        "{}",
        match (report.applied, report.dry_run, report.has_conflict()) {
            (true, _, _) => "applied; morpholog.claims analyzed",
            (false, true, _) => "dry run: nothing changed",
            (false, false, true) => "not applied: a conflict needs an operator first",
            (false, false, false) => "not applied",
        }
    );
}
