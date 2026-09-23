//! `morpholog propose` - parse and validate a `.morph` file, then propose
//! a named transformation against the database. JSON args in, a required
//! `--actor`, optional `--trace`; committed or rejected JSON out, with the
//! matching exit code. Any system can propose against its own programme
//! without compiling Rust.

use anyhow::Context;
use morpholog_core::{Subject, Transition, explain};
use morpholog_postgres::{
    PgProgram, PgProposalOutcome, PgTracedOutcome, Proposal, propose_against_pg,
    propose_against_pg_with_rejection_state, propose_against_pg_with_trace,
};
use std::io::Write as _;

use crate::ProposeArgs;
use crate::commands::args::{CliArgs, decode_args};
use crate::commands::{
    AlreadyReported, CommitOutcomeUnknown, ParsedSource, compile_or_report, connect,
    lookup_transformation, parse_or_report, print_json,
};
use morpholog_cli::envelopes;

pub(crate) async fn run(args: ProposeArgs) -> anyhow::Result<()> {
    let (parsed, program) = match load(&args.file) {
        Ok(loaded) => loaded,
        Err(failure) if args.batch.is_some() => return report_batch_refusal(failure),
        Err(failure) => return report_request_failure(failure),
    };
    if let Some(batch_path) = &args.batch {
        return run_batch(&args, &program, batch_path).await;
    }
    let (transition, pool) = match prepare(&args, &program).await {
        Ok(prepared) => prepared,
        Err(failure) => return report_request_failure(failure),
    };
    let proposal = Proposal::gateway(&transition);

    // Retrying a `serialization_failure` is the caller's job.
    if args.trace {
        let traced = match propose_against_pg_with_trace(&pool, &program, &proposal).await {
            Ok(traced) => traced,
            Err(err) => return report_request_failure(classify_pg_error(err)),
        };
        match traced {
            PgTracedOutcome::Outcome { outcome, trace } => {
                print_json(&envelopes::Traced {
                    result: &outcome,
                    trace: &trace,
                })?;
                if let PgProposalOutcome::Rejected { reason, .. } = &outcome {
                    return report_rejection(reason, &parsed);
                }
            }
            PgTracedOutcome::KernelErrored { error, trace } => {
                print_json(&envelopes::Traced {
                    result: envelopes::TracedError::new(format!("{error}")),
                    trace: &trace,
                })?;
                return Err(AlreadyReported.into());
            }
        }
    } else if args.explain_on_reject {
        // Explain against the exact state that refused, not a second
        // read that could have moved on.
        let morpholog_postgres::RejectionStateOutcome {
            outcome,
            rejection_state,
        } = match propose_against_pg_with_rejection_state(&pool, &program, &proposal).await {
            Ok(outcome) => outcome,
            Err(err) => return report_request_failure(classify_pg_error(err)),
        };
        match (&outcome, rejection_state) {
            (
                PgProposalOutcome::Rejected {
                    reason,
                    rule,
                    witness,
                },
                Some(state),
            ) => {
                let explanation = explain(program.core().program(), &transition, &state);
                print_json(&envelopes::RejectedWithExplanation::new(
                    reason,
                    rule.as_deref(),
                    witness,
                    explanation,
                ))?;
                return report_rejection(reason, &parsed);
            }
            _ => print_json(&outcome)?,
        }
    } else {
        let outcome = match propose_against_pg(&pool, &program, &proposal).await {
            Ok(outcome) => outcome,
            Err(err) => return report_request_failure(classify_pg_error(err)),
        };
        print_json(&outcome)?;
        if let PgProposalOutcome::Rejected { reason, .. } = &outcome {
            return report_rejection(reason, &parsed);
        }
    }
    Ok(())
}

/// Parse and validate the programme, or say that nothing was committed.
/// Shared by `transact`.
pub(crate) fn load(file: &std::path::Path) -> Result<(ParsedSource, PgProgram), RowError> {
    let parsed = parse_or_report(file).map_err(|e| not_committed(file, e))?;
    let compiled = compile_or_report(&parsed).map_err(|e| not_committed(file, e))?;
    Ok((parsed, PgProgram::new(compiled)))
}

