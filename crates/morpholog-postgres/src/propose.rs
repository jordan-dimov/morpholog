use crate::attestation::{AuditAttestation, Proposal};
use crate::error::{PgError, classify};
use crate::txn::{TxIsolation, begin_isolated_tx};
use morpholog_core::{
    ClaimInstance, CompiledProgram, Definition, EvalError, EvalValue, IntentInstance, Invariant,
    InvariantName, Outcome, PredicateName, RejectionReason, RuleName, State, Subject, TraceEntry,
    TracedProposal, Transformation, TransformationName, Transition, WitnessBinding, propose,
    propose_with_trace,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{PgPool, Postgres, Transaction};
use std::collections::HashSet;
use uuid::Uuid;

/// The result of proposing a transformation against PostgreSQL.
///
/// On `Committed`, the database transaction has already been committed:
/// claims have been mutated, one audit row written, and one outbox row
/// per emitted intent. On `Rejected`, the transaction has been rolled
/// back and no governed state has changed - and one row has been
/// recorded in the operational rejection log (`morpholog.rejections`)
/// after the rollback; a failed log insert surfaces as `Err(PgError)`,
/// never as a `Rejected` outcome.
///
/// `Serialize` uses serde's internally-tagged representation so the
/// CLI can emit outcomes directly as JSON with a `status` discriminant.
#[must_use = "a proposal outcome must be inspected; a dropped `Rejected` silently treats a refused change as if it had committed"]
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum PgProposalOutcome {
    Committed {
        transition_id: Uuid,
        #[serde(with = "morpholog_core::actor_repr")]
        actor: Subject,
        asserted_claims: Vec<ClaimInstance>,
        retracted_claims: Vec<ClaimInstance>,
        emitted_intents: Vec<IntentInstance>,
    },
    Rejected {
        reason: String,
        /// The refused rule's stable identifier: an invariant's name, or a
        /// named gate's. Absent when the gate has no name - never the
        /// rendered expression, so a caller reading this field never gets a
        /// value that rewording can change.
        #[serde(skip_serializing_if = "Option::is_none")]
        rule: Option<String>,
        /// The values the refused rule was reading where it failed. Absent
        /// rather than empty when the kernel could not single out an
        /// iteration, so an envelope without a witness is byte-identical
        /// to one from before witnesses existed.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        witness: Vec<WitnessBinding>,
    },
}

/// Propose a transformation against the live `morpholog.*` tables.
///
/// Opens one PostgreSQL transaction at SERIALIZABLE isolation, loads
/// the current claims into an in-memory [`State`], calls the existing
/// synchronous [`propose`] kernel, then either commits the changes
/// (writing claims, audit, and outbox rows) or rolls back atomically.
/// A rejection additionally records one row in the operational
/// rejection log after the rollback (see [`PgProposalOutcome`]).
///
/// External side effects do not run inside this transaction. Outbox rows
/// are enqueued for post-commit delivery by workers running outside.
///
/// The [`Proposal`] bundles the transformation name (verified against
/// `transformation.name`), the arguments, and the [`ActorAttestation`](crate::ActorAttestation)
/// establishing the actor under whose authority the change is proposed.
/// On `Committed`, the actor is persisted to the `morpholog.audit.actor`
/// column and the attestation lineage to `morpholog.audit.attestation`.
pub async fn propose_against_pg(
    pool: &PgPool,
    compiled: &CompiledProgram,
    proposal: &Proposal,
) -> Result<PgProposalOutcome, PgError> {
    let (transformation, invariants, definitions) =
        resolve(compiled, &proposal.transformation_name)?;
    let transition = proposal.transition();
    propose_against_pg_inner(pool, transformation, &transition, invariants, definitions).await
}

/// Resolve the pieces the kernel needs from a compiled programme and the
/// name of the proposed transformation: the transformation itself (by
/// name, O(1)) plus the programme's invariants and definitions. An
/// unknown name is the one new error path the facade introduces; because
/// the lookup is by the proposal's own name, the kernel's
/// `transformation.name == transition.transformation_name` check is then
/// a tautology.
pub(crate) fn resolve<'a>(
    compiled: &'a CompiledProgram,
    name: &TransformationName,
) -> Result<(&'a Transformation, &'a [Invariant], &'a [Definition]), PgError> {
    let transformation = compiled
        .transformation(name)
        .ok_or_else(|| PgError::UnknownTransformation { name: name.clone() })?;
    Ok((
        transformation,
        &compiled.program().invariants,
        &compiled.program().definitions,
    ))
}

