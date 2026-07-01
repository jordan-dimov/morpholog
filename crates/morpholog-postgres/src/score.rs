//! Driving the candidate scorer over committed history.
//!
//! Replays the audit log forward under a candidate programme and reports
//! which already-admitted commits each candidate invariant would have
//! refused. The kernel logic lives in `morpholog_core::CandidateScorer`;
//! this is the replay driver - a sibling of `coverage_replay` over the
//! same `ReplaySet`. Two sources feed the same fold: the live database,
//! and a portable evidence pack (offline, no connection).

use crate::as_of::ReplaySet;
use crate::audit::{AuditRow, REPLAY_CHUNK, list_audit_rows_page};
use crate::checkpoints::{Checkpoint, TreeVerification};
use crate::error::{PgError, classify};
use crate::pack::{EvidencePack, verify_pack};
use crate::txn::{TxIsolation, begin_isolated_tx};
use morpholog_core::{
    BatchScore, CandidateScore, CandidateScorer, CaseOutcome, CaseResult, EvalError, Program,
    SCORE_FORMAT_VERSION, SCORE_SEMANTICS, ScoreError, State,
};
use sqlx::PgPool;

/// Construct the scorer, mapping its refusal of an unscorable candidate
/// onto the adapter's error - a `pre(...)` candidate is rejected before
/// any further work, kernel faults pass through.
fn build_scorer(program: &Program) -> Result<CandidateScorer<'_>, PgError> {
    match CandidateScorer::new(program) {
        Ok(scorer) => Ok(scorer),
        Err(e @ ScoreError::PreUnsupported(_)) => Err(PgError::InvalidState(e.to_string())),
        Err(ScoreError::Eval(inner)) => Err(PgError::Kernel(inner)),
    }
}

/// Fold a run of audit rows (in canonical order) into the scorer: each
/// row's retractions then assertions update the `ReplaySet`, the post-state
/// is snapshotted, and the scorer observes it against the carried pre-state.
/// One fold for both the database and pack drivers, so the live and offline
/// scores cannot diverge.
fn fold_rows<'a>(
    replay: &mut ReplaySet,
    pre_state: &mut State,
    scorer: &mut CandidateScorer,
    rows: impl IntoIterator<Item = &'a AuditRow>,
) -> Result<(), EvalError> {
    for row in rows {
        for r in &row.retracted_claims {
            replay.retract(r);
        }
        for a in &row.asserted_claims {
            replay.assert(a);
        }
        let post_state = replay.snapshot_state();
        scorer.observe(&post_state, pre_state, &row.transition_id.to_string())?;
        *pre_state = post_state;
    }
    Ok(())
}

/// Score a candidate programme against the full committed audit log. Reads
/// under `SERIALIZABLE READ ONLY DEFERRABLE`, folds each transition's
/// claims into a `ReplaySet`, and asks the scorer whether the candidate's
/// invariants would have refused that commit. Commits nothing.
pub async fn score_candidate(pool: &PgPool, program: &Program) -> Result<CandidateScore, PgError> {
    // Reject an unscorable candidate before opening any transaction.
    let mut scorer = build_scorer(program)?;
    let mut tx = begin_isolated_tx(pool, TxIsolation::SerializableReadOnlyDeferrable).await?;
    let mut replay = ReplaySet::new();
    let mut pre_state = State::from_claims(Vec::new());

    let mut cursor = None;
    loop {
        let page = list_audit_rows_page(&mut tx, cursor, None, REPLAY_CHUNK).await?;
        if page.is_empty() {
            break;
        }
        fold_rows(&mut replay, &mut pre_state, &mut scorer, &page)?;
        if let Some(last) = page.last() {
            cursor = Some((last.committed_at, last.transition_id));
        }
        if page.len() < REPLAY_CHUNK as usize {
            break;
        }
    }
    tx.commit().await.map_err(classify)?;

    Ok(scorer.into_report())
}

/// Score a candidate against a portable evidence pack - offline, no
/// database. The pack is verified first and scoring is refused unless it
/// is `Intact`: scoring a pack that does not verify would be
/// meaningless. With `anchor` supplied the check also
/// catches a coordinated rewrite. The report is identical to the database
/// path, so a genuine pack reproduces the live score exactly.
pub fn score_candidate_against_pack(
    program: &Program,
    pack: &EvidencePack,
    anchor: Option<&Checkpoint>,
) -> Result<CandidateScore, PgError> {
    let mut scorer = build_scorer(program)?;

    match verify_pack(pack, anchor) {
        Ok(TreeVerification::Intact { .. }) => {}
        Ok(_) => {
            return Err(PgError::InvalidState(
                "refusing to score: the evidence pack does not verify as intact \
                 (run `evidence verify` for the verdict)"
                    .to_string(),
            ));
        }
        Err(e) => {
            return Err(PgError::InvalidState(format!("refusing to score: {e}")));
        }
    }

    // The pack's serialization order is not load-bearing; replay in
    // canonical order, exactly as the verifier recomputes the root.
    let mut rows: Vec<&AuditRow> = pack.rows.iter().collect();
    rows.sort_by_key(|r| (r.committed_at, r.transition_id));

    let mut replay = ReplaySet::new();
    let mut pre_state = State::from_claims(Vec::new());
    fold_rows(&mut replay, &mut pre_state, &mut scorer, rows)?;

    Ok(scorer.into_report())
}

/// Score one candidate against many packs in a single call - the discovery
/// search is candidates x cases, so this collapses the per-case process
/// spawn and parses the candidate once. Each pack is verified and scored
/// exactly as [`score_candidate_against_pack`] does (a fresh `CandidateScorer`
/// per pack, since it is stateful); a pack that fails (does not verify,
/// kernel error) becomes a `Failed` case and the batch continues. The
/// candidate is validated once up front so a `pre(...)` or otherwise
/// unscorable candidate fails the whole call with one error rather than N
/// identical case failures. Offline; no pool. The win is amortising the
/// process spawn, not the per-pack `CandidateScorer::new`.
pub fn score_candidate_against_packs(
    program: &Program,
    named_packs: &[(String, EvidencePack)],
) -> Result<BatchScore, PgError> {
    // Whole-batch candidate rejection (pre(...) / invalid scorer seed); each
    // pack then builds its own fresh scorer inside score_candidate_against_pack.
    let _ = build_scorer(program)?;

    let cases = named_packs
        .iter()
        .map(|(name, pack)| {
            let outcome = match score_candidate_against_pack(program, pack, None) {
                Ok(score) => CaseOutcome::Scored {
                    transitions_replayed: score.transitions_replayed,
                    invariants: score.invariants,
                },
                Err(e) => CaseOutcome::Failed {
                    error: e.to_string(),
                },
            };
            CaseResult {
                pack: name.clone(),
                outcome,
            }
        })
        .collect();

    Ok(BatchScore {
        score_format_version: SCORE_FORMAT_VERSION,
        semantics: SCORE_SEMANTICS.to_string(),
        program: program.name.clone(),
        program_hash: morpholog_core::format::canonical_hash(program),
        cases,
    })
}