/// Everything a one-shot proposal needs before the adapter call, each
/// failure coded as the same row would be in a batch.
async fn prepare(
    args: &ProposeArgs,
    program: &PgProgram,
) -> Result<(Transition, morpholog_postgres::PgPool), RowError> {
    use envelopes::ProposeCode;
    let compiled = program.core();
    let Some(transformation_name) = args.transformation.as_deref() else {
        return Err(RowError::coded(
            ProposeCode::InvalidRequest,
            anyhow::anyhow!("a transformation name is required outside --batch"),
        ));
    };
    let transformation = lookup_transformation(compiled, transformation_name, &args.file)
        .map_err(|e| RowError::coded(ProposeCode::UnknownTransformation, e))?;
    let codec_input = match (&args.args, &args.args_named) {
        (Some(tagged), None) => CliArgs::Tagged(tagged.as_str()),
        (None, Some(named)) => CliArgs::Named(named.as_str()),
        _ => unreachable!("clap enforces exactly-one-of `--args` and `--args-named`"),
    };
    let eval_args = decode_args(
        &compiled.validated(),
        transformation,
        &args.file,
        codec_input,
    )
    .map_err(|e| RowError::coded(ProposeCode::InvalidArguments, e))?;
    let Some(actor) = args.actor.clone() else {
        return Err(RowError::coded(
            ProposeCode::InvalidRequest,
            anyhow::anyhow!("--actor is required outside --batch"),
        ));
    };
    let pool = connect(&args.db.database_url)
        .await
        .map_err(|e| RowError::coded(ProposeCode::NotCommitted, e))?;
    let transition = Transition {
        transformation_name: transformation.name.clone(),
        args: eval_args,
        actor: Subject::from(actor),
    };
    Ok((transition, pool))
}

/// A decided rejection on a single-proposal path: point stderr at the rule
/// and carry the exit code out. The envelope is already on stdout.
///
/// Returns `Result`, not a bare error, because `Result` is `#[must_use]`:
/// a dropped error would let a rejection exit 0.
fn report_rejection(reason: &str, parsed: &ParsedSource) -> anyhow::Result<()> {
    print_rule_location(reason, parsed);
    Err(AlreadyReported.into())
}

/// On a single-run rejection, point stderr at the rule: resolve the first
/// backticked name in the reason as a declared invariant and print
/// `rule at <file>:<line>:<col> (<name>)`. Prints nothing when the name
/// cannot be placed (a generated invariant, a gate). Stderr only, so
/// stdout envelopes are unchanged; never in batch mode.
fn print_rule_location(reason: &str, parsed: &ParsedSource) {
    let Some(name) = reason.split('`').nth(1) else {
        return;
    };
    let Some(span) = parsed
        .map
        .decl_span(morpholog_surface::DeclKind::Invariant, name)
    else {
        return;
    };
    let (line, col) = morpholog_surface::line_col(&parsed.source, span.start);
    eprintln!("rule at {}:{line}:{col} ({name})", parsed.source_name);
}

/// One NDJSON batch row: a transition naming its own transformation and
/// actor, with args in exactly one codec. Also the body of a session's
/// propose request, which adds a per-request explanation flag.
#[derive(serde::Deserialize)]
pub(crate) struct BatchRow {
    pub(crate) transformation: String,
    pub(crate) actor: String,
    #[serde(default)]
    pub(crate) args: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) args_named: Option<serde_json::Value>,
}

/// A per-row failure: the stable code its receipt carries, or none when
/// the binary cannot say what happened. Every failure before or inside the
/// adapter call is coded; only one after it (a receipt that could not be
/// serialised) is not, and that aborts the batch or session so the caller
/// reads the row as unknown. The reason becomes the receipt's prose.
pub(crate) struct RowError {
    pub(crate) code: Option<envelopes::ProposeCode>,
    pub(crate) reason: anyhow::Error,
}

impl RowError {
    pub(crate) fn coded(code: envelopes::ProposeCode, reason: anyhow::Error) -> Self {
        Self {
            code: Some(code),
            reason,
        }
    }
    /// A failure after the adapter returned: the proposal may have
    /// committed, so it carries no code.
    fn after_decision(reason: anyhow::Error) -> Self {
        Self { code: None, reason }
    }
}

