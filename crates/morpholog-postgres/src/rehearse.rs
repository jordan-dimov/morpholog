//! Rehearsing a candidate programme over committed history, from the
//! database or from an evidence pack: one fold in canonical audit order,
//! one `propose` per row, the recorded delta applied after each.

use crate::audit::AuditRow;
use crate::audit_pages::AuditPages;
use crate::checkpoints::{Checkpoint, TreeVerification};
use crate::clock::{self, Span};
use crate::error::{PgError, classify};
use crate::pack::{EvidencePack, verify_pack};
use crate::replay::Upkeep;
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
    upkeep: Upkeep,
) {
    for row in rows {
        if row.is_control() {
            advance(replay, row, upkeep);
            continue;
        }
        let deciding = clock::start();
        let transition_id = row.transition_id.to_string();
        rehearsal.observe(
            replay,
            &RecordedProposal {
                transition_id: &transition_id,
                transformation: row.transformation_name.as_str(),
                args: &row.arguments,
                actor: &row.actor,
                committed_at: row.committed_at,
                asserted: &row.asserted_claims,
                retracted: &row.retracted_claims,
                emitted: &row.emitted_intents,
                drawn: row.drawn_subjects.as_deref(),
                model_hash: row.model_hash.as_deref(),
                semantics_version: row.semantics_version,
            },
        );
        clock::stop(Span::Decide, deciding);
        advance(replay, row, upkeep);
    }
}

fn advance(replay: &mut State, row: &AuditRow, upkeep: Upkeep) {
    let advancing = clock::start();
    row.effect().apply_to(replay, upkeep);
    clock::stop(Span::Advance, advancing);
}

/// The fold's step after each decision: the replay state advanced by the
/// row's recorded effect, whatever the candidate decided. Public for a
/// measurement that times the decisions apart from this step.
#[doc(hidden)]
pub fn advance_recorded(replay: &mut State, row: &AuditRow) {
    row.effect().apply_to(replay, Upkeep::Replay);
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
        rehearse_rows(&mut replay, &mut rehearsal, &page, Upkeep::Replay);
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
        rehearse_rows(
            &mut replay,
            &mut rehearsal,
            chunk.iter().copied(),
            Upkeep::Replay,
        );
        on_progress(RehearsalProgress {
            done: ((done + 1) * PROGRESS_CHUNK).min(rows.len()) as u64,
            total,
        });
    }
    rehearsal.into_report()
}

#[cfg(test)]
mod tests {
    use super::*;
    use morpholog_core::{ClaimInstance, EvalValue, Subject};
    use morpholog_test_support::{dec, subj};
    use uuid::Uuid;

    /// A legacy audit row of one act, its effect read from its claims.
    fn row(
        i: u128,
        act: &str,
        args: Vec<EvalValue>,
        asserted: Vec<ClaimInstance>,
        retracted: Vec<ClaimInstance>,
    ) -> AuditRow {
        AuditRow {
            transition_id: Uuid::from_u128(i),
            transformation_name: act.into(),
            arguments: args,
            actor: Subject::from("bench"),
            invariant_epoch: 1,
            invariants_checked: Vec::new(),
            asserted_claims: asserted,
            retracted_claims: retracted,
            emitted_intents: Vec::new(),
            committed_at: jiff::Timestamp::from_second(1_700_000_000 + i as i64).unwrap(),
            attestation: None,
            parameters: None,
            model_hash: None,
            semantics_version: None,
            drawn_subjects: None,
            audit_order: None,
            act_ordinal: None,
            ordered_at: None,
            operations: None,
        }
    }

    fn claim(predicate: &str, args: &[EvalValue]) -> ClaimInstance {
        ClaimInstance {
            predicate: predicate.into(),
            args: args.to_vec(),
        }
    }

