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

fn rehearse_rows<'a>(
    replay: &mut State,
    rehearsal: &mut Rehearsal<'_>,
    rows: impl IntoIterator<Item = &'a AuditRow>,
) {
    for row in rows {
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
                semantics_version: row.semantics_version,
            },
        );
        row.effect().apply_to(replay);
    }
}

/// Rehearse a candidate over the full committed audit log, read in one
/// serializable snapshot.
pub async fn rehearse_candidate(
    pool: &PgPool,
    program: &PreparedProgram,
) -> Result<RehearsalReport, PgError> {
    let mut rehearsal = Rehearsal::new(program);
    let mut tx = begin_isolated_tx(pool, TxIsolation::SerializableReadOnlyDeferrable).await?;
    let mut replay = State::default();
    let mut pages = AuditPages::new(None);
    loop {
        let page = pages.next(&mut tx).await?;
        if page.is_empty() {
            break;
        }
        rehearse_rows(&mut replay, &mut rehearsal, &page);
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
    let mut rehearsal = Rehearsal::new(program);
    let mut replay = State::default();
    rehearse_rows(&mut replay, &mut rehearsal, rows);
    Ok(rehearsal.into_report())
}