/// Report a one-shot request's failure. A coded one prints its error
/// object on stdout, the positive statement a caller relies on, and the
/// same prose on stderr for a person; it exits 3 for an unknown commit
/// outcome or 1 otherwise. An uncoded one prints nothing on stdout, so the
/// caller reads it as unknown.
pub(crate) fn report_request_failure(failure: RowError) -> anyhow::Result<()> {
    let RowError { code, reason } = failure;
    let Some(code) = code else {
        return Err(reason);
    };
    print_json(&envelopes::RequestError::new(
        code.into(),
        format!("{reason:#}"),
    ))?;
    if code == envelopes::ProposeCode::CommitOutcomeUnknown {
        return Err(CommitOutcomeUnknown(format!("{reason:#}")).into());
    }
    eprintln!("Error: {reason:?}");
    Err(AlreadyReported.into())
}

/// Report a batch refused before its first row: the same object as a
/// one-shot failure, framed as one NDJSON line like a row receipt, so a
/// caller reading the batch line by line can read it.
fn report_batch_refusal(failure: RowError) -> anyhow::Result<()> {
    let RowError { code, reason } = failure;
    let Some(code) = code else {
        return Err(reason);
    };
    let mut out = std::io::stdout().lock();
    writeln!(
        out,
        "{}",
        serde_json::to_string(&envelopes::RequestError::new(
            code.into(),
            format!("{reason:#}"),
        ))?
    )?;
    out.flush()?;
    eprintln!("Error: {reason:?}");
    Err(AlreadyReported.into())
}

/// A setup failure before any proposal was made: nothing was committed.
/// Diagnostics for a programme that did not parse or validate are already
/// on stderr, so the reason points there.
pub(crate) fn not_committed(file: &std::path::Path, err: anyhow::Error) -> RowError {
    let reason = if err.is::<AlreadyReported>() {
        anyhow::anyhow!(
            "{} has errors; its diagnostics are on stderr",
            file.display()
        )
    } else {
        err
    };
    RowError::coded(envelopes::ProposeCode::NotCommitted, reason)
}

/// Classify a proposal-path error into its receipt code. The match is
/// exhaustive and every arm carries a code, so a new adapter error must be
/// given one before it compiles: safe to re-submit, check the record first,
/// or fix the cause.
///
/// `SerializationFailure` is the caller's to retry. A kernel error or a
/// colliding intent is about the row's data. Every other error means
/// nothing was committed, except an unknown commit outcome. A rejection
/// that could not be recorded is `not_committed` too: the verdict was
/// reached, but the rollback came first, so nothing became durable.
pub(crate) fn classify_pg_error(err: morpholog_postgres::PgError) -> RowError {
    use envelopes::ProposeCode;
    use morpholog_postgres::PgError;
    let (code, context): (ProposeCode, &str) = match &err {
        PgError::SerializationFailure => (
            ProposeCode::SerializationFailure,
            "the proposal could not be decided",
        ),
        PgError::Kernel(_) => (
            ProposeCode::KernelError,
            "the proposal could not be decided",
        ),
        PgError::DuplicateIntent => (
            ProposeCode::DuplicateIntent,
            "the proposal could not be decided",
        ),
        PgError::ActorAssertionUnauthorised { .. } => (
            ProposeCode::ActorAssertionUnauthorised,
            "the proposal could not be decided",
        ),
        PgError::CommitOutcomeUnknown(_) => (
            ProposeCode::CommitOutcomeUnknown,
            "the commit outcome is unknown - read the record before re-submitting",
        ),
        PgError::Database(_)
        | PgError::Encoding(_)
        | PgError::InvalidState(_)
        | PgError::SchemaBehind { .. }
        | PgError::TransitionNotFound(_)
        | PgError::TransitionNotCovered { .. }
        | PgError::NoTransitionAtOrBefore(_)
        | PgError::StatVisibility { .. }
        | PgError::WriterRoleUnknown { .. }
        | PgError::WriterAssertionIncomplete { .. }
        | PgError::WriterSessionsHidden { .. }
        | PgError::WriterAssertionEmpty
        | PgError::ActorPolicyDeclaration { .. }
        | PgError::UnknownTransformation { .. }
        | PgError::NoCheckpoint
        | PgError::AuditPrefixIncomplete { .. }
        | PgError::AnchorDivergedFromStart { .. }
        | PgError::SigningKeyUnauthorised { .. }
        | PgError::SigningKeyUnauthorisedAtTruncatedPrefix { .. } => {
            (ProposeCode::NotCommitted, "the proposal was not committed")
        }
        PgError::RejectionLogFailure(_) => (
            ProposeCode::NotCommitted,
            "the proposal was rejected, but the rejection could not be recorded; \
             nothing was committed",
        ),
    };
    RowError::coded(code, anyhow::Error::new(err).context(context))
}

