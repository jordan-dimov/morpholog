use crate::attestation::{AuditAttestation, Proposal};
use crate::compiled::{DeltaStep, Stage, disable_jit};
use crate::error::{PgError, classify, classify_checked_query, classify_commit};
use crate::program::{PgProgram, Route};
use crate::sql_quote::quote_literal;
use crate::txn::{LoginRole, begin_authorised_proposal_tx};
use morpholog_core::{
    Admission, ClaimInstance, CompiledProgram, Definition, EffectiveDelta, EvalError, EvalValue,
    IntentInstance, Invariant, InvariantName, Outcome, PredicateName, ReadFilter, ReadPlan,
    RejectionReason, RuleName, StagedDelta, State, Subject, TraceEntry, TracedProposal,
    Transformation, TransformationName, Transition, WitnessBinding, propose_stage_delta,
    propose_with, propose_with_trace,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use sqlx::{PgPool, Postgres, Row as _, Transaction};
use std::collections::{BTreeMap, HashSet};
use uuid::Uuid;

/// The result of proposing a transformation against PostgreSQL.
///
/// On `Committed`, the transaction has committed: claims changed, one
/// audit row, and one outbox row per emitted intent. On `Rejected`, it
/// rolled back and no governed state changed; one row was then recorded
/// in the rejection log (`morpholog.rejections`). A failed log insert is
/// `Err(PgError)`, never `Rejected`.
///
/// Serialises with a `status` tag.
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
        /// The refused rule's stable name: an invariant's, or a named
        /// gate's. Absent for an unnamed gate, never the rendered
        /// expression, so rewording cannot change it.
        #[serde(skip_serializing_if = "Option::is_none")]
        rule: Option<String>,
        /// The values the refused rule was reading where it failed. Omitted
        /// when the kernel could not single out an iteration.
        #[serde(skip_serializing_if = "Vec::is_empty")]
        witness: Vec<WitnessBinding>,
    },
}

/// Propose a transformation against the live `morpholog.*` tables.
///
/// Opens one SERIALIZABLE transaction, loads the claims the proposal
/// needs into an in-memory [`State`], and runs the body through the
/// kernel. When the invariants compile to SQL ([`PgProgram::plan`]), only
/// the body's reads are loaded, the delta is written into the
/// transaction, and each invariant is checked in programme order against
/// the claims table. Otherwise the invariants' reads are loaded too and
/// the interpreter checks them. Either way, claims, audit and outbox rows
/// commit or roll back together; a rejection is then logged (see
/// [`PgProposalOutcome`]).
///
/// External side effects never run inside this transaction: outbox rows
/// are delivered after commit by workers outside it.
///
/// The [`Proposal`] carries the transformation name, the arguments, and
/// the [`ActorAttestation`](crate::ActorAttestation) establishing the
/// actor. On `Committed`, the actor goes to `morpholog.audit.actor` and
/// the attestation to `morpholog.audit.attestation`.
pub async fn propose_against_pg(
    pool: &PgPool,
    program: &PgProgram,
    proposal: &Proposal,
) -> Result<PgProposalOutcome, PgError> {
    let (transformation, admission) =
        resolve_admission(program.core(), &proposal.transformation_name)?;
    let transition = proposal.transition();
    let run = propose_against_pg_run(
        pool,
        program.route(),
        transformation,
        &transition,
        &admission,
        false,
    )
    .await?;
    Ok(run.outcome)
}

/// Look up the named transformation and the programme's invariants and
/// definitions. Refuses misshapen actor-policy declarations; an unknown
/// name is [`PgError::UnknownTransformation`].
pub(crate) fn resolve<'a>(
    compiled: &'a CompiledProgram,
    name: &TransformationName,
) -> Result<(&'a Transformation, &'a [Invariant], &'a [Definition]), PgError> {
    let findings = crate::actor_policy::validate_declarations(compiled.program());
    if !findings.is_empty() {
        return Err(PgError::ActorPolicyDeclaration {
            findings: findings.iter().map(ToString::to_string).collect(),
        });
    }
    let transformation = compiled
        .transformation(name)
        .ok_or_else(|| PgError::UnknownTransformation { name: name.clone() })?;
    Ok((
        transformation,
        &compiled.program().invariants,
        &compiled.program().definitions,
    ))
}