    /// A replay's upkeep changes when the fold's state is re-indexed and
    /// nothing else: the production fold run row by row under both
    /// upkeeps holds the same claims in the same order after every row,
    /// and decides every row alike, refusals, errors and their witnesses
    /// included. The history crosses both of the replay's bounds: legacy
    /// rows grow the overlay past its floor, an activation resets the
    /// state to a baseline, and ordered rows revise it, re-admit claims to
    /// the tail and carry no-op operations until the dead entries pass
    /// the live ones.
    #[test]
    fn a_replay_upkeep_folds_to_the_same_state_and_report_at_every_row() {
        use crate::activation::{ACTIVATE, BASELINE};
        use crate::audit::{Operation, OperationKind};
        let program = morpholog_surface::parse_program(
            "program register\n\
             predicate Row(row: Subject, version: Decimal)\n\
                 unique by (row)\n\
             predicate Note(key: Subject)\n\
             invariant versions_stay_small:\n\
             \x20   Row(_, v) implies v < 1000\n\
             invariant no_version_at_501:\n\
             \x20   Row(_, v) implies 1 / (v - 501) > -1000\n\
             transformation admit_row(row, version):\n\
             \x20   retract Row(row: row, ..)\n\
             \x20   admit Row(row, version)\n\
             transformation touch(row, version):\n\
             \x20   retract Row(row, version)\n\
             \x20   admit Row(row, version)\n\
             transformation note(key):\n\
             \x20   admit Note(key)\n",
        )
        .unwrap();
        let prepared = PreparedProgram::new(program).unwrap();
        let ordered = |mut r: AuditRow, ordinal: u32, operations: Vec<Operation>| {
            r.act_ordinal = Some(ordinal);
            r.operations = Some(operations);
            r
        };
        let row_claim = |key: &str, version: i64| claim("Row", &[subj(key), dec(version)]);
        let mut rows = Vec::new();
        let mut id = 0u128;
        let mut next_id = || {
            id += 1;
            id
        };

        // Legacy rows: the overlay grows past the replay's floor of 4,096.
        for i in 0..4_200 {
            let key = subj(&format!("k{i}"));
            rows.push(row(
                next_id(),
                "note",
                vec![key.clone()],
                vec![claim("Note", &[key])],
                vec![],
            ));
        }
        // The activation: two baseline chunks, then the record.
        let baseline = 200usize;
        for (ordinal, chunk) in [(0u32, 0..baseline / 2), (1, baseline / 2..baseline)] {
            let operations = chunk
                .map(|j| Operation::new(OperationKind::Ins, &row_claim(&format!("b{j}"), 1)))
                .collect();
            rows.push(ordered(
                row(next_id(), BASELINE, vec![], vec![], vec![]),
                ordinal,
                operations,
            ));
        }
        rows.push(ordered(
            row(next_id(), ACTIVATE, vec![], vec![], vec![]),
            2,
            vec![],
        ));
        // Ordered rows: revisions, a re-admission every seventh row and a
        // no-op operation every eleventh, until the dead pass the replay's
        // floor of 1,024.
        let mut version = vec![1i64; baseline];
        for i in 0..1_200usize {
            let j = i % baseline;
            let key = format!("b{j}");
            let before = row_claim(&key, version[j]);
            let (act, after) = if i % 7 == 0 {
                ("touch", before.clone())
            } else {
                version[j] = 2 + i as i64;
                ("admit_row", row_claim(&key, version[j]))
            };
            let mut operations = vec![
                Operation::new(OperationKind::Del, &before),
                Operation::new(OperationKind::Ins, &after),
            ];
            if i % 11 == 0 {
                operations.push(Operation::new(OperationKind::Noop, &after));
            }
            let args = vec![subj(&key), after.args[1].clone()];
            rows.push(ordered(
                row(next_id(), act, args, vec![after], vec![before]),
                0,
                operations,
            ));
        }

        let mut ordinary = State::default();
        let mut replayed = State::default();
        let mut ordinary_rehearsal = Rehearsal::new(&prepared);
        let mut replay_rehearsal = Rehearsal::new(&prepared);
        let (mut ordinary_rebuilds, mut replay_rebuilds) = (0, 0);
        let rebuilds = || morpholog_core::tally::snapshot()[2];
        for (n, row) in rows.iter().enumerate() {
            let before = rebuilds();
            rehearse_rows(
                &mut ordinary,
                &mut ordinary_rehearsal,
                [row],
                Upkeep::Ordinary,
            );
            ordinary_rebuilds += rebuilds() - before;
            let before = rebuilds();
            rehearse_rows(&mut replayed, &mut replay_rehearsal, [row], Upkeep::Replay);
            replay_rebuilds += rebuilds() - before;
            assert!(ordinary == replayed, "the states part after row {n}");
        }
        let ordinary_report = ordinary_rehearsal.into_report();
        let replay_report = replay_rehearsal.into_report();
        assert_eq!(
            serde_json::to_value(&ordinary_report).unwrap(),
            serde_json::to_value(&replay_report).unwrap(),
            "the reports part"
        );
        let totals = &replay_report.totals;
        assert!(
            totals.same > 0 && totals.would_reject > 0 && totals.would_error > 0,
            "the history decides alike, refuses and errors: {totals:?}"
        );
        // The replay compacted once for each cause: its overlay bound in
        // the legacy rows, its dead bound in the ordered ones.
        assert_eq!(replay_rebuilds, 2);
        assert!(
            ordinary_rebuilds > replay_rebuilds,
            "{ordinary_rebuilds} vs {replay_rebuilds}"
        );
    }