/// The decomposed propose primitive, shared by the public facade and the
/// compensation path (which proposes from a [`CompensationSpec`]'s own
/// transformation/invariants/definitions, not a [`CompiledProgram`]).
pub(crate) async fn propose_against_pg_inner(
    pool: &PgPool,
    transformation: &Transformation,
    transition: &Transition,
    invariants: &[Invariant],
    definitions: &[Definition],
) -> Result<PgProposalOutcome, PgError> {
    // The rejection-state variant is the primitive: it is this function
    // plus a free hand-off (the scoped state is moved, never cloned,
    // and only on rejection), so the SERIALIZABLE-setup ritual lives
    // in one fewer place.
    let result = propose_against_pg_with_rejection_state_inner(
        pool,
        transformation,
        transition,
        invariants,
        definitions,
    )
    .await?;
    Ok(result.outcome)
}

/// What [`propose_against_pg_with_rejection_state`] returns: the
/// commit-or-reject outcome, and the pre-state the kernel evaluated -
/// present only on rejection, since that is the snapshot a same-snapshot
/// explanation needs (`None` on commit).
///
/// `#[must_use]` on the struct, not just on `PgProposalOutcome`: the
/// attribute has to be on the type the caller actually receives, or a
/// dropped result (the dangerous case - a refusal treated as a commit)
/// slips through. A bare tuple would not carry the inner attribute.
#[must_use = "the proposal outcome must be inspected; a dropped `Rejected` silently treats a refused change as if it had committed"]
pub struct RejectionStateOutcome {
    pub outcome: PgProposalOutcome,
    pub rejection_state: Option<State>,
}

/// [`propose_against_pg`], additionally returning the scoped
/// pre-state the kernel evaluated - but only when the outcome is a
/// rejection, because that state is exactly what a same-snapshot
/// explanation must describe. A run-then-explain pair reads two
/// snapshots, and the second can differ from the one that refused;
/// handing back the rejecting state closes that gap without a second
/// read. `None` on commit: an admitted change needs no admissibility
/// diagnosis, and the happy path stays free of the hand-off.
pub async fn propose_against_pg_with_rejection_state(
    pool: &PgPool,
    compiled: &CompiledProgram,
    proposal: &Proposal,
) -> Result<RejectionStateOutcome, PgError> {
    let (transformation, invariants, definitions) =
        resolve(compiled, &proposal.transformation_name)?;
    let transition = proposal.transition();
    propose_against_pg_with_rejection_state_inner(
        pool,
        transformation,
        &transition,
        invariants,
        definitions,
    )
    .await
}

pub(crate) async fn propose_against_pg_with_rejection_state_inner(
    pool: &PgPool,
    transformation: &Transformation,
    transition: &Transition,
    invariants: &[Invariant],
    definitions: &[Definition],
) -> Result<RejectionStateOutcome, PgError> {
    let mut tx = begin_isolated_tx(pool, TxIsolation::Serializable).await?;

    let scope = compute_load_scope(transformation, invariants, definitions);
    let state = load_state(&mut tx, &scope).await?;
    let outcome = propose(transformation, transition, &state, invariants, definitions)?;
    let rejection_state = matches!(outcome, Outcome::Rejected { .. }).then_some(state);
    let pg_outcome =
        finalise_outcome(pool, tx, transformation, transition, invariants, outcome).await?;
    Ok(RejectionStateOutcome {
        outcome: pg_outcome,
        rejection_state,
    })
}