/// [`resolve`] plus the programme's admission rules, with the impact
/// plans it built at construction.
pub(crate) fn resolve_admission<'a>(
    compiled: &'a CompiledProgram,
    name: &TransformationName,
) -> Result<(&'a Transformation, Admission<'a>), PgError> {
    let (transformation, _, _) = resolve(compiled, name)?;
    Ok((transformation, compiled.admission()))
}

/// The interpreted propose primitive for compensation, which carries its
/// own transformation, invariants and definitions rather than a programme.
pub(crate) async fn propose_against_pg_inner(
    pool: &PgPool,
    transformation: &Transformation,
    transition: &Transition,
    invariants: &[Invariant],
    definitions: &[Definition],
) -> Result<PgProposalOutcome, PgError> {
    let run = propose_against_pg_run(
        pool,
        Route::Interpreted,
        transformation,
        transition,
        &Admission::of(invariants, definitions),
        false,
    )
    .await?;
    Ok(run.outcome)
}

/// [`propose_against_pg`] with a breakdown of where the wall time went.
/// Same path, same outcome; the ordinary facade reads no clock.
pub async fn propose_against_pg_timed(
    pool: &PgPool,
    program: &PgProgram,
    proposal: &Proposal,
) -> Result<TimedProposalOutcome, PgError> {
    let (transformation, admission) =
        resolve_admission(program.core(), &proposal.transformation_name)?;
    let transition = proposal.transition();
    let run = propose_against_pg_run(
        pool,
        program.route(),
        transformation,
        &transition,
        &admission,
        true,
    )
    .await?;
    let phases = run
        .phases
        .ok_or_else(|| PgError::InvalidState("a timed proposal recorded no phases".to_string()))?;
    Ok(TimedProposalOutcome {
        outcome: run.outcome,
        phases,
    })
}

/// What [`propose_against_pg_with_rejection_state`] returns: the outcome,
/// and on rejection only, the pre-state the kernel evaluated.
///
/// `#[must_use]` sits on this type because the caller receives it; the
/// inner outcome's attribute would not fire on a dropped wrapper.
#[must_use = "the proposal outcome must be inspected; a dropped `Rejected` silently treats a refused change as if it had committed"]
pub struct RejectionStateOutcome {
    pub outcome: PgProposalOutcome,
    pub rejection_state: Option<State>,
}

/// Everything one run yields; each public entry point hands out its part.
/// Phases are recorded only when asked for.
pub(crate) struct ProposalRun {
    outcome: PgProposalOutcome,
    rejection_state: Option<State>,
    phases: Option<ProposalPhases>,
}

/// A proposal's outcome with where its wall time went, from
/// [`propose_against_pg_timed`].
#[must_use = "the proposal outcome must be inspected; a dropped `Rejected` silently treats a refused change as if it had committed"]
pub struct TimedProposalOutcome {
    pub outcome: PgProposalOutcome,
    pub phases: ProposalPhases,
}

/// Where one proposal's wall time went: opening the transaction, loading
/// state, deciding, and persisting.
///
/// Phases depend on the route. Interpreted: `kernel` is the body and the
/// in-memory invariants, and `finalise` writes the delta and the record.
/// Compiled: `kernel` is the body, the delta write and the SQL checks, and
/// `finalise` the record alone. Compare routes by total time, never by
/// phase.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProposalPhases {
    pub begin: std::time::Duration,
    pub load: std::time::Duration,
    pub kernel: std::time::Duration,
    pub finalise: std::time::Duration,
}

