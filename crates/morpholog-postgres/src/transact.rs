//! Several proposals as one decision: every act admitted or none.
//!
//! One `SERIALIZABLE` transaction, acts applied in order, each act's
//! gates and invariants evaluated against the state the acts before it
//! staged - including the actor policy, which is read through the same
//! transaction. An accepted act writes its delta, audit row and outbox
//! rows at once, so database-enforced uniqueness, later authorisation
//! and the intents all see one staged reality; the intents reach the
//! outbox only if the whole batch commits. A refused act rolls
//! everything back and is recorded in the operational log alone.
//!
//! Deliberately not `propose --batch`: that is the import shape, one
//! receipt per row and carry on. This is the other contract, chosen by
//! name.

use morpholog_core::{
    ClaimInstance, CompiledProgram, IntentInstance, Outcome, RejectionReason, Subject,
    Transformation, Transition, WitnessBinding, propose,
};
use serde::Serialize;
use uuid::Uuid;

use crate::PgPool;
use crate::attestation::Proposal;
use crate::error::{PgError, classify, classify_commit};
use crate::propose::{
    compute_load_scope, load_state, resolve, rule_identity, write_accepted, write_rejection,
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

/// Propose every act as one decision. An unknown transformation or a
/// misshapen actor-policy declaration (the programme's, checked as the
/// acts resolve) refuses the batch before a transaction opens; the
/// first act's actor is authorised as the transaction opens, every
/// later act's inside it; an empty batch is refused as invalid. On the
/// proposal path's terms, every error but `CommitOutcomeUnknown` means
/// nothing was committed, for the whole batch. A `40001` retries the
/// whole batch; an unknown outcome is read back, never retried blind.
pub async fn propose_all_against_pg(
    pool: &PgPool,
    compiled: &CompiledProgram,
    proposals: &[Proposal],
) -> Result<PgAtomicOutcome, PgError> {
    if proposals.is_empty() {
        return Err(PgError::InvalidState(
            "an atomic batch needs at least one proposal".to_string(),
        ));
    }
    let acts: Vec<(&Transformation, Transition)> = proposals
        .iter()
        .map(|p| resolve(compiled, &p.transformation_name).map(|(t, _, _)| (t, p.transition())))
        .collect::<Result<_, _>>()?;
    let invariants = &compiled.program().invariants;
    let definitions = &compiled.program().definitions;

    let (mut tx, login_role) = begin_authorised_proposal_tx(pool, &acts[0].1.actor).await?;

    // One load over everything any act reads: later acts see the earlier
    // acts' effects through the kernel's candidate state, never a reread.
    let scope: Vec<_> = acts
        .iter()
        .flat_map(|(t, _)| compute_load_scope(t, invariants, definitions))
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
        match propose(transformation, transition, &state, invariants, definitions)? {
            Outcome::Accepted {
                asserted_claims,
                retracted_claims,
                emitted_intents,
                candidate_state,
            } => {
                let transition_id = Uuid::now_v7();
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
                receipts.push(AtomicAct {
                    actor: transition.actor.clone(),
                    asserted_claims,
                    emitted_intents,
                    retracted_claims,
                    row,
                    transition_id,
                });
                state = candidate_state;
            }
            Outcome::Rejected { reason } => {
                tx.rollback().await.map_err(classify)?;
                // Recorded after the rollback, like a single refusal. The
                // witness may carry values the rolled-back prefix staged:
                // it describes this act against that prefix, not history.
                write_rejection(pool, transformation, transition, &reason)
                    .await
                    .map_err(|e| PgError::RejectionLogFailure(Box::new(e)))?;
                let witness = match &reason {
                    RejectionReason::Invariant { witness, .. } => witness.clone(),
                    RejectionReason::Require { .. } | RejectionReason::BindNone { .. } => {
                        Vec::new()
                    }
                };
                return Ok(PgAtomicOutcome::Rejected {
                    act: row,
                    reason: reason.to_string(),
                    rule: rule_identity(&reason),
                    witness,
                });
            }
        }
    }
    tx.commit().await.map_err(classify_commit)?;
    Ok(PgAtomicOutcome::Committed { acts: receipts })
}
