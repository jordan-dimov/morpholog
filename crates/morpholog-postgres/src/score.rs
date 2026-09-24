//! Driving the candidate scorer over committed history.
//!
//! Replays the audit log under a candidate programme and reports which
//! committed transitions each candidate invariant would have refused. The
//! scoring lives in `morpholog_core::CandidateScorer`; this drives the
//! replay from the live database or from an offline evidence pack.

use crate::as_of::resolve_transition_at_or_before;
use crate::audit::{AuditRow, audit_cursor_for};
use crate::audit_pages::AuditPages;
use crate::checkpoints::{Checkpoint, TreeVerification};
use crate::error::{PgError, classify};
use crate::pack::{EvidencePack, verify_pack};
use crate::txn::{TxIsolation, begin_isolated_tx};
use jiff::Timestamp;
use morpholog_core::{
    BatchScore, CandidateScore, CandidateScorer, CaseOutcome, CaseResult, EvalError, Program,
    SCORE_FORMAT_VERSION, SCORE_SEMANTICS, ScoreError, SplitBoundaryReport, State, effective_delta,
};
use sqlx::PgPool;
use uuid::Uuid;

/// A train/test boundary for a split replay: everything at or before it
/// trains, everything after is held out. Both forms resolve to one
/// `(committed_at, transition_id)` cursor, so each splits at exactly one
/// point in replay order.
#[derive(Debug, Clone, Copy)]
pub enum SplitBoundary {
    /// Split immediately after this transition.
    Transition(Uuid),
    /// Split after the last transition committed at or before this
    /// instant.
    AtOrBefore(Timestamp),
}

impl SplitBoundary {
    /// The canonical form of what was asked, for the report.
    fn requested(&self) -> String {
        match self {
            SplitBoundary::Transition(id) => id.to_string(),
            SplitBoundary::AtOrBefore(at) => crate::wire_time::render(at),
        }
    }
}

/// A resolved boundary waiting to be marked: the cursor to compare
/// rows against, and the report the scorer records at the mark.
struct PendingSplit {
    cursor: (Timestamp, Uuid),
    report: SplitBoundaryReport,
}

fn pending_split(boundary: SplitBoundary, cursor: (Timestamp, Uuid)) -> PendingSplit {
    PendingSplit {
        cursor,
        report: SplitBoundaryReport {
            requested: boundary.requested(),
            resolved_transition_id: cursor.1.to_string(),
            resolved_committed_at: crate::wire_time::render(&cursor.0),
        },
    }
}

/// Construct the scorer. A `pre(...)` candidate is refused as
/// `InvalidState`; kernel faults pass through.
fn build_scorer(program: &Program) -> Result<CandidateScorer<'_>, PgError> {
    match CandidateScorer::new(program) {
        Ok(scorer) => Ok(scorer),
        Err(e @ ScoreError::PreUnsupported(_)) => Err(PgError::InvalidState(e.to_string())),
        Err(ScoreError::Eval(inner)) => Err(PgError::Kernel(inner)),
    }
}

/// Fold audit rows, in canonical order, into the scorer: apply each row to
/// the replayed state and let the scorer observe the result. The live and
/// offline drivers share this fold, so their scores cannot diverge.
fn fold_rows<'a>(
    replay: &mut State,
    scorer: &mut CandidateScorer,
    rows: impl IntoIterator<Item = &'a AuditRow>,
    split: &mut Option<PendingSplit>,
) -> Result<(), EvalError> {
    for row in rows {
        // Mark the boundary before the first row beyond it; a boundary
        // at or past the end of history is marked by the caller after
        // the fold (an empty test slice, not a lost one).
        if let Some(pending) = split.take_if(|p| (row.committed_at, row.transition_id) > p.cursor) {
            scorer.mark_split(pending.report);
        }
        // Read the effective delta before the state advances in place,
        // so no per-row snapshot is needed.
        let effective = effective_delta(replay, &row.asserted_claims, &row.retracted_claims);
        replay.apply(&row.asserted_claims, &row.retracted_claims);
        scorer.observe_transition(replay, &effective, &row.transition_id.to_string())?;
    }
    Ok(())
}