/// [`propose_against_pg`], also returning the pre-state the kernel
/// evaluated when the outcome is a rejection (`None` on commit).
///
/// An explanation must describe the snapshot that refused; a separate
/// explain call would read a second snapshot that may differ.
///
/// Always the interpreted route: the explanation needs the invariants'
/// state, which the compiled route never loads.
pub async fn propose_against_pg_with_rejection_state(
    pool: &PgPool,
    program: &PgProgram,
    proposal: &Proposal,
) -> Result<RejectionStateOutcome, PgError> {
    let (transformation, admission) =
        resolve_admission(program.core(), &proposal.transformation_name)?;
    let transition = proposal.transition();
    let run = propose_against_pg_run(
        pool,
        Route::Interpreted,
        transformation,
        &transition,
        &admission,
        false,
    )
    .await?;
    Ok(RejectionStateOutcome {
        outcome: run.outcome,
        rejection_state: run.rejection_state,
    })
}

pub(crate) async fn propose_against_pg_run(
    pool: &PgPool,
    route: Route<'_>,
    transformation: &Transformation,
    transition: &Transition,
    admission: &Admission<'_>,
    timed: bool,
) -> Result<ProposalRun, PgError> {
    let invariants = admission.invariants;
    let definitions = admission.definitions;
    let clock = timed.then(std::time::Instant::now);
    let elapsed = |clock: Option<std::time::Instant>| {
        clock.map_or(std::time::Duration::ZERO, |c| c.elapsed())
    };
    let (mut tx, login_role) = begin_authorised_proposal_tx(pool, &transition.actor).await?;
    let begin = elapsed(clock);

    let scope = compute_load_scope(
        transformation,
        Some(transition),
        invariants,
        definitions,
        route.reads(),
    );
    let state = load_state(&mut tx, &scope).await?;
    let load = elapsed(clock) - begin;

    let (decided, rejection_state) = match route {
        Route::Interpreted => {
            let outcome = propose_with(transformation, transition, &state, admission)?;
            let rejection_state = matches!(outcome, Outcome::Rejected { .. }).then_some(state);
            (Decided::Kernel(outcome), rejection_state)
        }
        Route::Compiled(set) => {
            let staged = propose_stage_delta(transformation, transition, &state, definitions)?;
            match staged {
                StagedDelta::Rejected { reason } => {
                    (Decided::Kernel(Outcome::Rejected { reason }), Some(state))
                }
                StagedDelta::Staged {
                    asserted,
                    retracted,
                    emitted,
                } => {
                    let transition_id = Uuid::now_v7();
                    // Check only what the database says the delta changed.
                    let effective =
                        write_claim_delta(&mut tx, transition_id, &asserted, &retracted).await?;
                    disable_jit(&mut tx).await?;
                    let steps = [DeltaStep {
                        transition_id,
                        asserted: asserted.clone(),
                    }];
                    let violation = set
                        .first_violation(
                            &mut tx,
                            Stage::CaseBound,
                            &effective.asserted,
                            &effective.retracted,
                            &steps,
                        )
                        .await?;
                    match violation {
                        Some(v) => (
                            Decided::Kernel(Outcome::Rejected { reason: v.into() }),
                            None,
                        ),
                        None => (
                            Decided::Checked {
                                transition_id,
                                asserted,
                                retracted,
                                emitted,
                            },
                            None,
                        ),
                    }
                }
            }
        }
    };
    let kernel = elapsed(clock) - begin - load;

    let pg_outcome = match decided {
        Decided::Kernel(outcome) => {
            finalise_outcome(
                pool,
                tx,
                transformation,
                transition,
                invariants,
                outcome,
                &login_role,
            )
            .await?
        }
        Decided::Checked {
            transition_id,
            asserted,
            retracted,
            emitted,
        } => {
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
            tx.commit().await.map_err(classify_commit)?;
            PgProposalOutcome::Committed {
                transition_id,
                actor: transition.actor.clone(),
                asserted_claims: asserted,
                retracted_claims: retracted,
                emitted_intents: emitted,
            }
        }
    };
    let finalise = elapsed(clock) - begin - load - kernel;
    Ok(ProposalRun {
        outcome: pg_outcome,
        rejection_state,
        phases: timed.then_some(ProposalPhases {
            begin,
            load,
            kernel,
            finalise,
        }),
    })
}

