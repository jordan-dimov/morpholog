//! Driving the candidate scorer over committed history.
//!
//! Replays the audit log forward under a candidate programme and reports
//! which already-admitted commits each candidate invariant would have
//! refused. The kernel logic lives in `morpholog_core::CandidateScorer`;
//! this is only the database-side replay loop, a sibling of
//! `coverage_replay` over the same `ReplaySet` and paginator.

use crate::as_of::ReplaySet;
use crate::audit::{REPLAY_CHUNK, list_audit_rows_page};
use crate::error::{PgError, classify};
use crate::txn::{TxIsolation, begin_isolated_tx};
use morpholog_core::{CandidateScore, CandidateScorer, Program, ScoreError, State};
use sqlx::PgPool;

/// Score a candidate programme against the full committed audit log. Reads
/// under `SERIALIZABLE READ ONLY DEFERRABLE`, folds each transition's
/// claims into a `ReplaySet`, and asks the scorer whether the candidate's
/// invariants would have refused that commit. Commits nothing.
pub async fn score_candidate(pool: &PgPool, program: &Program) -> Result<CandidateScore, PgError> {
    // Reject an unscorable candidate before opening any transaction, so
    // the "reject before database work" doctrine holds at the adapter, not
    // only the CLI.
    let mut scorer = match CandidateScorer::new(program) {
        Ok(scorer) => scorer,
        // A pre(...) candidate is unscorable under v1, not a kernel fault.
        Err(e @ ScoreError::PreUnsupported(_)) => return Err(PgError::InvalidState(e.to_string())),
        Err(ScoreError::Eval(inner)) => return Err(PgError::Kernel(inner)),
    };
    let mut tx = begin_isolated_tx(pool, TxIsolation::SerializableReadOnlyDeferrable).await?;
    let mut replay = ReplaySet::new();
    // The state before each transition; the empty state for the first.
    let mut pre_state = State::from_claims(Vec::new());

    let mut cursor = None;
    loop {
        let page = list_audit_rows_page(&mut tx, cursor, None, REPLAY_CHUNK).await?;
        if page.is_empty() {
            break;
        }
        for row in &page {
            for r in &row.retracted_claims {
                replay.retract(r);
            }
            for a in &row.asserted_claims {
                replay.assert(a);
            }
            let post_state = replay.snapshot_state();
            scorer.observe(&post_state, &pre_state, &row.transition_id.to_string())?;
            pre_state = post_state;
            cursor = Some((row.committed_at, row.transition_id));
        }
        if page.len() < REPLAY_CHUNK as usize {
            break;
        }
    }
    tx.commit().await.map_err(classify)?;

    Ok(scorer.into_report())
}