/// Three-way outcome returned by [`propose_against_pg_with_trace`].
/// Distinguishes kernel-side outcomes (success, lawful rejection,
/// kernel error) from PG-layer errors that flow through
/// `Result::Err` (`Database`, `SerializationFailure`, `Encoding`,
/// `InvalidState`).
///
/// The `KernelErrored` variant exists so the trace produced by the
/// kernel before the error is raised is **not** discarded: a kernel
/// error mid-transformation is exactly the case where the trace is
/// most valuable for debugging.
#[must_use = "a traced proposal outcome carries the commit/reject result (a dropped `Rejected` silently treats a refused change as committed) and the diagnostic trace"]
#[derive(Debug, Clone)]
pub enum PgTracedOutcome {
    /// Kernel ran to a normal outcome (Committed or Rejected) and
    /// the post-kernel persistence step succeeded. `trace` is the
    /// kernel's per-statement diagnostic record.
    Outcome {
        outcome: PgProposalOutcome,
        trace: Vec<TraceEntry>,
    },
    /// Kernel raised an [`EvalError`]. The SERIALIZABLE transaction
    /// has been rolled back; `trace` carries every statement that
    /// ran before the error.
    KernelErrored {
        error: EvalError,
        trace: Vec<TraceEntry>,
    },
}

/// `propose_against_pg` plus structured per-statement diagnostic
/// trace. Returns a [`PgTracedOutcome`] that carries the trace on
/// **both** kernel success/rejection and kernel error paths.
///
/// Trace preservation contract:
///
/// - **Committed** / **Rejected** kernel outcomes -
///   `Ok(PgTracedOutcome::Outcome { outcome, trace })`. A rejection
///   records its rejection-log row exactly as the untraced path does.
/// - **Kernel error** (`EvalError` raised mid-transformation) -
///   `Ok(PgTracedOutcome::KernelErrored { error, trace })`. The
///   open SERIALIZABLE transaction is rolled back before returning.
/// - **PG-layer error** (`Database`, `SerializationFailure`,
///   `Encoding`, `InvalidState`) - `Err(PgError)`. These errors
///   happen outside the kernel call and have no kernel trace to
///   preserve.
pub async fn propose_against_pg_with_trace(
    pool: &PgPool,
    compiled: &CompiledProgram,
    proposal: &Proposal,
) -> Result<PgTracedOutcome, PgError> {
    let (transformation, invariants, definitions) =
        resolve(compiled, &proposal.transformation_name)?;
    let transition = proposal.transition();
    propose_against_pg_with_trace_inner(pool, transformation, &transition, invariants, definitions)
        .await
}

pub(crate) async fn propose_against_pg_with_trace_inner(
    pool: &PgPool,
    transformation: &Transformation,
    transition: &Transition,
    invariants: &[Invariant],
    definitions: &[Definition],
) -> Result<PgTracedOutcome, PgError> {
    let mut tx = begin_isolated_tx(pool, TxIsolation::Serializable).await?;

    let scope = compute_load_scope(transformation, invariants, definitions);
    let state = load_state(&mut tx, &scope).await?;
    let traced = propose_with_trace(transformation, transition, &state, invariants, definitions);
    match traced {
        TracedProposal::Completed { outcome, trace } => {
            let outcome =
                finalise_outcome(pool, tx, transformation, transition, invariants, outcome).await?;
            Ok(PgTracedOutcome::Outcome { outcome, trace })
        }
        TracedProposal::Errored { error, trace } => {
            // Explicit rollback (rather than relying on drop) frees
            // the connection sooner and surfaces any rollback-time DB
            // failure as a distinct `PgError::Database`.
            tx.rollback().await.map_err(classify)?;
            Ok(PgTracedOutcome::KernelErrored { error, trace })
        }
    }
}