/// What the deciding phase settled: a kernel outcome still to persist, or
/// a delta the compiled checks already admitted, owing only the record
/// and the commit.
enum Decided {
    Kernel(Outcome),
    Checked {
        transition_id: Uuid,
        asserted: Vec<ClaimInstance>,
        retracted: Vec<ClaimInstance>,
        emitted: Vec<IntentInstance>,
    },
}

/// What [`propose_against_pg_with_trace`] returns for kernel-side results;
/// PG-layer errors arrive as `Err`.
///
/// `KernelErrored` keeps the trace up to the error, where it matters most
/// for debugging.
#[must_use = "a traced proposal outcome carries the commit/reject result (a dropped `Rejected` silently treats a refused change as committed) and the diagnostic trace"]
#[derive(Debug, Clone)]
pub enum PgTracedOutcome {
    /// The kernel committed or rejected, and persistence succeeded.
    Outcome {
        outcome: PgProposalOutcome,
        trace: Vec<TraceEntry>,
    },
    /// The kernel raised an [`EvalError`]. The transaction was rolled
    /// back; `trace` holds every statement that ran before the error.
    KernelErrored {
        error: EvalError,
        trace: Vec<TraceEntry>,
    },
}

/// [`propose_against_pg`] plus a per-statement diagnostic trace.
///
/// - **Committed** / **Rejected**: `Ok(PgTracedOutcome::Outcome { .. })`.
///   A rejection is logged as on the untraced path.
/// - **Kernel error**: `Ok(PgTracedOutcome::KernelErrored { .. })`, after
///   rolling back.
/// - **PG-layer error**: `Err(PgError)`, with no trace.
pub async fn propose_against_pg_with_trace(
    pool: &PgPool,
    program: &PgProgram,
    proposal: &Proposal,
) -> Result<PgTracedOutcome, PgError> {
    let (transformation, invariants, definitions) =
        resolve(program.core(), &proposal.transformation_name)?;
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
    let (mut tx, login_role) = begin_authorised_proposal_tx(pool, &transition.actor).await?;

    // Always interpreted, so the trace shows the specification's own steps.
    let scope = compute_load_scope(
        transformation,
        Some(transition),
        invariants,
        definitions,
        Reads::BodyAndInvariants,
    );
    let state = load_state(&mut tx, &scope).await?;
    let traced = propose_with_trace(transformation, transition, &state, invariants, definitions);
    match traced {
        TracedProposal::Completed { outcome, trace } => {
            let outcome = finalise_outcome(
                pool,
                tx,
                transformation,
                transition,
                invariants,
                outcome,
                &login_role,
            )
            .await?;
            Ok(PgTracedOutcome::Outcome { outcome, trace })
        }
        TracedProposal::Errored { error, trace } => {
            // Explicit, so a rollback failure surfaces as an error.
            tx.rollback().await.map_err(classify)?;
            Ok(PgTracedOutcome::KernelErrored { error, trace })
        }
    }
}

/// A refusal as a caller reports it.
pub(crate) struct Refusal {
    pub(crate) reason: String,
    pub(crate) rule: Option<String>,
    pub(crate) witness: Vec<WitnessBinding>,
}

/// Roll back, then record the refusal in `morpholog.rejections`. Every
/// refusing path records here, once.
///
/// The record is a separate autocommit insert on `pool`, because the
/// refusing transaction rolls back. A crash in between loses it: the log
/// is operational evidence, and audit stays the record that counts. A
/// failed insert is an error, never a rejected envelope.
pub(crate) async fn record_refusal(
    pool: &PgPool,
    tx: Transaction<'_, Postgres>,
    transformation: &Transformation,
    transition: &Transition,
    reason: &RejectionReason,
) -> Result<Refusal, PgError> {
    tx.rollback().await.map_err(classify)?;
    write_rejection(pool, transformation, transition, reason)
        .await
        .map_err(|e| PgError::RejectionLogFailure(Box::new(e)))?;
    let witness = match reason {
        RejectionReason::Invariant { witness, .. } => witness.clone(),
        RejectionReason::Require { .. } | RejectionReason::BindNone { .. } => Vec::new(),
    };
    Ok(Refusal {
        reason: reason.to_string(),
        rule: rule_identity(reason),
        witness,
    })
}

