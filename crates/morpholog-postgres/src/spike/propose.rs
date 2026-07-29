//! SPIKE: the compiled propose sequence. Body statements stay interpreted
//! against a load scoped to the BODY read set only; the delta is written
//! into the open SERIALIZABLE transaction (the claims table then IS the
//! candidate state); invariants are checked by their compiled violation
//! queries; commit or rollback follows exactly the interpreted contract.

use std::collections::BTreeSet;

use morpholog_core::{
    ClaimInstance, CompiledProgram, EvalValue, PredicateArgKind, PredicateName, RejectionReason,
    StagedDelta, Transition, WitnessBinding, propose_stage_delta,
};
use sqlx::Row;
use uuid::Uuid;

use crate::attestation::Proposal;
use crate::error::{PgError, classify};
use crate::propose::{
    finalise_outcome, load_state, resolve, write_audit_outbox, write_claim_delta,
};
use crate::txn::{TxIsolation, begin_isolated_tx};
use crate::{PgPool, PgProposalOutcome};

use super::compile::{CaseFilter, CompiledInvariant, CompiledInvariantSet};

/// Which invariant-check stage the compiled path runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Full-body violation queries: verdict-equivalent to the kernel on
    /// every history, dirty or governed.
    Stage1,
    /// Delta-substituted residuals: case-bound. Skips untouched
    /// invariants entirely; admits non-worsening writes over dirty
    /// history where the kernel would refuse.
    Stage2,
}

/// `propose_against_pg`, with invariant checking compiled to SQL. The
/// caller supplies the [`CompiledInvariantSet`] built once per programme
/// via [`super::compile_invariants`]; a programme with any uncompilable
/// invariant never reaches this function (build the set first, fall back
/// to the interpreted path on refusal).
pub async fn propose_against_pg_compiled(
    pool: &PgPool,
    compiled: &CompiledProgram,
    sql_set: &CompiledInvariantSet,
    proposal: &Proposal,
    stage: Stage,
) -> Result<PgProposalOutcome, PgError> {
    let (transformation, invariants, definitions) =
        resolve(compiled, &proposal.transformation_name)?;
    let transition = proposal.transition();

    let mut tx = begin_isolated_tx(pool, TxIsolation::Serializable).await?;

    // Body read set ONLY: invariant footprints are never loaded, decoded,
    // or indexed - that is the cost this path exists to remove.
    let mut scope = BTreeSet::new();
    for stmt in &transformation.body {
        morpholog_core::predicates_read_by_stmt(stmt, definitions, &mut scope);
    }
    let scope: Vec<PredicateName> = scope.into_iter().collect();
    let state = load_state(&mut tx, &scope).await?;

    let (asserted, retracted, emitted) =
        match propose_stage_delta(transformation, &transition, &state, definitions)? {
            StagedDelta::Rejected { reason } => {
                return finalise_outcome(
                    pool,
                    tx,
                    transformation,
                    &transition,
                    invariants,
                    morpholog_core::Outcome::Rejected { reason },
                )
                .await;
            }
            StagedDelta::Staged {
                asserted,
                retracted,
                emitted,
            } => (asserted, retracted, emitted),
        };

    let transition_id = Uuid::now_v7();
    write_claim_delta(&mut tx, transition_id, &asserted, &retracted).await?;

    for inv in &sql_set.invariants {
        let sql = match stage {
            Stage::Stage1 => inv.violation_sql(None),
            Stage::Stage2 => match inv.case_filter(&asserted, &retracted) {
                CaseFilter::Untouched => continue,
                CaseFilter::Bounded(filter) => inv.violation_sql(Some(&filter)),
                CaseFilter::Unbounded => inv.violation_sql(None),
            },
        };
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_optional(&mut *tx)
            .await
            .map_err(classify)?;
        if let Some(row) = row {
            let witness = decode_witness(inv, &row)?;
            let reason = RejectionReason::Invariant {
                name: inv.name.clone(),
                version: inv.version,
                witness,
            };
            return finalise_outcome(
                pool,
                tx,
                transformation,
                &transition,
                invariants,
                morpholog_core::Outcome::Rejected { reason },
            )
            .await;
        }
    }

    write_audit_outbox(
        &mut tx,
        transition_id,
        transformation,
        &transition,
        invariants,
        &asserted,
        &retracted,
        &emitted,
    )
    .await?;
    tx.commit().await.map_err(classify)?;
    Ok(PgProposalOutcome::Committed {
        transition_id,
        actor: transition.actor.clone(),
        asserted_claims: asserted,
        retracted_claims: retracted,
        emitted_intents: emitted,
    })
}

/// Decode a violation row's witness columns by declared kind. The
/// columns are `::text` casts of the tagged payload's value.
fn decode_witness(
    inv: &CompiledInvariant,
    row: &sqlx::postgres::PgRow,
) -> Result<Vec<WitnessBinding>, PgError> {
    let mut witness = Vec::with_capacity(inv.witness_cols.len());
    for (var, kind) in &inv.witness_cols {
        let col = format!("w_{var}");
        let text: String = row
            .try_get(col.as_str())
            .map_err(|e| PgError::InvalidState(format!("witness column {col} missing: {e}")))?;
        let value = match kind {
            PredicateArgKind::Subject => EvalValue::Subject(text.as_str().into()),
            PredicateArgKind::Decimal => EvalValue::Decimal(text.parse().map_err(|e| {
                PgError::InvalidState(format!("witness decimal {col} unparseable: {e}"))
            })?),
            PredicateArgKind::Bool => EvalValue::Bool(text == "true"),
            other => {
                return Err(PgError::InvalidState(format!(
                    "witness kind {other} outside spike decode"
                )));
            }
        };
        witness.push(WitnessBinding {
            var: var.clone(),
            value,
        });
    }
    Ok(witness)
}

/// A convenience for tests: the delta a staged proposal produced,
/// exposed so harnesses can drive `case_filter` directly.
pub fn staged_delta_for(
    compiled: &CompiledProgram,
    proposal: &Proposal,
    state: &morpholog_core::State,
) -> Result<Option<(Vec<ClaimInstance>, Vec<ClaimInstance>)>, PgError> {
    let (transformation, _, definitions) = resolve(compiled, &proposal.transformation_name)?;
    let transition: Transition = proposal.transition();
    match propose_stage_delta(transformation, &transition, state, definitions)? {
        StagedDelta::Rejected { .. } => Ok(None),
        StagedDelta::Staged {
            asserted,
            retracted,
            ..
        } => Ok(Some((asserted, retracted))),
    }
}
