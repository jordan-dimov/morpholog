//! Evaluation receipts: a statement that a programme, over a checkpointed
//! history, gives an answer, which anyone holding a complete-prefix
//! evidence pack and the programme can recompute offline.
//!
//! A receipt is never trusted for its answer. Verification rebuilds the
//! state from authenticated evidence, checks the supplied programme's hash,
//! runs the ordinary evaluator and requires the same answer. It detects
//! any disagreement between what a receipt asserts and what recomputes; it
//! cannot tell which true statement a receipt originally made, since an
//! unsigned receipt rewritten into another true one still recomputes.

use std::collections::BTreeSet;

use morpholog_core::format::canonical_hash;
use morpholog_core::{ClaimInstance, SEMANTICS_VERSION, State, ValidatedProgram};
use serde::{Deserialize, Serialize};

use crate::audit::AuditRow;
use crate::checkpoints::{Checkpoint, TreeVerification};
use crate::error::PgError;
use crate::merkle::Digest;
use crate::pack::{EvidencePack, verify_pack};
use crate::witnesses::PackVerificationReport;

/// The receipt format this binary writes and reads. It fixes the receipt's
/// own shape and canonical form; what the answer means is fixed by
/// [`SEMANTICS_VERSION`].
const RECEIPT_FORMAT_VERSION: u32 = 1;

/// Under programme `program_hash` and semantics `semantics_version`, over
/// the history `checkpoint` commits to, `query` gives `answer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationReceipt {
    pub receipt_format_version: u32,
    /// The canonical hash of the programme that evaluated the query. It
    /// need not be one that admitted the rows; each row names its own.
    pub program_hash: String,
    /// The semantics the answer was computed under, which need not be the
    /// semantics that admitted the rows.
    pub semantics_version: u32,
    pub checkpoint: ReceiptCheckpoint,
    pub query: ReceiptQuery,
    /// In canonical order, each row once.
    pub answer: Vec<ClaimInstance>,
}

/// The history a receipt names: what a checkpoint commits to. Signatures
/// and witnesses sit outside it, so adding one later names the same
/// history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptCheckpoint {
    pub tree_size: i64,
    pub root_hash: Digest,
    pub checkpoint_hash: Digest,
}

impl ReceiptCheckpoint {
    fn of(checkpoint: &Checkpoint) -> Self {
        Self {
            tree_size: checkpoint.tree_size,
            root_hash: checkpoint.root_hash,
            checkpoint_hash: checkpoint.checkpoint_hash,
        }
    }
}

/// The question a receipt answers: one of Morpholog's existing reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReceiptQuery {
    /// Every row of a derived claim at the checkpoint.
    Derived { predicate: String },
}

/// A receipt that passed its form checks. Only [`parse_receipt`] makes
/// one, so whatever takes it can rely on them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedReceipt(EvaluationReceipt);

impl ParsedReceipt {
    pub fn receipt(&self) -> &EvaluationReceipt {
        &self.0
    }
}

/// Read a receipt, refusing any but the one canonical form of its answer,
/// so a receipt has exactly one representation, and a semantics version
/// no Morpholog contract has ever had.
pub fn parse_receipt(bytes: &[u8]) -> Result<ParsedReceipt, String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if let Some(n) = value
        .get("receipt_format_version")
        .and_then(serde_json::Value::as_u64)
        && n > u64::from(RECEIPT_FORMAT_VERSION)
    {
        return Err(format!(
            "receipt_format_version {n} is newer than this binary understands; \
             upgrade morpholog to verify it"
        ));
    }
    let receipt: EvaluationReceipt = serde_json::from_value(value).map_err(|e| e.to_string())?;
    if receipt.receipt_format_version != RECEIPT_FORMAT_VERSION {
        return Err(format!(
            "receipt_format_version {} is not one this binary reads",
            receipt.receipt_format_version
        ));
    }
    if receipt.semantics_version == 0 {
        return Err("semantics_version 0 names no Morpholog semantics".to_string());
    }
    let keys: Vec<String> = receipt.answer.iter().map(canonical_key).collect();
    if keys.windows(2).any(|pair| pair[0] >= pair[1]) {
        return Err("the answer is not in canonical order, or repeats a row".to_string());
    }
    Ok(ParsedReceipt(receipt))
}

