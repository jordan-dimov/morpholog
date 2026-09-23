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

use crate::ProposeArgs;
use crate::commands::args::{CliArgs, decode_args};
use crate::commands::{
    AlreadyReported, CommitOutcomeUnknown, ParsedSource, compile_or_report, connect,
    lookup_transformation, parse_or_report, print_json,
};
use morpholog_cli::envelopes;

pub(crate) async fn run(args: ProposeArgs) -> anyhow::Result<()> {
    let parsed = parse_or_report(&args.file)?;
    let program = PgProgram::new(compile_or_report(&parsed)?);
    let compiled = program.core();

    if let Some(batch_path) = &args.batch {
        return run_batch(&args, &program, batch_path).await;
    }

    let Some(transformation_name) = args.transformation.as_deref() else {
        // Clap requires it outside batch mode; bail rather than panic.
        anyhow::bail!("a transformation name is required outside --batch");
    };
    let transformation = lookup_transformation(compiled, transformation_name, &args.file)?;

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
    )?;

    // Retrying a `PgError::SerializationFailure` is the caller's job.
    let pool = connect(&args.db.database_url).await?;
    let transition = Transition {
        transformation_name: transformation.name.clone(),
        args: eval_args,
        actor: match args.actor.clone() {
            Some(actor) => Subject::from(actor),
            None => anyhow::bail!("--actor is required outside --batch"),
        },
    };

    if args.trace {
        let traced =
            propose_against_pg_with_trace(&pool, &program, &Proposal::gateway(&transition))
                .await
                .map_err(one_shot_failure)?;
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
        } = propose_against_pg_with_rejection_state(
            &pool,
            &program,
            &Proposal::gateway(&transition),
        )
        .await
        .map_err(one_shot_failure)?;
        match (&outcome, rejection_state) {
            (
                PgProposalOutcome::Rejected {
                    reason,
                    rule,
                    witness,
                },
                Some(state),
            ) => {
                let explanation = explain(compiled.program(), &transition, &state);
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
        let outcome = propose_against_pg(&pool, &program, &Proposal::gateway(&transition))
            .await
            .map_err(one_shot_failure)?;
        print_json(&outcome)?;
        if let PgProposalOutcome::Rejected { reason, .. } = &outcome {
            return report_rejection(reason, &parsed);
        }
    }
    Ok(())
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

/// A per-row failure: the stable code its receipt carries, or none for an
/// operational failure (a dead connection, a schema mismatch), which aborts
/// the batch or session instead. The reason becomes the receipt's prose.
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
    fn operational(reason: anyhow::Error) -> Self {
        Self { code: None, reason }
    }
}

/// Word a one-shot proposal's adapter failure. Every error but one means
/// nothing was committed. The exception, a commit whose outcome is
/// unknown, gets its own exit code.
fn one_shot_failure(err: morpholog_postgres::PgError) -> anyhow::Error {
    use morpholog_postgres::PgError;
    match err {
        PgError::CommitOutcomeUnknown(inner) => CommitOutcomeUnknown(inner.to_string()).into(),
        PgError::RejectionLogFailure(_) => anyhow::Error::new(err),
        other => anyhow::Error::new(other).context("the proposal was not committed"),
    }
}

/// Classify a proposal-path error into its receipt code. The match is
/// exhaustive, so a new adapter error must be placed before it compiles:
/// safe to re-submit, check the record first, or operational.
///
/// `SerializationFailure` is the caller's to retry. A kernel error or a
/// colliding intent is about the row's data. Most other errors mean nothing
/// was committed. Two are exceptions: an unknown commit outcome, and a
/// rejection that could not be recorded. The latter is operational, since
/// the verdict was reached and a "not decided" code would be wrong.
pub(crate) fn classify_pg_error(err: morpholog_postgres::PgError) -> RowError {
    use envelopes::ProposeCode;
    use morpholog_postgres::PgError;
    let (code, context) = match &err {
        PgError::SerializationFailure => (
            Some(ProposeCode::SerializationFailure),
            "the proposal could not be decided",
        ),
        PgError::Kernel(_) => (
            Some(ProposeCode::KernelError),
            "the proposal could not be decided",
        ),
        PgError::DuplicateIntent => (
            Some(ProposeCode::DuplicateIntent),
            "the proposal could not be decided",
        ),
        PgError::ActorAssertionUnauthorised { .. } => (
            Some(ProposeCode::ActorAssertionUnauthorised),
            "the proposal could not be decided",
        ),
        PgError::CommitOutcomeUnknown(_) => (
            Some(ProposeCode::CommitOutcomeUnknown),
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
        | PgError::SigningKeyUnauthorisedAtTruncatedPrefix { .. } => (
            Some(ProposeCode::NotCommitted),
            "the proposal was not committed",
        ),
        PgError::RejectionLogFailure(_) => (None, "the rejection could not be recorded"),
    };
    RowError {
        code,
        reason: anyhow::Error::new(err).context(context),
    }
}

/// Batch mode: one receipt per row, in row order, each row its own
/// SERIALIZABLE commit. Not all-or-nothing. A malformed row gets an error
/// receipt and the batch goes on; a rejection is a normal outcome. Exits
/// zero once every row is processed; non-zero only on operational failure
/// (see [`RowError`]). `row` is the 1-based input line; blank lines are
/// skipped.
async fn run_batch(
    args: &ProposeArgs,
    program: &PgProgram,
    batch_path: &std::path::Path,
) -> anyhow::Result<()> {
    let input = if batch_path == std::path::Path::new("-") {
        let mut buf = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
            .context("failed to read batch rows from stdin")?;
        buf
    } else {
        std::fs::read_to_string(batch_path)
            .with_context(|| format!("failed to read batch rows from {}", batch_path.display()))?
    };

    let pool = connect(&args.db.database_url).await?;
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
            // An operational failure aborts, saying how far the batch got.
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
        println!("{}", serde_json::to_string(&receipt)?);
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
            .map_err(RowError::operational);
        }
        serde_json::to_value(&outcome)
            .context("serialising the receipt")
            .map_err(RowError::operational)
    } else {
        let outcome = propose_against_pg(pool, program, &Proposal::gateway(&transition))
            .await
            .map_err(classify_pg_error)?;
        serde_json::to_value(&outcome)
            .context("serialising the receipt")
            .map_err(RowError::operational)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use envelopes::ProposeCode;
    use morpholog_postgres::PgError;

    /// The three standings a row can be left in, by adapter error.
    #[test]
    fn adapter_errors_map_to_safe_to_retry_inspect_first_or_operational() {
        let not_committed = classify_pg_error(PgError::Database(sqlx::Error::PoolClosed));
        assert_eq!(not_committed.code, Some(ProposeCode::NotCommitted));
        let unknown = classify_pg_error(PgError::CommitOutcomeUnknown(sqlx::Error::PoolClosed));
        assert_eq!(unknown.code, Some(ProposeCode::CommitOutcomeUnknown));
        assert!(format!("{:#}", unknown.reason).contains("read the record"));
        let unrecorded = classify_pg_error(PgError::RejectionLogFailure(Box::new(
            PgError::Database(sqlx::Error::PoolClosed),
        )));
        assert_eq!(
            unrecorded.code, None,
            "decided, so never a pre-decision code"
        );
    }
}
