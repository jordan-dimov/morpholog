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
    AlreadyReported, ParsedSource, compile_or_report, connect, lookup_transformation,
    parse_or_report, print_json,
};
use morpholog_cli::envelopes;

pub(crate) async fn run(args: ProposeArgs) -> anyhow::Result<()> {
    // 1. Parse the source file. Exits on parse failure with rendered
    //    diagnostics (same path `check` and `parse` use).
    let parsed = parse_or_report(&args.file)?;

    // 2. Validate. Same error shape as `check`; exits non-zero on
    //    validation failure so a malformed programme never reaches
    //    the proposal path. The returned `ValidatedProgram` handle
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
                .context("the proposal could not be decided")?;
        match traced {
            PgTracedOutcome::Outcome { outcome, trace } => {
                print_json(&envelopes::Traced {
                    result: &outcome,
                    trace: &trace,
                })?;
                if let PgProposalOutcome::Rejected { reason, .. } = &outcome {
                    return Err(rejected(reason, &parsed));
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
        } = propose_against_pg_with_rejection_state(
            &pool,
            &compiled,
            &Proposal::gateway(&transition),
        )
        .await
        .context("the proposal could not be decided")?;
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
                return Err(rejected(reason, &parsed));
            }
            _ => print_json(&outcome)?,
        }
    } else {
        let outcome = propose_against_pg(&pool, &compiled, &Proposal::gateway(&transition))
            .await
            .context("the proposal could not be decided")?;
        print_json(&outcome)?;
        if let PgProposalOutcome::Rejected { reason, .. } = &outcome {
            return Err(rejected(reason, &parsed));
        }
    }
    Ok(())
}

/// A decided rejection on a single-proposal path: locate the rule on
/// stderr and carry the exit code out (the envelope is already on
/// stdout, so nothing further is printed).
fn rejected(reason: &str, parsed: &ParsedSource) -> anyhow::Error {
    print_rule_location(reason, parsed);
    AlreadyReported.into()
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

/// What exactly went wrong with one row's proposal. The batch only
/// needs the row-vs-operational split (its exit-code contract), but
/// the session answers with a stable per-request code, so the
/// classification keeps the distinctions rather than collapsing them
/// into prose.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowErrorKind {
    /// The row itself did not parse or carried both/neither codec.
    MalformedRow,
    /// The named transformation is not in the programme.
    UnknownTransformation,
    /// The arguments did not decode against the inferred kinds.
    BadArgs,
    /// A SERIALIZABLE conflict: the one failure a caller re-submits.
    Serialization,
    /// The kernel refused to evaluate (a programme/data mismatch).
    Kernel,
    /// The emitted intent collided on its idempotency key.
    DuplicateIntent,
    /// The connecting login role may not propose as the named actor.
    /// A receipt, not an abort: the request was well formed and the
    /// session is healthy - the caller simply may not speak for that
    /// actor. Deliberately NOT a business rejection, so it never
    /// reaches the rejection log.
    ActorAssertionUnauthorised,
    /// Infrastructure: a dead connection, a schema mismatch. Aborts
    /// the batch or the session; never a receipt.
    Operational,
}

/// A per-row failure with its kind. The kind decides receipt-vs-abort
/// (batch) and the stable error code (session); the reason renders
/// into the receipt's human prose.
pub(crate) struct RowError {
    pub(crate) kind: RowErrorKind,
    pub(crate) reason: anyhow::Error,
}

impl RowError {
    fn new(kind: RowErrorKind, reason: anyhow::Error) -> Self {
        Self { kind, reason }
    }
    pub(crate) fn is_operational(&self) -> bool {
        self.kind == RowErrorKind::Operational
    }
}

/// Classify a proposal-path error. `SerializationFailure` is the
/// documented per-row outcome (the caller re-submits that row;
/// retries stay the caller's), and a kernel error or colliding intent
/// is that row's data speaking - everything else is infrastructure.
fn classify_pg_error(err: morpholog_postgres::PgError) -> RowError {
    use morpholog_postgres::PgError;
    let kind = match &err {
        PgError::SerializationFailure => RowErrorKind::Serialization,
        PgError::Kernel(_) => RowErrorKind::Kernel,
        PgError::DuplicateIntent => RowErrorKind::DuplicateIntent,
        PgError::ActorAssertionUnauthorised { .. } => RowErrorKind::ActorAssertionUnauthorised,
        _ => RowErrorKind::Operational,
    };
    RowError::new(
        kind,
        anyhow::Error::new(err).context("the proposal could not be decided"),
    )
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
            Err(err) if err.is_operational() => {
                eprintln!(
                    "batch aborted at row {row}: {committed} committed, \
                     {rejected} rejected, {errored} errors before the failure"
                );
                return Err(err
                    .reason
                    .context(format!("operational failure at row {row}")));
            }
            // A row-level failure is a receipt, never a process
            // failure: the rows after it still run.
            Err(err) => {
                errored += 1;
                serde_json::json!({
                    "row": row,
                    "status": "error",
                    "error": format!("{:#}", err.reason),
                })
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
        .map_err(|e| RowError::new(RowErrorKind::MalformedRow, e))?;
    propose_row_outcome(&args.file, args.explain_on_reject, compiled, pool, row).await
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
    let transformation = lookup_transformation(compiled, &row.transformation, file)
        .map_err(|e| RowError::new(RowErrorKind::UnknownTransformation, e))?;
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
            return Err(RowError::new(
                RowErrorKind::MalformedRow,
                anyhow::anyhow!("a batch row carries exactly one of `args` and `args_named`"),
            ));
        }
    };
    let eval_args = decode_args(&compiled.validated(), transformation, file, codec_input)
        .map_err(|e| RowError::new(RowErrorKind::BadArgs, e))?;
    let transition = Transition {
        transformation_name: transformation.name.clone(),
        args: eval_args,
        actor: Subject::from(row.actor),
    };

    if explain_on_reject {
        let morpholog_postgres::RejectionStateOutcome {
            outcome,
            rejection_state,
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
            .map_err(|e| RowError::new(RowErrorKind::Operational, e));
        }
        serde_json::to_value(&outcome)
            .context("serialising the receipt")
            .map_err(|e| RowError::new(RowErrorKind::Operational, e))
    } else {
        let outcome = propose_against_pg(pool, compiled, &Proposal::gateway(&transition))
            .await
            .map_err(classify_pg_error)?;
        serde_json::to_value(&outcome)
            .context("serialising the receipt")
            .map_err(|e| RowError::new(RowErrorKind::Operational, e))
    }
}