    /// The fold this module runs, over rows in memory: a register revised
    /// behind claims no rule reads clones no state and rebuilds none, and
    /// decides every row as recorded. The kernel holds the same of its own
    /// observer; this holds it of the loop around it, which once compacted
    /// the state after every row.
    #[test]
    fn the_fold_behind_unrelated_state_clones_nothing_and_rebuilds_nothing() {
        let program = morpholog_surface::parse_program(
            "program register\n\
             predicate Row(row: Subject, version: Decimal)\n\
                 unique by (row)\n\
             predicate RowKinds(row: Subject, kind: Subject)\n\
                 current pointer by (row)\n\
             predicate Noise(key: Subject, figure: Decimal)\n\
             invariant kinds_describe_a_row:\n\
             \x20   RowKinds(row, _) implies Row(row, _)\n\
             transformation note_noise(key, figure):\n    admit Noise(key, figure)\n\
             transformation admit_row(row, version, kind):\n\
             \x20   retract Row(row: row, ..)\n\
             \x20   retract RowKinds(row: row, ..)\n\
             \x20   admit Row(row, version)\n\
             \x20   admit RowKinds(row, kind)\n",
        )
        .unwrap();
        let prepared = PreparedProgram::new(program).unwrap();
        let noise = 20_000u128;
        let keys = 50u128;
        // Under a replay's floors at this live set; the old per-row
        // policy's floor of 512 would have rebuilt several times over these.
        let revisions = 500u128;
        let mut rows = Vec::new();
        for i in 0..noise {
            let key = subj(&format!("k{i}"));
            rows.push(row(
                i + 1,
                "note_noise",
                vec![key.clone(), dec(i as i64)],
                vec![claim("Noise", &[key, dec(i as i64)])],
                vec![],
            ));
        }
        for i in 0..revisions {
            let key = subj(&format!("row_{}", i % keys));
            let retracted = if i < keys {
                vec![]
            } else {
                vec![
                    claim("Row", &[key.clone(), dec((i - keys) as i64)]),
                    claim("RowKinds", &[key.clone(), subj("k")]),
                ]
            };
            rows.push(row(
                noise + i + 1,
                "admit_row",
                vec![key.clone(), dec(i as i64), subj("k")],
                vec![
                    claim("Row", &[key.clone(), dec(i as i64)]),
                    claim("RowKinds", &[key, subj("k")]),
                ],
                retracted,
            ));
        }
        let refs: Vec<&AuditRow> = rows.iter().collect();

        let before = morpholog_core::tally::snapshot();
        let report = rehearse_recorded(&prepared, &refs, &mut |_| {});
        let after = morpholog_core::tally::snapshot();

        assert_eq!(
            report.totals.same,
            (noise + revisions) as u64,
            "{:?}",
            report.totals
        );
        assert_eq!(report.totals.unreplayable, 0);
        let [clones, _, rebuilds, _] = std::array::from_fn(|i| after[i] - before[i]);
        assert_eq!(clones, 0, "the fold clones no state");
        // The noise prefix grows the live set from nothing, which a
        // replay compacts a few times; the register's revisions behind it
        // reach no rebuild on their own.
        let noise_only: Vec<&AuditRow> = rows[..noise as usize].iter().collect();
        let before = morpholog_core::tally::snapshot();
        rehearse_recorded(&prepared, &noise_only, &mut |_| {});
        let after = morpholog_core::tally::snapshot();
        let rebuilds_for_noise = after[2] - before[2];
        assert_eq!(
            rebuilds,
            rebuilds_for_noise,
            "the register's revisions rebuilt the state {} times behind the noise",
            rebuilds - rebuilds_for_noise
        );
    }
}