/// Batch mode: one receipt per row, in row order, each row its own
/// SERIALIZABLE commit. Not all-or-nothing. A failing row gets an error
/// receipt and the batch goes on; a rejection is a normal outcome. Each
/// receipt is flushed before the next row runs, so a caller whose batch is
/// killed has every receipt for the rows that finished. A failure before
/// the first row prints one error object with no `row`: nothing was
/// attempted. Exits zero once every row is processed; non-zero only when
/// the binary can no longer say what a row did (see [`RowError`]). `row`
/// is the 1-based input line; blank lines are skipped.
async fn run_batch(
    args: &ProposeArgs,
    program: &PgProgram,
    batch_path: &std::path::Path,
) -> anyhow::Result<()> {
    let input = if batch_path == std::path::Path::new("-") {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .context("failed to read batch rows from stdin")
            .map(|_| buf)
    } else {
        std::fs::read_to_string(batch_path)
            .with_context(|| format!("failed to read batch rows from {}", batch_path.display()))
    };
    let input = match input {
        Ok(input) => input,
        Err(e) => return report_batch_refusal(not_committed(&args.file, e)),
    };
    let pool = match connect(&args.db.database_url).await {
        Ok(pool) => pool,
        Err(e) => return report_batch_refusal(not_committed(&args.file, e)),
    };
    let mut out = std::io::stdout().lock();
    let (mut committed, mut rejected, mut errored, mut rows) = (0u64, 0u64, 0u64, 0u64);

    for (line_no, line) in input.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        rows += 1;
        let row = line_no + 1;
        let receipt = match batch_row_outcome(args, program, &pool, line).await {
            Ok(mut envelope) => {
                match envelope.get("status").and_then(|s| s.as_str()) {
                    Some("committed") => committed += 1,
                    Some("rejected") => rejected += 1,
                    _ => {}
                }
                if let Some(obj) = envelope.as_object_mut() {
                    obj.insert("row".to_string(), serde_json::json!(row));
                }
                envelope
            }
            // The binary cannot say what this row did: abort with no
            // receipt for it, so the caller reads it as unknown.
            Err(RowError { code: None, reason }) => {
                eprintln!(
                    "batch aborted at row {row}: {committed} committed, \
                     {rejected} rejected, {errored} errors before the failure"
                );
                return Err(reason.context(format!("operational failure at row {row}")));
            }
            // A row failure is a receipt; later rows still run.
            Err(RowError {
                code: Some(code),
                reason,
            }) => {
                errored += 1;
                serde_json::to_value(envelopes::ErrorReceipt::new(
                    code.into(),
                    format!("{reason:#}"),
                    row as u64,
                ))?
            }
        };
        writeln!(out, "{}", serde_json::to_string(&receipt)?)?;
        out.flush()?;
    }

    eprintln!("batch: {rows} rows - {committed} committed, {rejected} rejected, {errored} errors");
    Ok(())
}

/// Parse one row and hand it to [`propose_row_outcome`], which the session
/// shares. Returns the single-run envelope without `row`.
async fn batch_row_outcome(
    args: &ProposeArgs,
    program: &PgProgram,
    pool: &morpholog_postgres::PgPool,
    line: &str,
) -> Result<serde_json::Value, RowError> {
    let row: BatchRow = serde_json::from_str(line)
        .context("malformed batch row")
        .map_err(|e| RowError::coded(envelopes::ProposeCode::InvalidRequest, e))?;
    propose_row_outcome(&args.file, args.explain_on_reject, program, pool, row).await
}

