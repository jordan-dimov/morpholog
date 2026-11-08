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
    ClaimInstance, Compared, IntentInstance, RejectionReason, Subject, Transformation, Transition,
    WitnessBinding,
};
use serde::Serialize;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::PgPool;
use crate::attestation::Proposal;
use crate::compiled::{DeltaStep, disable_jit};
use crate::error::{PgError, classify_commit};
use crate::program::{PgProgram, Route};
use crate::propose::{Drawn, Refusal, decide_act, load_state, record_act, record_refusal, resolve};
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
        /// The comparison the refusal blamed, with its two values.
        #[serde(skip_serializing_if = "Option::is_none")]
        compared: Option<Compared>,
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
    let prepared = program.prepared();
    let mut acts: Vec<(&Transformation, Transition)> = proposals
        .iter()
        .map(|p| resolve(prepared, &p.transformation_name).map(|(t, _, _)| (t, p.transition())))
        .collect::<Result<_, _>>()?;
    let route = program.route();

    let (mut tx, login_role, decided_at) =
        begin_authorised_proposal_tx(pool, &acts[0].1.actor).await?;
    // One transaction, one decision time, for every act.
    for (_, transition) in &mut acts {
        transition.decided_at = Some(decided_at);
    }
    if matches!(route, Route::Compiled(_) | Route::Mixed(_)) {
        disable_jit(&mut tx).await?;
    }

    // One load over everything any act reads, each act keyed by its own
    // arguments: later acts see the earlier acts' effects through the
    // kernel's candidate state, never a reread. A key an act reads
    // through a predicate an earlier act admits is dropped.
    let mut scope = crate::propose::LoadScope::default();
    let mut admitted_before = std::collections::BTreeSet::new();
    for (t, transition) in &acts {
        let mut own = program.load_scope(t, transition, route);
        own.after_admissions_of(&admitted_before);
        scope.union(own);
        morpholog_core::predicates_asserted_by(t, &mut admitted_before);
    }
    let mut state = load_state(&mut tx, &scope).await?;

    let mut receipts = Vec::with_capacity(acts.len());
    // Every act's admissions so far, so an error query orders the batch's
    // rows as the kernel's candidate state holds them.
    let mut steps: Vec<DeltaStep> = Vec::new();
    for (index, (transformation, transition)) in acts.iter().enumerate() {
        let row = index as u64 + 1;
        if index > 0 {
            // Against the policy as the acts before this one left it.
            crate::actor_policy::authorise(&mut tx, &transition.actor, &login_role.name).await?;
        }
        // Each act draws on its own, so its row records its own draws.
        let Drawn { decided, draws } =
            decide_act(&mut tx, route, prepared, transition, &state, &mut steps).await?;
        let recorded = record_act(
            &mut tx,
            transformation,
            transition,
            prepared,
            decided,
            &draws,
            &login_role,
        )
        .await?;
        let mut admitted = match recorded {
            Ok(admitted) => admitted,
            Err(reason) => return refuse(pool, tx, transformation, transition, reason, row).await,
        };
        admitted.advance(&mut state);
        receipts.push(AtomicAct {
            actor: transition.actor.clone(),
            asserted_claims: admitted.asserted,
            emitted_intents: admitted.emitted,
            retracted_claims: admitted.retracted,
            row,
            transition_id: admitted.transition_id,
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
        compared,
    } = record_refusal(pool, tx, transformation, transition, &reason).await?;
    Ok(PgAtomicOutcome::Rejected {
        act: row,
        reason,
        rule,
        witness,
        compared,
    })
}
