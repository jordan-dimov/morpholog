//! `morpholog propose` - parse and validate a `.morph` source file, then
//! propose a named transformation against a Morpholog PostgreSQL
//! database. The CLI's commit path: JSON-encoded args, a required
//! `--actor`, optional `--trace`, committed/rejected JSON output and the
//! matching exit code.
//!
//! This closes the input boundary of the compute/commit/outbox split:
//! an external system proposes against its own `.morph` programme by
//! path, without forking the CLI or compiling Rust.

use anyhow::Context;
use morpholog_core::{Subject, Transition, explain};
use morpholog_postgres::{
    PgProposalOutcome, PgTracedOutcome, Proposal, propose_against_pg,
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
    // 1. Parse the source file. Exits on parse failure with rendered
    //    diagnostics (same path `check` and `parse` use).
    let parsed = parse_or_report(&args.file)?;

    // 2. Validate. Same error shape as `check`; returns a reported
    //    failure so a malformed programme never reaches the proposal
    //    path. The returned `ValidatedProgram` handle
    //    threads through to the codec so it does not re-validate.
    let compiled = compile_or_report(&parsed)?;

    if let Some(batch_path) = &args.batch {
        return run_batch(&args, &compiled, batch_path).await;
    }

    // 3. Resolve the transformation. Clap guarantees it is present
    //    outside batch mode.
    let Some(transformation_name) = args.transformation.as_deref() else {
        // Clap's required_unless_present("batch") makes this
        // unreachable; the bail keeps the invariant honest without a
        // panic path in the binary.
        anyhow::bail!("a transformation name is required outside --batch");
    };
    let transformation = lookup_transformation(&compiled, transformation_name, &args.file)?;

    // 4. Decode --args or --args-named into `Vec<EvalValue>`. Clap
    //    has already enforced exactly-one-of via `conflicts_with` +
    //    `required_unless_present`, so `unwrap_either` would be
    //    safe; the explicit match keeps the intent clear and gives
    //    the codec a typed handle.
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

    // 5. Connect and propose. Same retry caveat as `propose`:
    //    `PgError::SerializationFailure` is the caller's to retry.
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
            propose_against_pg_with_trace(&pool, &compiled, &Proposal::gateway(&transition))
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
        // Same-snapshot diagnosis: the variant hands back the exact
        // pre-state the gates evaluated, and the explanation engine
        // (pure, in-memory) runs against it - never a second read
        // that could describe different state than the one that
        // refused.
        let morpholog_postgres::RejectionStateOutcome {
            outcome,
            rejection_state,
            ..
        } = propose_against_pg_with_rejection_state(
            &pool,
            &compiled,
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
        let outcome = propose_against_pg(&pool, &compiled, &Proposal::gateway(&transition))
            .await
            .map_err(one_shot_failure)?;
        print_json(&outcome)?;
        if let PgProposalOutcome::Rejected { reason, .. } = &outcome {
            return report_rejection(reason, &parsed);
        }
    }
    Ok(())
}

/// A decided rejection on a single-proposal path: locate the rule on
/// stderr and carry the exit code out (the envelope is already on
/// stdout, so nothing further is printed).
///
/// Returns `Result` rather than a bare `anyhow::Error` so the compiler
/// enforces what a caller must do with it. An earlier draft of this
/// refactor handed back the error, a caller built it and dropped it,
/// and a decided refusal started exiting 0 - reported as success.
/// `Result` is `#[must_use]`; a bare error is not.
fn report_rejection(reason: &str, parsed: &ParsedSource) -> anyhow::Result<()> {
    print_rule_location(reason, parsed);
    Err(AlreadyReported.into())
}

/// On a single-run rejection, point stderr at the rule: take the
/// first backticked name in the reason, resolve it as an invariant
/// declared in the source, and print `rule at <file>:<line>:<col>
/// (<name>)`. A name the map cannot place (a generated discipline
/// invariant, a gate refusal that names no invariant) prints
/// nothing. Stderr only - every stdout envelope stays byte-identical
/// - and single-run only: batch receipts are the machine contract.
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

/// One NDJSON batch row: a self-contained transition naming its own
/// transformation and actor, with args in either codec (exactly one).
/// Also the propose body of a session request, which is the same
/// self-contained shape plus a per-request explanation flag.
#[derive(serde::Deserialize)]
pub(crate) struct BatchRow {
    pub(crate) transformation: String,
    pub(crate) actor: String,
    #[serde(default)]
    pub(crate) args: Option<serde_json::Value>,
    #[serde(default)]
    pub(crate) args_named: Option<serde_json::Value>,
}