/// Turn a batch row into its transition: look up the transformation and
/// decode the arguments. Shared by batch, session and `transact`, so all
/// refuse a malformed row with the same code.
pub(crate) fn decode_row(
    file: &std::path::Path,
    compiled: &morpholog_core::CompiledProgram,
    row: BatchRow,
) -> Result<Transition, RowError> {
    let transformation = lookup_transformation(compiled, &row.transformation, file)
        .map_err(|e| RowError::coded(envelopes::ProposeCode::UnknownTransformation, e))?;
    let (tagged, named);
    let codec_input = match (&row.args, &row.args_named) {
        (Some(t), None) => {
            tagged = t.to_string();
            CliArgs::Tagged(&tagged)
        }
        (None, Some(n)) => {
            named = n.to_string();
            CliArgs::Named(&named)
        }
        _ => {
            return Err(RowError::coded(
                envelopes::ProposeCode::InvalidRequest,
                anyhow::anyhow!("a batch row carries exactly one of `args` and `args_named`"),
            ));
        }
    };
    let eval_args = decode_args(&compiled.validated(), transformation, file, codec_input)
        .map_err(|e| RowError::coded(envelopes::ProposeCode::InvalidArguments, e))?;
    Ok(Transition {
        transformation_name: transformation.name.clone(),
        args: eval_args,
        actor: Subject::from(row.actor),
    })
}

/// Propose one transition and return its single-run envelope, without
/// `row`. It uses the same codecs, calls and JSON shapes as the single-run
/// path, so receipts cannot drift from it. Shared by batch and session.
pub(crate) async fn propose_row_outcome(
    file: &std::path::Path,
    explain_on_reject: bool,
    program: &PgProgram,
    pool: &morpholog_postgres::PgPool,
    row: BatchRow,
) -> Result<serde_json::Value, RowError> {
    let compiled = program.core();
    let transition = decode_row(file, compiled, row)?;
    if explain_on_reject {
        let morpholog_postgres::RejectionStateOutcome {
            outcome,
            rejection_state,
        } = propose_against_pg_with_rejection_state(pool, program, &Proposal::gateway(&transition))
            .await
            .map_err(classify_pg_error)?;
        if let (
            PgProposalOutcome::Rejected {
                reason,
                rule,
                witness,
            },
            Some(state),
        ) = (&outcome, rejection_state)
        {
            let explanation = explain(compiled.program(), &transition, &state);
            return serde_json::to_value(envelopes::RejectedWithExplanation::new(
                reason,
                rule.as_deref(),
                witness,
                explanation,
            ))
            .context("serialising the receipt")
            .map_err(RowError::after_decision);
        }
        serde_json::to_value(&outcome)
            .context("serialising the receipt")
            .map_err(RowError::after_decision)
    } else {
        let outcome = propose_against_pg(pool, program, &Proposal::gateway(&transition))
            .await
            .map_err(classify_pg_error)?;
        serde_json::to_value(&outcome)
            .context("serialising the receipt")
            .map_err(RowError::after_decision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use envelopes::ProposeCode;
    use morpholog_postgres::PgError;

    /// The standings a row can be left in, by adapter error. Every adapter
    /// error is coded: only the binary saying so lets a caller treat a
    /// proposal as not committed.
    #[test]
    fn every_adapter_error_is_coded_and_only_one_is_unknown() {
        let not_committed = classify_pg_error(PgError::Database(sqlx::Error::PoolClosed));
        assert_eq!(not_committed.code, Some(ProposeCode::NotCommitted));
        let unknown = classify_pg_error(PgError::CommitOutcomeUnknown(sqlx::Error::PoolClosed));
        assert_eq!(unknown.code, Some(ProposeCode::CommitOutcomeUnknown));
        assert!(format!("{:#}", unknown.reason).contains("read the record"));
        let unrecorded = classify_pg_error(PgError::RejectionLogFailure(Box::new(
            PgError::Database(sqlx::Error::PoolClosed),
        )));
        assert_eq!(
            unrecorded.code,
            Some(ProposeCode::NotCommitted),
            "the rollback came before the failed record, so nothing is durable"
        );
        assert!(format!("{:#}", unrecorded.reason).contains("rejected"));
    }
}
