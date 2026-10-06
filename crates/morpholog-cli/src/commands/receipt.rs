//! `morpholog audit receipt` / `verify-receipt` - state what a derived read
//! gives over a complete-prefix pack, and check such a statement offline
//! by recomputing it.

use anyhow::Context;
use morpholog_core::format::canonical_hash;
use morpholog_postgres::{
    CheckpointMatch, Completeness, Evaluation, PackVerdict, ProgramMatch, ReceiptForm,
    ReceiptQuery, ReceiptVerificationReport, SelectiveVerification, TreeVerification, VerdictKind,
    WindowVerification, issue_receipt, parse_receipt, reproduce,
};

use crate::commands::evidence::{
    complete_prefix_of, open_pack_from, pack_report_of, read_complete_prefix,
};
use crate::commands::{AlreadyReported, parse_or_report, print_json, validate_or_report};
use crate::{ReceiptArgs, ReceiptVerifyArgs};

pub(crate) fn issue(args: &ReceiptArgs) -> anyhow::Result<()> {
    let parsed = parse_or_report(&args.file)?;
    let program = validate_or_report(&parsed)?;
    let pack = read_complete_prefix(&args.pack)?;
    let receipt = issue_receipt(
        program,
        &pack,
        ReceiptQuery::Derived {
            predicate: args.derived.clone(),
        },
    )
    .context("no receipt was issued")?;
    print_json(&receipt)
}

/// The receipt's layers, judged in order. The evidence report carries the
/// trust asked for (anchor, signature policy, witnesses); the rows are
/// replayed only from a pack whose verdict proved them, read once.
/// A receipt that does not parse is a verdict, like a pack that does not;
/// a file that cannot be read is operational.
pub(crate) fn verify(args: &ReceiptVerifyArgs) -> anyhow::Result<()> {
    let parsed = parse_or_report(&args.file)?;
    let program = validate_or_report(&parsed)?;
    let receipt = std::fs::read(&args.receipt)
        .with_context(|| format!("reading receipt {}", args.receipt.display()))?;
    let pack_bytes = std::fs::read(&args.pack)
        .with_context(|| format!("reading pack file {}", args.pack.display()))?;
    let evidence = pack_report_of(open_pack_from(&pack_bytes[..], &args.pack)?, &args.trust)?;

    let (verdict_kind, completeness) = match &evidence.verdict {
        PackVerdict::Prefix(verdict) => (
            VerdictKind::Prefix,
            if matches!(verdict, TreeVerification::Intact { .. }) {
                Completeness::Complete
            } else {
                Completeness::NotChecked
            },
        ),
        PackVerdict::Window(verdict) => (
            VerdictKind::Window,
            if matches!(verdict, WindowVerification::Intact { .. }) {
                Completeness::NotComplete
            } else {
                Completeness::NotChecked
            },
        ),
        PackVerdict::Selective(verdict) => (
            VerdictKind::Selective,
            if matches!(verdict, SelectiveVerification::Intact { .. }) {
                Completeness::NotComplete
            } else {
                Completeness::NotChecked
            },
        ),
    };
    let receipt = parse_receipt(&receipt);
    let program_match = match &receipt {
        Ok(r) => {
            let supplied = canonical_hash(program.as_program());
            if supplied == r.receipt().program_hash {
                ProgramMatch::Matches
            } else {
                ProgramMatch::Differs {
                    receipt: r.receipt().program_hash.clone(),
                    supplied,
                }
            }
        }
        Err(_) => ProgramMatch::NotChecked,
    };
    let (checkpoint, evaluation) = match &receipt {
        Ok(r) if completeness == Completeness::Complete => {
            let pack =
                complete_prefix_of(open_pack_from(&pack_bytes[..], &args.pack)?, &args.pack)?;
            reproduce(program, r, &pack)
        }
        _ => (CheckpointMatch::NotChecked, Evaluation::NotEvaluated),
    };
    let report = ReceiptVerificationReport {
        receipt: match receipt {
            Ok(_) => ReceiptForm::WellFormed,
            Err(detail) => ReceiptForm::Malformed { detail },
        },
        verdict_kind,
        evidence,
        completeness,
        checkpoint,
        program: program_match,
        evaluation,
    };
    print_json(&report)?;
    if !report.passes() {
        return Err(AlreadyReported.into());
    }
    Ok(())
}