/// A per-row failure: the stable code its receipt carries, or none for
/// an operational failure - a dead connection, a schema mismatch - which
/// aborts the batch or the session and never becomes a receipt. The
/// reason renders into the receipt's human prose.
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

/// A one-shot proposal's adapter failure, worded for what the adapter
/// knows: every error but one means the proposal was not committed;
/// the one is a commit whose outcome could not be proven, which exits
/// on its own code so a caller need not parse this prose.
fn one_shot_failure(err: morpholog_postgres::PgError) -> anyhow::Error {
    use morpholog_postgres::PgError;
    match err {
        PgError::CommitOutcomeUnknown(inner) => CommitOutcomeUnknown(inner.to_string()).into(),
        PgError::RejectionLogFailure(_) => anyhow::Error::new(err),
        other => anyhow::Error::new(other).context("the proposal was not committed"),
    }
}

/// Classify a proposal-path error into its receipt code. Exhaustive on
/// purpose: a new adapter error must be placed here - safe to
/// re-submit, must inspect first, or operational - before it compiles.
/// `SerializationFailure` is the documented per-row outcome (the caller
/// re-submits that row; retries stay the caller's); a kernel error or
/// colliding intent is that row's data speaking; every other adapter
/// error on this path is a known non-commit, except the commit whose
/// outcome the adapter could not prove, and a decided rejection whose
/// record could not be written - which is operational, because the
/// verdict was reached and a pre-decision code would misdescribe it.
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
/// SERIALIZABLE commit - an import is explicitly NOT all-or-nothing.
/// A malformed row (bad JSON, unknown transformation, undecodable
/// args) gets an error receipt and processing continues; rejections
/// are lawful outcomes. The exit code is zero whenever every row was
/// processed; non-zero is reserved for operational failure (unreadable
/// input, a broken connection - see [`RowError`]). `row` is the
/// 1-based line number in the input; blank lines are skipped without
/// receipts.
async fn run_batch(
    args: &ProposeArgs,
    compiled: &morpholog_core::CompiledProgram,
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
        let receipt = match batch_row_outcome(args, compiled, &pool, line).await {
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
            // Infrastructure failure aborts: the summary names how far
            // the batch got, and the exit code tells the truth.
            Err(RowError { code: None, reason }) => {
                eprintln!(
                    "batch aborted at row {row}: {committed} committed, \
                     {rejected} rejected, {errored} errors before the failure"
                );
                return Err(reason.context(format!("operational failure at row {row}")));
            }
            // A row-level failure is a receipt, never a process
            // failure: the rows after it still run.
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

/// Process one row to its single-run envelope (without the `row`
/// field). Thin parse step over [`propose_row_outcome`], which the
/// session shares.
async fn batch_row_outcome(
    args: &ProposeArgs,
    compiled: &morpholog_core::CompiledProgram,
    pool: &morpholog_postgres::PgPool,
    line: &str,
) -> Result<serde_json::Value, RowError> {
    let row: BatchRow = serde_json::from_str(line)
        .context("malformed batch row")
        .map_err(|e| RowError::coded(envelopes::ProposeCode::InvalidRequest, e))?;
    propose_row_outcome(&args.file, args.explain_on_reject, compiled, pool, row).await
}

/// A batch row to the kernel transition it names: the transformation
/// looked up, the arguments decoded by the row's codec. Shared by the
/// batch, the session, and `transact`, so every row-shaped surface
/// refuses a malformed row with the same code.
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

/// One self-contained transition to its single-run envelope (without
/// the `row` field): the same codecs, the same propose calls, the
/// same JSON shapes as the non-batch path, so the receipt contract
/// cannot drift from the pinned single-run contract. Shared by the
/// batch (whose explanation flag is batch-wide) and the session
/// (whose flag is per request).
pub(crate) async fn propose_row_outcome(
    file: &std::path::Path,
    explain_on_reject: bool,
    compiled: &morpholog_core::CompiledProgram,
    pool: &morpholog_postgres::PgPool,
    row: BatchRow,
) -> Result<serde_json::Value, RowError> {
    let transition = decode_row(file, compiled, row)?;
    if explain_on_reject {
        let morpholog_postgres::RejectionStateOutcome {
            outcome,
            rejection_state,
            ..
        } = propose_against_pg_with_rejection_state(
            pool,
            compiled,
            &Proposal::gateway(&transition),
        )
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
        let outcome = propose_against_pg(pool, compiled, &Proposal::gateway(&transition))
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
