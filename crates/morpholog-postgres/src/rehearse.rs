//! Rehearsing a candidate programme over committed history, from the
//! database or from an evidence pack: one fold in canonical audit order,
//! one `propose` per row, the recorded delta applied after each.

use crate::audit::AuditRow;
use crate::audit_pages::AuditPages;
use crate::checkpoints::{Checkpoint, TreeVerification};
use crate::error::{PgError, classify};
use crate::pack::{EvidencePack, verify_pack};
use crate::txn::{TxIsolation, begin_isolated_tx};
use morpholog_core::{PreparedProgram, RecordedProposal, Rehearsal, RehearsalReport, State};
use sqlx::PgPool;

/// How far a rehearsal has got: rows re-decided so far, out of the rows
/// the record holds, control rows counted on both sides.
#[derive(Debug, Clone, Copy)]
pub struct RehearsalProgress {
    pub done: u64,
    pub total: u64,
}

/// Rows between progress calls on the pack path, where there are no pages.
const PROGRESS_CHUNK: usize = 1_000;

fn rehearse_rows<'a>(
    replay: &mut State,
    rehearsal: &mut Rehearsal<'_>,
    rows: impl IntoIterator<Item = &'a AuditRow>,
) {
    for row in rows {
        if row.is_control() {
            row.effect().apply_to(replay);
            continue;
        }
        let transition_id = row.transition_id.to_string();
        rehearsal.observe(
            replay,
            &RecordedProposal {
                transition_id: &transition_id,
                transformation: row.transformation_name.as_str(),
                args: &row.arguments,
                actor: &row.actor,
                asserted: &row.asserted_claims,
                retracted: &row.retracted_claims,
                emitted: &row.emitted_intents,
                drawn: row.drawn_subjects.as_deref(),
                model_hash: row.model_hash.as_deref(),
                semantics_version: row.semantics_version,
            },
        );
        row.effect().apply_to(replay);
    }
}

/// Rehearse a candidate over the full committed audit log, read in one
/// serializable snapshot. `on_progress` is called after each page.
pub async fn rehearse_candidate(
    pool: &PgPool,
    program: &PreparedProgram,
    on_progress: &mut dyn FnMut(RehearsalProgress),
) -> Result<RehearsalReport, PgError> {
    let mut rehearsal = Rehearsal::new(program);
    let mut tx = begin_isolated_tx(pool, TxIsolation::SerializableReadOnlyDeferrable).await?;
    // count(*) is never null, but Postgres cannot prove it.
    let rows = sqlx::query!(r#"SELECT count(*) AS "count!" FROM morpholog.audit"#)
        .fetch_one(&mut *tx)
        .await
        .map_err(classify)?
        .count;
    let total = u64::try_from(rows).unwrap_or(0);
    let mut replay = State::default();
    let mut pages = AuditPages::new(None);
    let mut done = 0u64;
    loop {
        let page = pages.next(&mut tx).await?;
        if page.is_empty() {
            break;
        }
        rehearse_rows(&mut replay, &mut rehearsal, &page);
        done += page.len() as u64;
        on_progress(RehearsalProgress { done, total });
    }
    tx.commit().await.map_err(classify)?;
    Ok(rehearsal.into_report())
}

/// Rehearse a candidate offline over a complete-prefix evidence pack,
/// refusing a pack that does not verify as intact.
pub fn rehearse_candidate_against_pack(
    program: &PreparedProgram,
    pack: &EvidencePack,
    anchor: Option<&Checkpoint>,
    on_progress: &mut dyn FnMut(RehearsalProgress),
) -> Result<RehearsalReport, PgError> {
    match verify_pack(pack, anchor) {
        Ok(TreeVerification::Intact { .. }) => {}
        Ok(_) => {
            return Err(PgError::InvalidState(
                "refusing to rehearse: the evidence pack does not verify as intact \
                 (run `audit verify-pack` for the verdict)"
                    .to_string(),
            ));
        }
        Err(e) => {
            return Err(PgError::InvalidState(format!("refusing to rehearse: {e}")));
        }
    }
    let mut rows: Vec<&AuditRow> = pack.rows.iter().collect();
    rows.sort_by_key(|r| r.position());
    Ok(rehearse_recorded(program, &rows, on_progress))
}

/// Rehearse a candidate over rows already in memory, in the order given,
/// which must be the canonical one. The fold the pack path runs, and
/// what a measurement runs when it wants the decisions apart from the
/// read that fetched the rows.
pub fn rehearse_recorded(
    program: &PreparedProgram,
    rows: &[&AuditRow],
    on_progress: &mut dyn FnMut(RehearsalProgress),
) -> RehearsalReport {
    let mut rehearsal = Rehearsal::new(program);
    let mut replay = State::default();
    let total = rows.len() as u64;
    for (done, chunk) in rows.chunks(PROGRESS_CHUNK).enumerate() {
        rehearse_rows(&mut replay, &mut rehearsal, chunk.iter().copied());
        on_progress(RehearsalProgress {
            done: ((done + 1) * PROGRESS_CHUNK).min(rows.len()) as u64,
            total,
        });
    }
    rehearsal.into_report()
}