/// Persist a kernel [`Outcome`]: commit it, or roll back and log the
/// refusal.
pub(crate) async fn finalise_outcome(
    pool: &PgPool,
    mut tx: Transaction<'_, Postgres>,
    transformation: &Transformation,
    transition: &Transition,
    invariants: &[Invariant],
    outcome: Outcome,
    login_role: &LoginRole,
) -> Result<PgProposalOutcome, PgError> {
    match outcome {
        Outcome::Rejected { reason } => {
            let Refusal {
                reason,
                rule,
                witness,
            } = record_refusal(pool, tx, transformation, transition, &reason).await?;
            Ok(PgProposalOutcome::Rejected {
                reason,
                rule,
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
                login_role,
            )
            .await?;
            tx.commit().await.map_err(classify_commit)?;
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

/// Load the pre-state for a proposal: every row of every predicate in
/// `scope`, or for a keyed predicate only the rows matching one of its
/// patterns' coordinates. Nothing else can affect the proposal. An empty
/// scope returns an empty state without a query.
///
/// One statement, each predicate its own branch with the predicate
/// spelled as a literal, so the partial indexes' predicates are provable;
/// a coordinate seeks by the digest of its key, what those indexes hold,
/// against the bound value's key through the same function. A seek that
/// finds nothing still locks the index range it searched, which is how
/// SERIALIZABLE protects the absence.
pub(crate) async fn load_state(
    tx: &mut Transaction<'_, Postgres>,
    scope: &LoadScope,
) -> Result<State, PgError> {
    if scope.filters.is_empty() {
        return Ok(State::default());
    }
    let (sql, binds) = load_sql(scope)?;
    // Safe: the structure is rendered from the programme with the
    // predicate names quoted; every proposed value is a bound parameter.
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for value in binds {
        query = query.bind(value);
    }
    let rows = query.fetch_all(&mut **tx).await.map_err(classify)?;

    let mut claims = Vec::with_capacity(rows.len());
    for row in rows {
        let predicate: String = row.try_get("predicate_name").map_err(classify)?;
        let arguments: serde_json::Value = row.try_get("arguments").map_err(classify)?;
        let args: Vec<EvalValue> = serde_json::from_value(arguments)?;
        claims.push(ClaimInstance {
            predicate: PredicateName::from(predicate),
            args,
        });
    }
    Ok(State::from_claims(claims))
}

/// The loader's statement and its bound values, apart so a plan gate can
/// explain what production runs.
pub(crate) fn load_sql(scope: &LoadScope) -> Result<(String, Vec<serde_json::Value>), PgError> {
    // One select per predicate, joined by UNION ALL, so the planner plans
    // each predicate's scan on its own: a keyed predicate seeks its
    // indexes whatever a whole one beside it costs, and each read's lock
    // footprint is its own.
    let mut selects = Vec::with_capacity(scope.filters.len());
    let mut binds: Vec<serde_json::Value> = Vec::new();
    for (predicate, filter) in &scope.filters {
        let literal = quote_literal(predicate.as_str());
        let condition = match filter {
            LoadFilter::Whole => format!("predicate_name = {literal}"),
            LoadFilter::Keyed(patterns) => {
                let mut disjuncts = Vec::with_capacity(patterns.len());
                for pattern in patterns {
                    let mut conjuncts = Vec::with_capacity(pattern.len());
                    for (position, value) in pattern {
                        binds.push(serde_json::to_value(value)?);
                        conjuncts.push(format!(
                            "{} = morpholog.claim_digest(morpholog.value_key_v1(${}::jsonb))",
                            crate::compiled::seek_expression("", *position),
                            binds.len()
                        ));
                    }
                    disjuncts.push(format!("({})", conjuncts.join(" AND ")));
                }
                format!(
                    "predicate_name = {literal} AND ({})",
                    disjuncts.join(" OR ")
                )
            }
        };
        selects.push(format!(
            "SELECT predicate_name, arguments, arguments_hash FROM morpholog.claims WHERE {condition}"
        ));
    }
    // Ordered because a refusal's witness is the first violating match;
    // an unordered scan could explain the same refusal differently between
    // runs. By the primary key, which the index already provides: ordering
    // by `asserted_at` forces a sort (measured ~1.8x propose latency at 20k
    // claims) and depends on history.
    let sql = format!(
        "SELECT predicate_name, arguments FROM (\n{}\n) AS scope\n ORDER BY predicate_name, arguments_hash",
        selects.join("\nUNION ALL\n")
    );
    Ok((sql, binds))
}

/// What a loaded state must serve. `Body`: the compiled route, where the
/// claims table serves the checks and reports the effective delta.
/// `BodyAndInvariants`: the interpreter, which evaluates invariants and
/// computes the effective delta from the state it holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reads {
    Body,
    BodyAndInvariants,
}

/// How much of one predicate a load fetches: every row, or the rows
/// matching any pattern's coordinates, each pattern a conjunction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LoadFilter {
    Whole,
    Keyed(Vec<Vec<(usize, EvalValue)>>),
}

impl LoadFilter {
    fn union(&mut self, other: LoadFilter) {
        match (&mut *self, other) {
            (LoadFilter::Whole, _) => {}
            (_, LoadFilter::Whole) => *self = LoadFilter::Whole,
            (LoadFilter::Keyed(mine), LoadFilter::Keyed(theirs)) => {
                for pattern in theirs {
                    if !mine.contains(&pattern) {
                        mine.push(pattern);
                    }
                }
            }
        }
    }
}

/// What `load_state` fetches for one execution, by predicate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct LoadScope {
    pub(crate) filters: BTreeMap<PredicateName, LoadFilter>,
}

impl LoadScope {
    fn add(&mut self, predicate: PredicateName, filter: LoadFilter) {
        match self.filters.entry(predicate) {
            std::collections::btree_map::Entry::Vacant(v) => {
                v.insert(filter);
            }
            std::collections::btree_map::Entry::Occupied(mut o) => o.get_mut().union(filter),
        }
    }

    /// Both scopes' rows: what a batch loads once for all its acts.
    pub(crate) fn union(&mut self, other: LoadScope) {
        for (predicate, filter) in other.filters {
            self.add(predicate, filter);
        }
    }

    /// The predicates in scope, whatever their filters.
    pub(crate) fn predicates(&self) -> Vec<PredicateName> {
        self.filters.keys().cloned().collect()
    }

    /// Whether a claim would be loaded: the same test in the kernel's
    /// terms, for holding the loader to the plan.
    #[cfg(test)]
    pub(crate) fn admits(&self, claim: &ClaimInstance) -> bool {
        match self.filters.get(&claim.predicate) {
            None => false,
            Some(LoadFilter::Whole) => true,
            Some(LoadFilter::Keyed(patterns)) => patterns.iter().any(|pattern| {
                pattern
                    .iter()
                    .all(|(position, value)| claim.args.get(*position) == Some(value))
            }),
        }
    }
}

/// What `load_state` must fetch for this transformation:
///
/// - every predicate the body reads, keyed by the coordinates its
///   patterns fix before the body runs (see [`morpholog_core::ReadPlan`]),
///   or whole when a pattern fixes none;
/// - with [`Reads::BodyAndInvariants`], also every predicate the
///   invariants reference, whole, and every predicate the body admits,
///   keyed the same way (the interpreter decides from its state whether
///   an admit changed anything).
///
/// Without a transition the coordinates cannot be resolved, so every
/// predicate is whole: the diagnostic reads. The promise is equivalence:
/// proposing against this projection behaves as against full state,
/// pinned by the scope differential.
pub(crate) fn compute_load_scope(
    transformation: &Transformation,
    transition: Option<&Transition>,
    invariants: &[Invariant],
    definitions: &[Definition],
    reads: Reads,
) -> LoadScope {
    let plan = ReadPlan::of(transformation, definitions);
    let resolve =
        |filter: &ReadFilter| match transition.and_then(|t| filter.resolve(transformation, t)) {
            Some(patterns) => LoadFilter::Keyed(patterns),
            None => LoadFilter::Whole,
        };
    let mut scope = LoadScope::default();
    for (predicate, filter) in &plan.reads {
        scope.add(predicate.clone(), resolve(filter));
    }
    if reads == Reads::BodyAndInvariants {
        for (predicate, filter) in &plan.admits {
            scope.add(predicate.clone(), resolve(filter));
        }
        let mut referenced = std::collections::BTreeSet::new();
        for inv in invariants {
            morpholog_core::predicates_referenced_by_prop(&inv.body, definitions, &mut referenced);
        }
        for predicate in referenced {
            scope.add(predicate, LoadFilter::Whole);
        }
    }
    scope
}

/// One entry in an audit row's `invariants_checked`: an active invariant
/// the transition was admitted under, with its `version` at the time.
///
/// Every active invariant is listed, whether it was discharged because the
/// delta could not affect it, because every affected case held, or by a
/// whole evaluation. Distinct from the kernel's transient
/// `TraceEntry::InvariantCheck`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditedInvariantCheck {
    pub name: InvariantName,
    pub version: u32,
}

