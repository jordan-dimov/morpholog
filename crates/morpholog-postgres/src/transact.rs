//! Several proposals as one decision: every act admitted or none.
//!
//! One `SERIALIZABLE` transaction. Acts apply in order, each judged
//! (actor policy included) against the state the earlier acts staged. An
//! accepted act writes its delta, audit and outbox rows at once, so later
//! acts and database uniqueness see them; the intents are delivered only
//! if the whole batch commits. A refused act rolls everything back and is
//! only recorded in the rejection log.
//!
//! Unlike `propose --batch`, which gives one receipt per row and carries
//! on.

use morpholog_core::{
    ClaimInstance, IntentInstance, Outcome, RejectionReason, StagedDelta, Subject, Transformation,
    Transition, WitnessBinding, propose_stage_delta, propose_with,
};
use serde::Serialize;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::PgPool;
use crate::attestation::Proposal;
use crate::compiled::{Stage, disable_jit};
use crate::error::{PgError, classify_commit};
use crate::program::{PgProgram, Route};
use crate::propose::{
    Refusal, load_state, record_refusal, resolve, write_acceptance_record, write_accepted,
    write_claim_delta,
};
use crate::txn::begin_authorised_proposal_tx;

/// One act's receipt inside a committed atomic batch: the single-run
/// committed envelope plus its 1-based `row`.
#[derive(Debug, Clone, Serialize)]
pub struct AtomicAct {
    #[serde(with = "morpholog_core::actor_repr")]
    pub actor: Subject,
    pub asserted_claims: Vec<ClaimInstance>,
    pub emitted_intents: Vec<IntentInstance>,
    pub retracted_claims: Vec<ClaimInstance>,
    pub row: u64,
    pub transition_id: Uuid,
}

/// The one decision: every act committed, in order, or the first act
/// refused and nothing written.
#[must_use = "the batch outcome must be inspected; a dropped `Rejected` silently treats a refused batch as if it had committed"]
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PgAtomicOutcome {
    Committed {
        acts: Vec<AtomicAct>,
    },
    Rejected {
        /// The refusing act's 1-based position. The acts before it were
        /// staged and rolled back; they get no receipt, because a
        /// receipt would read as committed.
        act: u64,
        reason: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        rule: Option<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        witness: Vec<WitnessBinding>,
    },
}

/// Propose every act as one decision.
///
/// An empty batch, an unknown transformation, or a misshapen actor-policy
/// declaration is refused before a transaction opens. Every actor is
/// authorised inside the transaction. Every error but
/// `CommitOutcomeUnknown` means nothing was committed. Retry the whole
/// batch on `40001`; read back an unknown outcome, never retry it blind.
pub async fn propose_all_against_pg(
    pool: &PgPool,
    program: &PgProgram,
    proposals: &[Proposal],
) -> Result<PgAtomicOutcome, PgError> {
    if proposals.is_empty() {
        return Err(PgError::InvalidState(
            "an atomic batch needs at least one proposal".to_string(),
        ));
    }
    let compiled = program.core();
    let acts: Vec<(&Transformation, Transition)> = proposals
        .iter()
        .map(|p| resolve(compiled, &p.transformation_name).map(|(t, _, _)| (t, p.transition())))
        .collect::<Result<_, _>>()?;
    let admission = compiled.admission();
    let invariants = admission.invariants;
    let definitions = admission.definitions;
    let route = program.route();

    let (mut tx, login_role) = begin_authorised_proposal_tx(pool, &acts[0].1.actor).await?;
    if matches!(route, Route::Compiled(_)) {
        disable_jit(&mut tx).await?;
    }

    // One load over everything any act reads: later acts see the earlier
    // acts' effects through the kernel's candidate state, never a reread.
    let scope: Vec<_> = acts
        .iter()
        .flat_map(|(t, _)| program.load_scope(t, route))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut state = load_state(&mut tx, &scope).await?;

    let mut receipts = Vec::with_capacity(acts.len());
    for (index, (transformation, transition)) in acts.iter().enumerate() {
        let row = index as u64 + 1;
        if index > 0 {
            // Against the policy as the acts before this one left it.
            crate::actor_policy::authorise(&mut tx, &transition.actor, &login_role).await?;
        }
        let transition_id = Uuid::now_v7();
        let (asserted_claims, retracted_claims, emitted_intents) = match route {
            Route::Interpreted => {
                match propose_with(transformation, transition, &state, &admission)? {
                    Outcome::Accepted {
                        asserted_claims,
                        retracted_claims,
                        emitted_intents,
                        candidate_state,
                    } => {
                        write_accepted(
                            &mut tx,
                            transition_id,
                            transformation,
                            transition,
                            invariants,
                            &asserted_claims,
                            &retracted_claims,
                            &emitted_intents,
                            &login_role,
                        )
                        .await?;
                        state = candidate_state;
                        (asserted_claims, retracted_claims, emitted_intents)
                    }
                    Outcome::Rejected { reason } => {
                        return refuse(pool, tx, transformation, transition, reason, row).await;
                    }
                }
            }
            Route::Compiled(set) => {
                match propose_stage_delta(transformation, transition, &state, definitions)? {
                    StagedDelta::Rejected { reason } => {
                        return refuse(pool, tx, transformation, transition, reason, row).await;
                    }
                    StagedDelta::Staged {
                        asserted,
                        retracted,
                        emitted,
                    } => {
                        let effective =
                            write_claim_delta(&mut tx, transition_id, &asserted, &retracted)
                                .await?;
                        if let Some(v) = set
                            .first_violation(
                                &mut tx,
                                Stage::CaseBound,
                                &effective.asserted,
                                &effective.retracted,
                            )
                            .await?
                        {
                            return refuse(pool, tx, transformation, transition, v.into(), row)
                                .await;
                        }
                        write_acceptance_record(
                            &mut tx,
                            transition_id,
                            transformation,
                            transition,
                            invariants,
                            &asserted,
                            &retracted,
                            &emitted,
                            &login_role,
                        )
                        .await?;
                        // The next act reads the state these acts left,
                        // matching the claims table.
                        state.apply(&asserted, &retracted);
                        (asserted, retracted, emitted)
                    }
                }
            }
        };
        receipts.push(AtomicAct {
            actor: transition.actor.clone(),
            asserted_claims,
            emitted_intents,
            retracted_claims,
            row,
            transition_id,
        });
    }
    tx.commit().await.map_err(classify_commit)?;
    Ok(PgAtomicOutcome::Committed { acts: receipts })
}

/// Roll the whole batch back, then record the refusing act. Its witness
/// may carry values the rolled-back acts staged: it describes this act
/// against them, not against history.
async fn refuse(
    pool: &PgPool,
    tx: Transaction<'_, Postgres>,
    transformation: &Transformation,
    transition: &Transition,
    reason: RejectionReason,
    row: u64,
) -> Result<PgAtomicOutcome, PgError> {
    let Refusal {
        reason,
        rule,
        witness,
    } = record_refusal(pool, tx, transformation, transition, &reason).await?;
    Ok(PgAtomicOutcome::Rejected {
        act: row,
        reason,
        rule,
        witness,
    })
}