/// Shared post-kernel persistence path used by both
/// `propose_against_pg` and `propose_against_pg_with_trace`. Takes
/// the kernel's [`Outcome`], commits or rolls back, and returns
/// the [`PgProposalOutcome`] the public API exposes.
///
/// A rejection is recorded in `morpholog.rejections` AFTER the
/// rollback, in a separate autocommit insert on `pool` - it cannot
/// live inside the transaction that refused, because that
/// transaction rolls back. At-most-once: a crash between rollback
/// and insert loses the record. Operational evidence only; the
/// audit table remains the legitimacy-grade record. An insert
/// failure surfaces as `Err(PgError)` rather than a rejected
/// envelope - the database is broken, and pretending the refusal
/// was cleanly recorded would not be honest.
pub(crate) async fn finalise_outcome(
    pool: &PgPool,
    mut tx: Transaction<'_, Postgres>,
    transformation: &Transformation,
    transition: &Transition,
    invariants: &[Invariant],
    outcome: Outcome,
) -> Result<PgProposalOutcome, PgError> {
    match outcome {
        Outcome::Rejected { reason } => {
            tx.rollback().await.map_err(classify)?;
            write_rejection(pool, transformation, transition, &reason).await?;
            let witness = match &reason {
                RejectionReason::Invariant { witness, .. } => witness.clone(),
                RejectionReason::Require { .. } | RejectionReason::BindNone { .. } => Vec::new(),
            };
            Ok(PgProposalOutcome::Rejected {
                reason: reason.to_string(),
                rule: rule_identity(&reason),
                witness,
            })
        }
        Outcome::Accepted {
            asserted_claims,
            retracted_claims,
            emitted_intents,
            candidate_state: _,
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
            )
            .await?;
            tx.commit().await.map_err(classify)?;
            Ok(PgProposalOutcome::Committed {
                transition_id,
                actor: transition.actor.clone(),
                asserted_claims,
                retracted_claims,
                emitted_intents,
            })
        }
    }
}

/// Load the pre-state for a `propose_against_pg` call, scoped to a
/// specific set of predicate names.
///
/// `scope` is the list of predicate names the transformation body and
/// the active invariants will consult (see [`compute_load_scope`]).
/// Claims of any other predicate are not loaded - they cannot affect
/// the kernel's evaluation of this transformation, and skipping them
/// avoids fetching and decoding every row in `morpholog.claims`.
///
/// Empty scope returns an empty state without issuing a query
/// (mirrors [`list_claims_for_predicates`]). A transformation that
/// reads no state and has no invariants correctly sees an empty
/// `State`.
pub(crate) async fn load_state(
    tx: &mut Transaction<'_, Postgres>,
    scope: &[PredicateName],
) -> Result<State, PgError> {
    if scope.is_empty() {
        return Ok(State::default());
    }

    // PredicateName is opaque to sqlx; bind the names as `text[]` for the
    // `predicate_name` text column's `ANY(...)` filter (the macro infers
    // `&[String]` for the array parameter).
    let scope: Vec<String> = scope.iter().map(|p| p.as_str().to_owned()).collect();
    // Ordered because a refusal's witness is drawn from the first
    // violating match, so an unordered scan would let the same database
    // and the same claims explain a refusal differently between runs.
    //
    // By the PRIMARY KEY, not by `asserted_at`: any total order gives
    // determinism, and this one the index already provides. Ordering by
    // `asserted_at` forces a sort and measured ~1.8x on propose latency
    // at 20k claims (840ms against 480ms) - a cost every accepted
    // proposal would pay so that refusals reproduce. The key order is
    // also the better guarantee: canonical rather than history-dependent.
    let rows = sqlx::query!(
        "SELECT predicate_name, arguments
         FROM morpholog.claims
         WHERE predicate_name = ANY($1)
         ORDER BY predicate_name, arguments",
        &scope[..],
    )
    .fetch_all(&mut **tx)
    .await
    .map_err(classify)?;

    let mut claims = Vec::with_capacity(rows.len());
    for row in rows {
        let args: Vec<EvalValue> = serde_json::from_value(row.arguments)?;
        claims.push(ClaimInstance {
            predicate: PredicateName::from(row.predicate_name),
            args,
        });
    }
    Ok(State::from_claims(claims))
}