/// The `kind` column's values, shared by writer and readers so they cannot
/// drift; the schema's CHECK constraint pins the same set.
pub(crate) const REJECTION_KIND_INVARIANT: &str = "invariant";

pub(crate) const REJECTION_KIND_REQUIRE: &str = "require";

pub(crate) const REJECTION_KIND_BIND: &str = "bind";

/// The refused rule's stable identifier, or `None` when it has none.
/// Taken from the variant, never parsed from the display text.
pub(crate) fn rule_identity(reason: &RejectionReason) -> Option<String> {
    match reason {
        RejectionReason::Invariant { name, .. } => Some(name.to_string()),
        RejectionReason::Require { name, .. } | RejectionReason::BindNone { name, .. } => {
            name.as_ref().map(ToString::to_string)
        }
    }
}

/// Record a refused proposal in `morpholog.rejections`, autocommit on the
/// pool, since the refusing transaction has rolled back (see
/// [`record_refusal`]). The kind, rule and version columns come from the
/// [`RejectionReason`] variant, never from parsing its display text.
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
        // A named gate stores its name, so refusals group by cause. An
        // unnamed one stores the rendered expression: unstable, but better
        // than nothing in an operational log.
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
    // NULL, not `[]`: "none captured", not "captured, empty".
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
    .map_err(classify_checked_query)?;
    Ok(())
}