/// The order a receipt's answer is written in: by each row's wire form.
fn canonical_key(claim: &ClaimInstance) -> String {
    serde_json::to_string(claim).unwrap_or_default()
}

fn canonical(mut answer: Vec<ClaimInstance>) -> Vec<ClaimInstance> {
    answer.sort_by_cached_key(canonical_key);
    answer.dedup();
    answer
}

/// A derived read over a complete prefix: the state at the pack's
/// covering checkpoint, folded from its rows in log order. Refused unless
/// the pack verifies intact. `None` when the programme derives no such
/// predicate, which is not an empty answer.
fn derive_over_pack(
    program: ValidatedProgram<'_>,
    pack: &EvidencePack,
    predicate: &str,
) -> Result<Option<Vec<ClaimInstance>>, PgError> {
    match verify_pack(pack, None) {
        Ok(TreeVerification::Intact { .. }) => {}
        Ok(_) => {
            return Err(PgError::InvalidState(
                "the evidence pack does not verify as intact \
                 (run `audit verify-pack` for the verdict)"
                    .to_string(),
            ));
        }
        Err(e) => return Err(PgError::InvalidState(e.to_string())),
    }
    Ok(program
        .enumerate_derived(predicate, &state_of(pack))?
        .map(canonical))
}

/// The state a pack's rows fold to, in log order.
fn state_of(pack: &EvidencePack) -> State {
    let mut rows: Vec<&AuditRow> = pack.rows.iter().collect();
    rows.sort_by_key(|r| (r.committed_at, r.transition_id));
    let mut state = State::default();
    for row in rows {
        state.apply(&row.asserted_claims, &row.retracted_claims);
    }
    state
}

/// Evaluate `query` over a complete prefix and state the result as a
/// receipt naming the pack's covering checkpoint.
pub fn issue_receipt(
    program: ValidatedProgram<'_>,
    pack: &EvidencePack,
    query: ReceiptQuery,
) -> Result<EvaluationReceipt, PgError> {
    let ReceiptQuery::Derived { predicate } = &query;
    let answer = derive_over_pack(program, pack, predicate)?.ok_or_else(|| {
        PgError::InvalidState(format!(
            "the programme declares no derived claim `{predicate}`"
        ))
    })?;
    let covering = pack.checkpoints.last().ok_or(PgError::NoCheckpoint)?;
    Ok(EvaluationReceipt {
        receipt_format_version: RECEIPT_FORMAT_VERSION,
        program_hash: canonical_hash(program.as_program()),
        semantics_version: SEMANTICS_VERSION,
        checkpoint: ReceiptCheckpoint::of(covering),
        query,
        answer,
    })
}

/// `audit verify-receipt`'s report: each layer a receipt rests on, kept
/// apart. Evidence integrity is the pack's own verdict; completeness says
/// whether that evidence is the whole history; the checkpoint and
/// programme say whether it is the history and rulebook the receipt names;
/// evaluation says whether the answer recomputes. Whether the history
/// holds every claim the outside world does is not Morpholog's to say.
#[derive(Debug, Clone, Serialize)]
pub struct ReceiptVerificationReport {
    pub receipt: ReceiptForm,
    pub verdict_kind: VerdictKind,
    pub evidence: PackVerificationReport,
    pub completeness: Completeness,
    pub checkpoint: CheckpointMatch,
    pub program: ProgramMatch,
    pub evaluation: Evaluation,
}