/// Score a candidate programme against the full committed audit log, read
/// under `SERIALIZABLE READ ONLY DEFERRABLE`. Writes nothing.
pub async fn score_candidate(
    pool: &PgPool,
    program: &Program,
    split: Option<SplitBoundary>,
) -> Result<CandidateScore, PgError> {
    // Reject an unscorable candidate before opening any transaction.
    let mut scorer = build_scorer(program)?;
    let mut tx = begin_isolated_tx(pool, TxIsolation::SerializableReadOnlyDeferrable).await?;
    // The boundary resolves inside the replay snapshot, so it and the
    // replayed rows describe the same world even under concurrent
    // writers.
    let mut pending = match split {
        Some(boundary) => {
            let id = match boundary {
                SplitBoundary::Transition(id) => id,
                SplitBoundary::AtOrBefore(at) => {
                    resolve_transition_at_or_before(&mut *tx, at).await?
                }
            };
            let cursor = audit_cursor_for(&mut tx, id).await?;
            Some(pending_split(boundary, cursor))
        }
        None => None,
    };
    let mut replay = State::default();

    let mut pages = AuditPages::new(None);
    loop {
        let page = pages.next(&mut tx).await?;
        if page.is_empty() {
            break;
        }
        fold_rows(&mut replay, &mut scorer, &page, &mut pending)?;
    }
    tx.commit().await.map_err(classify)?;
    // A boundary at or past the end of history: an empty test slice.
    if let Some(p) = pending.take() {
        scorer.mark_split(p.report);
    }

    Ok(scorer.into_report())
}

/// Score a candidate against an evidence pack, offline. Scoring is refused
/// unless the pack verifies as `Intact`; with `anchor` supplied, that also
/// catches a coordinated rewrite. A genuine pack reproduces the live score
/// exactly.
pub fn score_candidate_against_pack(
    program: &Program,
    pack: &EvidencePack,
    anchor: Option<&Checkpoint>,
    split: Option<SplitBoundary>,
) -> Result<CandidateScore, PgError> {
    let mut scorer = build_scorer(program)?;

    match verify_pack(pack, anchor) {
        Ok(TreeVerification::Intact { .. }) => {}
        Ok(_) => {
            return Err(PgError::InvalidState(
                "refusing to score: the evidence pack does not verify as intact \
                 (run `audit verify-pack` for the verdict)"
                    .to_string(),
            ));
        }
        Err(e) => {
            return Err(PgError::InvalidState(format!("refusing to score: {e}")));
        }
    }

    // Replay in canonical order, as the verifier does, whatever order the
    // pack stores rows in.
    let mut rows: Vec<&AuditRow> = pack.rows.iter().collect();
    rows.sort_by_key(|r| (r.committed_at, r.transition_id));

    // Resolved against the pack's own rows, so it splits where the live
    // replay would over the covered prefix.
    let mut pending = match split {
        Some(b @ SplitBoundary::Transition(id)) => Some(pending_split(
            b,
            rows.iter()
                .find(|r| r.transition_id == id)
                .map(|r| (r.committed_at, r.transition_id))
                .ok_or(PgError::TransitionNotFound(id))?,
        )),
        Some(b @ SplitBoundary::AtOrBefore(at)) => Some(pending_split(
            b,
            rows.iter()
                .rev()
                .find(|r| r.committed_at <= at)
                .map(|r| (r.committed_at, r.transition_id))
                .ok_or(PgError::NoTransitionAtOrBefore(at))?,
        )),
        None => None,
    };

    let mut replay = State::default();
    fold_rows(&mut replay, &mut scorer, rows, &mut pending)?;
    if let Some(p) = pending.take() {
        scorer.mark_split(p.report);
    }

    Ok(scorer.into_report())
}

/// Score one candidate against many packs in one call, saving a process
/// spawn per pack. Each pack is scored as [`score_candidate_against_pack`]
/// does; a pack that fails becomes a `Failed` case and the batch goes on.
/// An unscorable candidate fails the whole call once, up front. Offline.
pub fn score_candidate_against_packs(
    program: &Program,
    named_packs: &[(String, EvidencePack)],
) -> Result<BatchScore, PgError> {
    score_candidate_against_packs_lazily(
        program,
        named_packs
            .iter()
            .map(|(name, pack)| Ok::<_, PgError>((name.clone(), pack))),
    )
}

/// As [`score_candidate_against_packs`], taking the packs one at a time as
/// they are loaded, so only one is held at once. An error loading one ends
/// the call.
pub fn score_candidate_against_packs_lazily<E, P>(
    program: &Program,
    named_packs: impl IntoIterator<Item = Result<(String, P), E>>,
) -> Result<BatchScore, E>
where
    E: From<PgError>,
    P: std::borrow::Borrow<EvidencePack>,
{
    // Refuse an unscorable candidate once; each pack builds its own scorer.
    let _ = build_scorer(program)?;

    let mut cases = Vec::new();
    for named in named_packs {
        let (pack, evidence) = named?;
        let outcome = match score_candidate_against_pack(program, evidence.borrow(), None, None) {
            Ok(score) => CaseOutcome::Scored {
                transitions_replayed: score.transitions_replayed,
                invariants: score.invariants,
            },
            Err(e) => CaseOutcome::Failed {
                error: e.to_string(),
            },
        };
        cases.push(CaseResult { pack, outcome });
    }

    Ok(BatchScore {
        score_format_version: SCORE_FORMAT_VERSION,
        semantics: SCORE_SEMANTICS.to_string(),
        program: program.name.clone(),
        program_hash: morpholog_core::format::canonical_hash(program),
        cases,
    })
}