/// Apply a delta to the claims table: deletes, then inserts. Split from
/// [`write_accepted`] so the compiled route can make the table the
/// candidate state before checking.
///
/// Returns the effective delta, with the table answering membership: a
/// delete that removed a row, or an insert that changed nothing, found
/// the claim present.
pub(crate) async fn write_claim_delta(
    tx: &mut Transaction<'_, Postgres>,
    transition_id: Uuid,
    asserted_claims: &[ClaimInstance],
    retracted_claims: &[ClaimInstance],
) -> Result<EffectiveDelta, PgError> {
    let mut present: HashSet<ClaimInstance> = HashSet::new();
    // Each distinct retraction must delete exactly one row; zero means
    // the table disagrees with the pre-state (SSI catches concurrent
    // interference later). The digest finds the row; the equality on the
    // array makes a digest collision retract nothing, not the wrong claim.
    let mut seen: HashSet<(PredicateName, String)> = HashSet::new();
    for claim in retracted_claims {
        let args_repr = serde_json::to_string(&claim.args)?;
        let key = (claim.predicate.clone(), args_repr);
        if !seen.insert(key) {
            continue;
        }
        let args_json: serde_json::Value = serde_json::to_value(&claim.args)?;
        let result = sqlx::query!(
            "DELETE FROM morpholog.claims
             WHERE predicate_name = $1
               AND arguments_hash = morpholog.claim_digest($2)
               AND arguments = $2",
            claim.predicate.as_str(),
            args_json,
        )
        .execute(&mut **tx)
        .await
        .map_err(classify_checked_query)?;
        if result.rows_affected() != 1 {
            return Err(PgError::InvalidState(format!(
                "expected exactly 1 row deleted for retraction of `{}`, got {}",
                claim.predicate,
                result.rows_affected()
            )));
        }
        present.insert(claim.clone());
    }

    // Claims are a set: asserting a present claim is a no-op.
    for claim in asserted_claims {
        let args_json: serde_json::Value = serde_json::to_value(&claim.args)?;
        let result = sqlx::query!(
            "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
             VALUES ($1, $2, $3)
             ON CONFLICT (predicate_name, arguments_hash) DO NOTHING",
            claim.predicate.as_str(),
            args_json,
            transition_id,
        )
        .execute(&mut **tx)
        .await
        .map_err(classify_checked_query)?;
        // A claim retracted by this delta was just re-inserted, so only
        // an unretracted claim reports pre-state presence here.
        if result.rows_affected() == 0 && !present.contains(claim) {
            present.insert(claim.clone());
        }
    }

    Ok(EffectiveDelta::of(asserted_claims, retracted_claims, |c| {
        present.contains(c)
    }))
}