/// Which kind of verdict `evidence` carries, so a reader can decode it. A
/// file that is not a pack at all is reported with a prefix verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerdictKind {
    Prefix,
    Window,
    Selective,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ReceiptForm {
    WellFormed,
    Malformed { detail: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Completeness {
    /// An intact complete prefix: every row up to its checkpoint.
    Complete,
    /// Intact, but a window or selective pack, which does not prove the
    /// history before or between its rows.
    NotComplete,
    /// The evidence did not verify, so there is nothing to judge.
    NotChecked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CheckpointMatch {
    Matches,
    Differs {
        receipt: ReceiptCheckpoint,
        pack: ReceiptCheckpoint,
    },
    NotChecked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ProgramMatch {
    /// The supplied programme means what the receipt names; its source may
    /// differ in comments, formatting or surface spelling.
    Matches,
    Differs {
        receipt: String,
        supplied: String,
    },
    NotChecked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Evaluation {
    Reproduced,
    /// Rows the recomputation gives that the receipt lacks, and rows the
    /// receipt states that the recomputation does not give.
    Differs {
        missing: usize,
        unexpected: usize,
    },
    /// The programme derives no such predicate: not an empty answer.
    QueryUnknown {
        predicate: String,
    },
    Errored {
        detail: String,
    },
    /// Every other layer holds, but the receipt was computed under
    /// semantics this binary does not implement.
    NotReEvaluated {
        receipt_semantics: u32,
        binary_semantics: u32,
    },
    /// An earlier layer failed.
    NotEvaluated,
}

/// Whether a receipt names the history a complete-prefix pack covers
/// and, when it does and `program` is the programme it names, whether its
/// answer recomputes there. The pack's integrity is checked here without
/// an anchor or any policy: trust beyond the pack itself is the caller's
/// to establish, and this says nothing about it.
pub fn reproduce(
    program: ValidatedProgram<'_>,
    receipt: &ParsedReceipt,
    pack: &EvidencePack,
) -> (CheckpointMatch, Evaluation) {
    let receipt = &receipt.0;
    let intact = matches!(verify_pack(pack, None), Ok(TreeVerification::Intact { .. }));
    let Some(covering) = pack.checkpoints.last().filter(|_| intact) else {
        return (CheckpointMatch::NotChecked, Evaluation::NotEvaluated);
    };
    let covered = ReceiptCheckpoint::of(covering);
    if covered != receipt.checkpoint {
        return (
            CheckpointMatch::Differs {
                receipt: receipt.checkpoint.clone(),
                pack: covered,
            },
            Evaluation::NotEvaluated,
        );
    }
    if canonical_hash(program.as_program()) != receipt.program_hash {
        return (CheckpointMatch::Matches, Evaluation::NotEvaluated);
    }
    (CheckpointMatch::Matches, reevaluate(program, receipt, pack))
}

fn reevaluate(
    program: ValidatedProgram<'_>,
    receipt: &EvaluationReceipt,
    pack: &EvidencePack,
) -> Evaluation {
    if receipt.semantics_version != SEMANTICS_VERSION {
        return Evaluation::NotReEvaluated {
            receipt_semantics: receipt.semantics_version,
            binary_semantics: SEMANTICS_VERSION,
        };
    }
    let ReceiptQuery::Derived { predicate } = &receipt.query;
    match program.enumerate_derived(predicate, &state_of(pack)) {
        Ok(Some(answer)) => {
            let recomputed: BTreeSet<String> = answer.iter().map(canonical_key).collect();
            let stated: BTreeSet<String> = receipt.answer.iter().map(canonical_key).collect();
            let missing = recomputed.difference(&stated).count();
            let unexpected = stated.difference(&recomputed).count();
            if missing == 0 && unexpected == 0 {
                Evaluation::Reproduced
            } else {
                Evaluation::Differs {
                    missing,
                    unexpected,
                }
            }
        }
        Ok(None) => Evaluation::QueryUnknown {
            predicate: predicate.clone(),
        },
        Err(e) => Evaluation::Errored {
            detail: e.to_string(),
        },
    }
}