/// Compute the predicate scope that `load_state` must fetch to
/// evaluate this transformation correctly. The union of:
///
/// - Every predicate read by every statement in the transformation
///   body (via `morpholog_core::predicates_read_by_stmt`).
/// - Every predicate referenced by every invariant body (via
///   `morpholog_core::predicates_referenced_by_prop`). Invariants
///   evaluate against the candidate state, so any predicate an
///   invariant inspects must be loaded.
///
/// `Stmt::Assert`'s output predicate is deliberately NOT in the read
/// set: the assert stages a new claim rather than reading existing
/// ones. An invariant that also references it is picked up via the
/// invariant walker.
pub(crate) fn compute_load_scope(
    transformation: &Transformation,
    invariants: &[Invariant],
    definitions: &[Definition],
) -> Vec<PredicateName> {
    let mut scope = std::collections::BTreeSet::new();
    for stmt in &transformation.body {
        morpholog_core::predicates_read_by_stmt(stmt, definitions, &mut scope);
    }
    for inv in invariants {
        morpholog_core::predicates_referenced_by_prop(&inv.body, definitions, &mut scope);
    }
    scope.into_iter().collect()
}

/// One entry in an audit row's `invariants_checked` JSONB array.
/// Recorded per committed transformation: the invariant `name` plus
/// the `version` active at admission time.
///
/// Named `AuditedInvariantCheck`, not `InvariantCheck`, to disambiguate
/// from the kernel's `TraceEntry::InvariantCheck`: this is the durable
/// audit record persisted alongside a committed transition, whereas the
/// kernel variant is a transient per-call diagnostic entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditedInvariantCheck {
    pub name: InvariantName,
    pub version: u32,
}

/// The `kind` column's vocabulary, shared by the writer below and the
/// readers (`coverage_replay`'s invariant attribution) so they cannot
/// drift; the schema's CHECK constraint pins the same three values.
pub(crate) const REJECTION_KIND_INVARIANT: &str = "invariant";

pub(crate) const REJECTION_KIND_REQUIRE: &str = "require";

pub(crate) const REJECTION_KIND_BIND: &str = "bind";

/// The refused rule's stable identifier, or `None` when it has none.
/// Matched off the variant, never parsed out of the Display text - the
/// reason string is prose for a human, and this is the value a caller holds.
fn rule_identity(reason: &RejectionReason) -> Option<String> {
    match reason {
        RejectionReason::Invariant { name, .. } => Some(name.to_string()),
        RejectionReason::Require { name, .. } | RejectionReason::BindNone { name, .. } => {
            name.as_ref().map(ToString::to_string)
        }
    }
}