/// Persist an accepted outcome whole: the claim delta, then the
/// acceptance record. The interpreted paths call this; the compiled
/// route writes the delta first, checks, then records.
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
    login_role: &LoginRole,
) -> Result<(), PgError> {
    let _ = write_claim_delta(tx, transition_id, asserted_claims, retracted_claims).await?;
    write_acceptance_record(
        tx,
        transition_id,
        transformation,
        transition,
        invariants,
        asserted_claims,
        retracted_claims,
        emitted_intents,
        login_role,
    )
    .await
}

/// The record of an admitted transition: the audit row and one outbox row
/// per emitted intent, written after every invariant is discharged.
/// `invariants_checked` lists all of the programme's invariants on either
/// route, so the audit leaf does not depend on the route.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn write_acceptance_record(
    tx: &mut Transaction<'_, Postgres>,
    transition_id: Uuid,
    transformation: &Transformation,
    transition: &Transition,
    invariants: &[Invariant],
    asserted_claims: &[ClaimInstance],
    retracted_claims: &[ClaimInstance],
    emitted_intents: &[IntentInstance],
    login_role: &LoginRole,
) -> Result<(), PgError> {
    let checked: Vec<AuditedInvariantCheck> = invariants
        .iter()
        .map(|inv| AuditedInvariantCheck {
            name: inv.name.clone(),
            version: inv.version,
        })
        .collect();
    // The login role that asserted the actor, read once when this
    // transaction opened and checked against the actor policy, so the
    // identity CHECKED and the identity RECORDED cannot differ.
    let attestation = AuditAttestation::Gateway {
        authenticated_by: login_role.name.clone(),
        authenticated_by_oid: Some(login_role.oid),
    };
    // The actor is stored as a tagged `EvalValue::Subject`; `actor_repr`
    // only applies when serialising through `Transition`.
    sqlx::query!(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents, attestation,
            parameters
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
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
        serde_json::to_value(
            transformation
                .parameters
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        )?,
    )
    .execute(&mut **tx)
    .await
    .map_err(classify_checked_query)?;

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
        .map_err(classify_checked_query)?;
    }

    Ok(())
}

/// Deterministic idempotency key for an emitted intent:
///
/// ```text
/// hex(sha256(transition_id_bytes ‖ 0x00 ‖ name_bytes ‖ 0x00 ‖ canonical_json(args)))
/// ```
///
/// `canonical_json` is `serde_json` output, stable because derived
/// `Serialize` fixes field order and there are no map-like values.
///
/// Unique per `(transition_id, intent.name, intent.args)`. It prevents
/// duplicate outbox rows under retry, not duplicate business events,
/// which need a key from the inbound request.
///
/// Two identical intents in one transformation share a key; the second
/// insert fails as [`PgError::DuplicateIntent`] and rolls the whole
/// transformation back, since such duplicates are almost always a bug.
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
    Ok(hex::encode(hasher.finalize()))
}
