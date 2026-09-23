//! `morpholog transact` - several proposals as one decision. The act
//! decoding here is shared with the session's `transact` op.

use anyhow::Context;
use morpholog_postgres::{PgAtomicOutcome, Proposal, propose_all_against_pg};

use crate::TransactArgs;
use crate::commands::propose::{BatchRow, RowError, classify_pg_error, decode_row};
use crate::commands::{
    AlreadyReported, CommitOutcomeUnknown, compile_or_report, connect, parse_or_report, print_json,
};
use morpholog_cli::envelopes;

/// One act: the batch row shape, but strict. A misspelt field refuses the
/// whole batch rather than being ignored.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Act {
    transformation: String,
    actor: String,
    #[serde(default)]
    args: Option<serde_json::Value>,
    #[serde(default)]
    args_named: Option<serde_json::Value>,
}

impl From<Act> for BatchRow {
    fn from(act: Act) -> Self {
        BatchRow {
            transformation: act.transformation,
            actor: act.actor,
            args: act.args,
            args_named: act.args_named,
        }
    }
}

/// Decode every act, or name the first that fails by its 1-based position
/// (the numbering refusals use). A malformed act makes the whole request
/// invalid; it never reaches the database.
pub(crate) fn decode_acts(
    file: &std::path::Path,
    compiled: &morpholog_core::CompiledProgram,
    acts: Vec<Act>,
) -> Result<Vec<Proposal>, RowError> {
    if acts.is_empty() {
        return Err(RowError::coded(
            envelopes::ProposeCode::InvalidRequest,
            anyhow::anyhow!("an atomic batch needs at least one act"),
        ));
    }
    acts.into_iter()
        .enumerate()
        .map(|(index, act)| {
            decode_row(file, compiled, act.into())
                .map(|t| Proposal::gateway(&t))
                .map_err(|e| RowError {
                    code: e.code,
                    reason: e.reason.context(format!("act {}", index + 1)),
                })
        })
        .collect()
}

/// Run `transact`: decode every act, propose them as one decision, and
/// print one outcome object. Exit codes follow `propose`: 0 committed,
/// 1 refused or a known error, 3 when the commit outcome is unknown.
pub(crate) async fn run(args: TransactArgs) -> anyhow::Result<()> {
    let parsed = parse_or_report(&args.file)?;
    let program = morpholog_postgres::PgProgram::new(compile_or_report(&parsed)?);
    let compiled = program.core();
    let input = if args.acts == std::path::Path::new("-") {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .context("failed to read acts from stdin")?;
        buf
    } else {
        std::fs::read_to_string(&args.acts)
            .with_context(|| format!("failed to read acts from {}", args.acts.display()))?
    };
    // Blank lines are skipped, as in a batch. An act is numbered by its
    // position among the acts, as refusals are; the file line is kept too.
    let rows: Result<Vec<Act>, RowError> = input
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .enumerate()
        .map(|(position, (line_index, line))| {
            serde_json::from_str(line)
                .with_context(|| {
                    format!("malformed act {} (line {})", position + 1, line_index + 1)
                })
                .map_err(|e| RowError::coded(envelopes::ProposeCode::InvalidRequest, e))
        })
        .collect();
    let proposals = match rows.and_then(|rows| decode_acts(&args.file, compiled, rows)) {
        Ok(proposals) => proposals,
        Err(failure) => return report_failure(failure),
    };

    let pool = connect(&args.db.database_url).await?;
    match propose_all_against_pg(&pool, &program, &proposals).await {
        Ok(outcome) => {
            print_json(&outcome)?;
            match outcome {
                PgAtomicOutcome::Committed { .. } => Ok(()),
                PgAtomicOutcome::Rejected { .. } => Err(AlreadyReported.into()),
            }
        }
        Err(err) => report_failure(classify_pg_error(err)),
    }
}

/// A coded failure prints the error object and exits by its code. An
/// operational one (a rejection the log could not record) prints only a
/// diagnostic, with nothing on stdout.
fn report_failure(failure: RowError) -> anyhow::Result<()> {
    let RowError { code, reason } = failure;
    let Some(code) = code else {
        return Err(reason);
    };
    print_json(&envelopes::AtomicError::new(
        code.into(),
        format!("{reason:#}"),
    ))?;
    if code == envelopes::ProposeCode::CommitOutcomeUnknown {
        return Err(CommitOutcomeUnknown(format!("{reason:#}")).into());
    }
    Err(AlreadyReported.into())
}