/// Record a refused proposal in `morpholog.rejections`. Runs on the
/// pool (implicit autocommit transaction) because the refusing
/// transaction has already rolled back - see `finalise_outcome` for
/// the at-most-once doctrine. The kind/rule/version columns come
/// from matching the [`RejectionReason`] variant, never from parsing
/// the display string.
pub(crate) async fn write_rejection(
    pool: &PgPool,
    transformation: &Transformation,
    transition: &Transition,
    reason: &RejectionReason,
) -> Result<(), PgError> {
    let (kind, rule, invariant_version): (&str, &str, Option<i64>) = match reason {
        RejectionReason::Invariant { name, version, .. } => (
            REJECTION_KIND_INVARIANT,
            name.as_str(),
            Some(i64::from(*version)),
        ),
        // A named gate stores its name, so this column means the same
        // thing for every kind and refusals group by cause. Unnamed keeps
        // the rendered expression: this log is an operational floor, and
        // fuller beats emptier here even when the text is not stable.
        RejectionReason::Require { name, rendered } => (
            REJECTION_KIND_REQUIRE,
            name.as_ref().map_or(rendered.as_str(), RuleName::as_str),
            None,
        ),
        RejectionReason::BindNone { name, rendered } => (
            REJECTION_KIND_BIND,
            name.as_ref().map_or(rendered.as_str(), RuleName::as_str),
            None,
        ),
    };
    // NULL rather than `[]` when there is nothing to record, so a row with
    // no witness reads as "none captured" and not as "captured, empty".
    let witness_json: Option<serde_json::Value> = match reason {
        RejectionReason::Invariant { witness, .. } if !witness.is_empty() => {
            Some(serde_json::to_value(witness).map_err(PgError::Encoding)?)
        }
        _ => None,
    };
    let args_json: serde_json::Value =
        serde_json::to_value(&transition.args).map_err(PgError::Encoding)?;
    let actor_json: serde_json::Value =
        serde_json::to_value(EvalValue::Subject(transition.actor.clone()))
            .map_err(PgError::Encoding)?;
    sqlx::query!(
        "INSERT INTO morpholog.rejections (
            rejection_id, transformation_name, arguments, actor,
            kind, rule, invariant_version, reason, witness
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
        Uuid::now_v7(),
        transformation.name.as_str(),
        args_json,
        actor_json,
        kind,
        rule,
        invariant_version,
        reason.to_string(),
        witness_json,
    )
    .execute(pool)
    .await
    .map_err(classify)?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn write_accepted(
    tx: &mut Transaction<'_, Postgres>,
    transition_id: Uuid,
    transformation: &Transformation,
    transition: &Transition,
    invariants: &[Invariant],
    asserted_claims: &[ClaimInstance],
    retracted_claims: &[ClaimInstance],
    emitted_intents: &[IntentInstance],
) -> Result<(), PgError> {
    // Retractions: dedupe, then delete each distinct claim. Exactly
    // one row per distinct retraction is expected; zero rows means a
    // persistent-state mismatch (concurrent interference, which SSI
    // catches later, or a pre-state snapshot that disagrees with the
    // live table).
    let mut seen: HashSet<(PredicateName, String)> = HashSet::new();
    for claim in retracted_claims {
        let args_repr = serde_json::to_string(&claim.args)?;
        let key = (claim.predicate.clone(), args_repr);
        if !seen.insert(key) {
            continue;
        }
        let args_json: serde_json::Value = serde_json::to_value(&claim.args)?;
        let result = sqlx::query!(
            "DELETE FROM morpholog.claims WHERE predicate_name = $1 AND arguments = $2",
            claim.predicate.as_str(),
            args_json,
        )
        .execute(&mut **tx)
        .await
        .map_err(classify)?;
        if result.rows_affected() != 1 {
            return Err(PgError::InvalidState(format!(
                "expected exactly 1 row deleted for retraction of `{}`, got {}",
                claim.predicate,
                result.rows_affected()
            )));
        }
    }

    // Assertions: ON CONFLICT DO NOTHING preserves the set-valued
    // semantics of claims (asserting an already-present claim is an
    // idempotent no-op).
    for claim in asserted_claims {
        let args_json: serde_json::Value = serde_json::to_value(&claim.args)?;
        sqlx::query!(
            "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
             VALUES ($1, $2, $3)
             ON CONFLICT (predicate_name, arguments) DO NOTHING",
            claim.predicate.as_str(),
            args_json,
            transition_id,
        )
        .execute(&mut **tx)
        .await
        .map_err(classify)?;
    }

    // Audit row.
    let checked: Vec<AuditedInvariantCheck> = invariants
        .iter()
        .map(|inv| AuditedInvariantCheck {
            name: inv.name.clone(),
            version: inv.version,
        })
        .collect();
    // The attestation lineage: which PostgreSQL-authenticated login
    // role asserted the actor. Resolved here, inside the committing
    // transaction, from the connection itself - `session_user` is the
    // role PostgreSQL authenticated at login and is immune to SET ROLE,
    // so a caller cannot supply or spoof it through this adapter.
    // session_user is never NULL for an authenticated connection.
    let authenticated_by = sqlx::query_scalar!(r#"SELECT session_user AS "session_user!""#)
        .fetch_one(&mut **tx)
        .await
        .map_err(classify)?;
    let attestation = AuditAttestation::Gateway { authenticated_by };
    // Serialise the actor via the tagged `EvalValue::Subject` so the
    // `actor` column keeps its v0 shape (`#[serde(with = "actor_repr")]`
    // does not apply when the field is serialised directly, only through
    // `Transition`).
    sqlx::query!(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents, attestation
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)",
        transition_id,
        transformation.name.as_str(),
        serde_json::to_value(&transition.args)?,
        serde_json::to_value(EvalValue::Subject(transition.actor.clone()))?,
        1_i32,
        serde_json::to_value(&checked)?,
        serde_json::to_value(asserted_claims)?,
        serde_json::to_value(retracted_claims)?,
        serde_json::to_value(emitted_intents)?,
        serde_json::to_value(&attestation)?,
    )
    .execute(&mut **tx)
    .await
    .map_err(classify)?;

    // Outbox rows, one per emitted intent.
    for intent in emitted_intents {
        let intent_id = Uuid::now_v7();
        let idempotency_key = compute_idempotency_key(transition_id, intent)?;
        let args_json: serde_json::Value = serde_json::to_value(&intent.args)?;
        sqlx::query!(
            "INSERT INTO morpholog.outbox (
                intent_id, transition_id, intent_type, arguments, idempotency_key
             ) VALUES ($1, $2, $3, $4, $5)",
            intent_id,
            transition_id,
            intent.name.as_str(),
            args_json,
            idempotency_key,
        )
        .execute(&mut **tx)
        .await
        .map_err(classify)?;
    }

    Ok(())
}

/// Deterministic idempotency key for an emitted intent:
///
/// ```text
/// hex(sha256(transition_id_bytes ‖ 0x00 ‖ name_bytes ‖ 0x00 ‖ canonical_json(args)))
/// ```
///
/// `canonical_json` is `serde_json` output; the shape is stable for
/// the current structs because field order is fixed by derived
/// `Serialize` and there are no map-like runtime values.
///
/// The key is unique per `(transition_id, intent.name, intent.args)`. It
/// prevents duplicate outbox rows under retry/redelivery mechanics - not
/// duplicate business events, which would require an idempotency key
/// derived from the inbound request.
///
/// Within one transformation, two identical intents share a key and the
/// second `INSERT` violates the `outbox.idempotency_key` constraint,
/// surfacing as [`PgError::DuplicateIntent`] and rolling back the whole
/// transformation - identical duplicate intents are almost always a bug.
pub fn compute_idempotency_key(
    transition_id: Uuid,
    intent: &IntentInstance,
) -> Result<String, serde_json::Error> {
    let args_bytes = serde_json::to_vec(&intent.args)?;
    let mut hasher = Sha256::new();
    hasher.update(transition_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(intent.name.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(&args_bytes);
    Ok(format!("{:x}", hasher.finalize()))
}

// ===========================================================================
// Read API - current-state inspection
// ===========================================================================
//
// These helpers expose the durable substrate for inspection without
// requiring callers to write raw SQL. They return *current* state.
// Historical state ("what did the books look like at transition T?")
// is reachable through the as-of helpers further down in this file:
// `reconstruct_state_at`, `list_claims_at`, `list_derived_at`.
