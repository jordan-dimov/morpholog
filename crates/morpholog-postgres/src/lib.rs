//! Morpholog PostgreSQL persistence adapter.
//!
//! Thin async I/O layer around the existing synchronous
//! [`morpholog_core::propose`] kernel. The kernel itself is unchanged.
//!
//! See `crates/morpholog-core/sql/schema.sql` for the canonical schema
//! and `docs/scope-and-ambition.md` for the runtime's positioning.

pub mod testing;

use chrono::{DateTime, Utc};
use morpholog_core::{
    ClaimInstance, DerivedClaim, EvalError, EvalValue, IntentInstance, Invariant, InvariantName,
    Outcome, PredicateName, State, Subject, TraceEntry, TracedProposal, Transformation,
    TransformationName, Transition, enumerate_derived, predicates_referenced_by_derived, propose,
    propose_with_trace,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Re-export of `sqlx::PgPool` so downstream crates can take the
/// connection-pool type the public async functions use without
/// pulling `sqlx` in as a direct dependency.
pub use sqlx::PgPool;

/// Sentinel actor for transitions the runtime itself initiates, with
/// no user under whose authority the transition is being proposed.
/// Used by the outbox compensation path: when a delivery fails
/// non-retryably and a [`CompensationSpec`] is configured, the
/// compensating transformation is proposed by the runtime, not by
/// the actor of the original commit. The sentinel keeps the audit
/// row's `actor` column meaningfully populated.
pub fn system_actor() -> Subject {
    Subject::from("morpholog-system")
}

/// Errors returned by the PostgreSQL adapter.
///
/// Lawful business rejection is **not** an error - it is returned as
/// [`PgProposalOutcome::Rejected`]. This enum captures only conditions
/// where the caller cannot or should not proceed as if the kernel had
/// run successfully.
#[derive(thiserror::Error, Debug)]
pub enum PgError {
    /// SQLSTATE 40001 from PostgreSQL SSI. The transaction should be
    /// retried by the caller.
    #[error("SERIALIZABLE retry needed (SQLSTATE 40001)")]
    SerializationFailure,
    /// Evaluation error from the in-memory kernel (e.g. unbound variable,
    /// type mismatch). Distinct from a business [`PgProposalOutcome::Rejected`].
    #[error(transparent)]
    Kernel(#[from] EvalError),
    /// Any other database error (connection, schema mismatch, etc.).
    #[error(transparent)]
    Database(sqlx::Error),
    /// JSON serialisation or deserialisation error at the codec boundary.
    #[error(transparent)]
    Encoding(#[from] serde_json::Error),
    /// Persistent state does not match expectations (e.g. a retraction
    /// matched zero rows when exactly one was expected).
    #[error("invalid persistent state: {0}")]
    InvalidState(String),
    /// A supplied `transition_id` does not name an existing audit row.
    /// Returned by the as-of helpers when the caller asks for state at
    /// a coordinate that does not correspond to any committed
    /// transition. The contract is "exists or error": every unknown id
    /// - smaller, larger, or between known ids - is rejected here.
    #[error("transition_id {0} not found in morpholog.audit")]
    TransitionNotFound(Uuid),
    /// A transformation emitted the same intent (same name and args)
    /// more than once, so two outbox rows collided on the
    /// deterministic idempotency key (SQLSTATE 23505 on the outbox
    /// idempotency-key unique constraint). The whole transformation
    /// rolls back. Named distinctly from [`PgError::Database`] because
    /// it is a modelling bug, not a transient condition: identical
    /// duplicate intents must not silently produce two outbox rows.
    #[error(
        "transformation emitted a duplicate intent (same name and args); \
         outbox idempotency keys collided"
    )]
    DuplicateIntent,
    /// An `--as-of` timestamp earlier than every committed transition:
    /// there is no state to reconstruct at or before that instant.
    /// Distinct from [`PgError::TransitionNotFound`] because the caller
    /// supplied a time, not an id, and the remedy differs (pick a later
    /// instant vs fix a wrong id).
    #[error("no transition committed at or before {0}")]
    NoTransitionAtOrBefore(DateTime<Utc>),
}

/// The result of proposing a transformation against PostgreSQL.
///
/// On `Committed`, the database transaction has already been committed:
/// claims have been mutated, one audit row written, and one outbox row
/// per emitted intent. On `Rejected`, the transaction has been rolled
/// back and no governed state has changed.
///
/// `Serialize` uses serde's internally-tagged representation so the
/// CLI can emit outcomes directly as JSON with a `status` discriminant.
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
    },
}

/// Propose a transformation against the live `morpholog.*` tables.
///
/// Opens one PostgreSQL transaction at SERIALIZABLE isolation, loads
/// the current claims into an in-memory [`State`], calls the existing
/// synchronous [`propose`] kernel, then either commits the changes
/// (writing claims, audit, and outbox rows) or rolls back atomically.
///
/// External side effects do not run inside this transaction. Outbox rows
/// are enqueued for post-commit delivery by workers running outside.
///
/// The [`Transition`] bundles the transformation name (verified against
/// `transformation.name`), the arguments, and the actor under whose
/// authority the transition is proposed. On `Committed`, the actor is
/// persisted to the `morpholog.audit.actor` column.
pub async fn propose_against_pg(
    pool: &PgPool,
    transformation: &Transformation,
    transition: &Transition,
    invariants: &[Invariant],
) -> Result<PgProposalOutcome, PgError> {
    let mut tx = pool.begin().await.map_err(classify)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await
        .map_err(classify)?;

    let scope = compute_load_scope(transformation, invariants);
    let state = load_state(&mut tx, &scope).await?;
    let outcome = propose(transformation, transition, &state, invariants)?;
    finalise_outcome(tx, transformation, transition, invariants, outcome).await
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
///   `Ok(PgTracedOutcome::Outcome { outcome, trace })`.
/// - **Kernel error** (`EvalError` raised mid-transformation) -
///   `Ok(PgTracedOutcome::KernelErrored { error, trace })`. The
///   open SERIALIZABLE transaction is rolled back before returning.
/// - **PG-layer error** (`Database`, `SerializationFailure`,
///   `Encoding`, `InvalidState`) - `Err(PgError)`. These errors
///   happen outside the kernel call and have no kernel trace to
///   preserve.
pub async fn propose_against_pg_with_trace(
    pool: &PgPool,
    transformation: &Transformation,
    transition: &Transition,
    invariants: &[Invariant],
) -> Result<PgTracedOutcome, PgError> {
    let mut tx = pool.begin().await.map_err(classify)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await
        .map_err(classify)?;

    let scope = compute_load_scope(transformation, invariants);
    let state = load_state(&mut tx, &scope).await?;
    let traced = propose_with_trace(transformation, transition, &state, invariants);
    match traced {
        TracedProposal::Completed { outcome, trace } => {
            let outcome =
                finalise_outcome(tx, transformation, transition, invariants, outcome).await?;
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
async fn finalise_outcome(
    mut tx: Transaction<'_, Postgres>,
    transformation: &Transformation,
    transition: &Transition,
    invariants: &[Invariant],
    outcome: Outcome,
) -> Result<PgProposalOutcome, PgError> {
    match outcome {
        Outcome::Rejected { reason } => {
            tx.rollback().await.map_err(classify)?;
            Ok(PgProposalOutcome::Rejected { reason })
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

/// Is this SQLSTATE the PostgreSQL serialization-failure code
/// (`40001`) returned by SSI when a SERIALIZABLE transaction cannot be
/// linearised? Pure function so the magic string can be unit-tested
/// without mocking `sqlx::DatabaseError`.
fn is_serialization_failure_code(code: Option<&str>) -> bool {
    code == Some("40001")
}

/// Is this SQLSTATE the PostgreSQL `unique_violation` code (`23505`)?
fn is_unique_violation_code(code: Option<&str>) -> bool {
    code == Some("23505")
}

/// Maps a `sqlx::Error` to a [`PgError`], recognising SQLSTATE 40001
/// (PostgreSQL SSI serialization failure) as the distinct retryable
/// variant and a 23505 on the outbox idempotency-key constraint as
/// [`PgError::DuplicateIntent`]. All other errors propagate as
/// [`PgError::Database`].
fn classify(err: sqlx::Error) -> PgError {
    let db = err.as_database_error();
    let code = db.and_then(sqlx::error::DatabaseError::code);
    if is_serialization_failure_code(code.as_deref()) {
        return PgError::SerializationFailure;
    }
    if is_unique_violation_code(code.as_deref())
        && db
            .and_then(|e| e.constraint())
            .is_some_and(|c| c.contains("idempotency_key"))
    {
        return PgError::DuplicateIntent;
    }
    PgError::Database(err)
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
async fn load_state(
    tx: &mut Transaction<'_, Postgres>,
    scope: &[PredicateName],
) -> Result<State, PgError> {
    if scope.is_empty() {
        return Ok(State::default());
    }

    // PredicateName is opaque to sqlx; bind the names as text for the
    // `predicate_name` text column's `ANY(...)` filter.
    let scope: Vec<&str> = scope.iter().map(PredicateName::as_str).collect();
    let rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT predicate_name, arguments
         FROM morpholog.claims
         WHERE predicate_name = ANY($1)",
    )
    .bind(&scope)
    .fetch_all(&mut **tx)
    .await
    .map_err(classify)?;

    let mut claims = Vec::with_capacity(rows.len());
    for (predicate, args_json) in rows {
        let args: Vec<EvalValue> = serde_json::from_value(args_json)?;
        claims.push(ClaimInstance {
            predicate: PredicateName::from(predicate),
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
fn compute_load_scope(
    transformation: &Transformation,
    invariants: &[Invariant],
) -> Vec<PredicateName> {
    let mut scope = std::collections::BTreeSet::new();
    for stmt in &transformation.body {
        morpholog_core::predicates_read_by_stmt(stmt, &mut scope);
    }
    for inv in invariants {
        morpholog_core::predicates_referenced_by_prop(&inv.body, &mut scope);
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

#[allow(clippy::too_many_arguments)]
async fn write_accepted(
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
        let result = sqlx::query(
            "DELETE FROM morpholog.claims WHERE predicate_name = $1 AND arguments = $2",
        )
        .bind(claim.predicate.as_str())
        .bind(&args_json)
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
        sqlx::query(
            "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
             VALUES ($1, $2, $3)
             ON CONFLICT (predicate_name, arguments) DO NOTHING",
        )
        .bind(claim.predicate.as_str())
        .bind(&args_json)
        .bind(transition_id)
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
    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(transition_id)
    .bind(transformation.name.as_str())
    .bind(serde_json::to_value(&transition.args)?)
    // Serialise via the tagged `EvalValue::Subject` so the `actor` column
    // keeps its v0 shape (`#[serde(with = "actor_repr")]` does not apply
    // when the field is serialised directly, only through `Transition`).
    .bind(serde_json::to_value(EvalValue::Subject(
        transition.actor.clone(),
    ))?)
    .bind(1_i32)
    .bind(serde_json::to_value(&checked)?)
    .bind(serde_json::to_value(asserted_claims)?)
    .bind(serde_json::to_value(retracted_claims)?)
    .bind(serde_json::to_value(emitted_intents)?)
    .execute(&mut **tx)
    .await
    .map_err(classify)?;

    // Outbox rows, one per emitted intent.
    for intent in emitted_intents {
        let intent_id = Uuid::now_v7();
        let idempotency_key = compute_idempotency_key(transition_id, intent)?;
        let args_json: serde_json::Value = serde_json::to_value(&intent.args)?;
        sqlx::query(
            "INSERT INTO morpholog.outbox (
                intent_id, transition_id, intent_type, arguments, idempotency_key
             ) VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(intent_id)
        .bind(transition_id)
        .bind(intent.name.as_str())
        .bind(&args_json)
        .bind(&idempotency_key)
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

/// One row of `morpholog.audit` decoded into typed runtime values.
///
/// Each row corresponds to exactly one committed transformation. The
/// JSONB columns are decoded through the same codec that wrote them,
/// so the round-trip is exact for any value the kernel can represent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditRow {
    pub transition_id: Uuid,
    pub transformation_name: TransformationName,
    pub arguments: Vec<EvalValue>,
    #[serde(with = "morpholog_core::actor_repr")]
    pub actor: Subject,
    pub invariant_epoch: i32,
    pub invariants_checked: Vec<AuditedInvariantCheck>,
    pub asserted_claims: Vec<ClaimInstance>,
    pub retracted_claims: Vec<ClaimInstance>,
    pub emitted_intents: Vec<IntentInstance>,
    pub committed_at: DateTime<Utc>,
}

/// One row of `morpholog.outbox` decoded into typed runtime values.
///
/// Carries every column on the table. The delivery-state extensions are
/// nullable in the schema and `Option<T>` here; they fill in as a row
/// moves through the delivery state machine. A `pending` row with
/// `attempt_count > 0` and a non-NULL `last_attempt_at` is one a worker
/// has tried and failed transiently, not a fresh enqueue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OutboxRow {
    pub intent_id: Uuid,
    pub transition_id: Uuid,
    pub intent_type: String,
    pub arguments: Vec<EvalValue>,
    pub idempotency_key: String,
    pub status: String,
    pub attempt_count: i32,
    pub enqueued_at: DateTime<Utc>,
    pub last_attempt_at: Option<DateTime<Utc>>,
    pub delivered_at: Option<DateTime<Utc>>,
    pub failed_at: Option<DateTime<Utc>>,
    pub failure_reason: Option<String>,
    pub next_attempt_at: Option<DateTime<Utc>>,
    pub compensation_transition_id: Option<Uuid>,
    pub locked_by: Option<String>,
    pub lock_expires_at: Option<DateTime<Utc>>,
}

/// Outcome of a state-mutating helper on a leased outbox row.
///
/// A worker that does not hold the current lease (expired and taken
/// over, or wrong `worker_id`) cannot clobber the row's state. Lease
/// loss is a normal operational condition, not an error, so the caller
/// sees [`OutboxUpdate::LeaseLost`] and can log, retry-after-reclaim,
/// or move on.
#[doc(hidden)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum OutboxUpdate {
    /// The row was updated as requested.
    Applied,
    /// The lease was no longer held by the supplied `worker_id`
    /// (expired, released, or never held). No change was made.
    LeaseLost,
}

/// Return every currently-admitted claim from `morpholog.claims`.
///
/// Order is `(asserted_at, predicate_name, arguments::text)`: causal
/// admission order with predicate-then-args as the stable tie-break,
/// so the result is deterministic across runs.
///
/// A `SELECT *` over the entire table, intended for tests, demos, and
/// small-state inspection; large states should query SQL directly.
pub async fn list_claims(pool: &PgPool) -> Result<Vec<ClaimInstance>, PgError> {
    let rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT predicate_name, arguments
         FROM morpholog.claims
         ORDER BY asserted_at, predicate_name, arguments::text",
    )
    .fetch_all(pool)
    .await
    .map_err(classify)?;

    decode_claim_rows(rows)
}

/// Decode `(predicate_name, arguments)` rows into `ClaimInstance`s -
/// the shared tail of the current-claims listings.
fn decode_claim_rows(
    rows: Vec<(String, serde_json::Value)>,
) -> Result<Vec<ClaimInstance>, PgError> {
    rows.into_iter()
        .map(|(predicate, args_json)| {
            Ok(ClaimInstance {
                predicate: PredicateName::from(predicate),
                args: serde_json::from_value(args_json)?,
            })
        })
        .collect()
}

/// Return every currently-admitted claim whose `predicate_name` is in
/// `predicates`. Empty `predicates` short-circuits to `Ok(vec![])`
/// without a query: an empty footprint is meaningful (e.g. a derived
/// claim whose domain is a no-op), not an error.
///
/// Used by [`list_derived`] to load only the claims a derived claim's
/// footprint references, avoiding the rest of `morpholog.claims`.
///
/// Order matches [`list_claims`].
pub async fn list_claims_for_predicates(
    pool: &PgPool,
    predicates: &[String],
) -> Result<Vec<ClaimInstance>, PgError> {
    if predicates.is_empty() {
        return Ok(Vec::new());
    }

    let rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT predicate_name, arguments
         FROM morpholog.claims
         WHERE predicate_name = ANY($1)
         ORDER BY asserted_at, predicate_name, arguments::text",
    )
    .bind(predicates)
    .fetch_all(pool)
    .await
    .map_err(classify)?;

    decode_claim_rows(rows)
}

/// Load the current scoped pre-state a transformation would see, the
/// read-only counterpart of the load inside [`propose_against_pg`].
/// Scopes to exactly the predicates the transformation body reads and
/// the invariants reference (see `compute_load_scope`); claims outside
/// that scope cannot affect the verdict, so they are not fetched.
///
/// Unlike `propose_against_pg`, this issues a plain pooled read, not a
/// SERIALIZABLE transaction: the caller is explaining what *would*
/// happen, not committing a decision, so the right semantics is a
/// point-in-time snapshot, not a serialization point.
///
/// Used by `morpholog explain` to run the kernel in-memory against live
/// state without opening a write transaction.
pub async fn load_scoped_state(
    pool: &PgPool,
    transformation: &Transformation,
    invariants: &[Invariant],
) -> Result<State, PgError> {
    let scope: Vec<String> = compute_load_scope(transformation, invariants)
        .into_iter()
        .map(|p| p.to_string())
        .collect();
    let claims = list_claims_for_predicates(pool, &scope).await?;
    Ok(State::from_claims(claims))
}

/// Return every committed audit row from `morpholog.audit`, ordered by
/// `(committed_at, transition_id)`: causal commit order with the
/// time-ordered UUIDv7 PRIMARY KEY as the stable tie-break.
///
/// JSONB columns are decoded through the codec into typed values. A
/// decoding error surfaces as [`PgError::Encoding`]; against a database
/// the runtime itself wrote, that indicates corruption or tampering.
pub async fn list_audit_rows(pool: &PgPool) -> Result<Vec<AuditRow>, PgError> {
    type Row = (
        Uuid,
        String,
        serde_json::Value,
        serde_json::Value,
        i32,
        serde_json::Value,
        serde_json::Value,
        serde_json::Value,
        serde_json::Value,
        DateTime<Utc>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT transition_id, transformation_name, arguments, actor,
                invariant_epoch, invariants_checked,
                asserted_claims, retracted_claims, emitted_intents,
                committed_at
         FROM morpholog.audit
         ORDER BY committed_at, transition_id",
    )
    .fetch_all(pool)
    .await
    .map_err(classify)?;

    rows.into_iter()
        .map(
            |(
                transition_id,
                transformation_name,
                args_json,
                actor_json,
                invariant_epoch,
                invariants_checked_json,
                asserted_json,
                retracted_json,
                intents_json,
                committed_at,
            )| {
                Ok(AuditRow {
                    transition_id,
                    transformation_name: TransformationName::from(transformation_name),
                    arguments: serde_json::from_value(args_json)?,
                    // Decode the tagged actor JSON and extract the subject,
                    // erroring at this boundary if the column somehow holds a
                    // non-subject value.
                    actor: match serde_json::from_value::<EvalValue>(actor_json)? {
                        EvalValue::Subject(s) => s,
                        other => {
                            return Err(PgError::InvalidState(format!(
                                "audit actor is not a subject: {other:?}"
                            )));
                        }
                    },
                    invariant_epoch,
                    invariants_checked: serde_json::from_value(invariants_checked_json)?,
                    asserted_claims: serde_json::from_value(asserted_json)?,
                    retracted_claims: serde_json::from_value(retracted_json)?,
                    emitted_intents: serde_json::from_value(intents_json)?,
                    committed_at,
                })
            },
        )
        .collect()
}

/// Return outbox rows whose `status = 'pending'`, ordered by
/// `(enqueued_at, intent_id)`. The "what does the worker have to
/// deliver?" query.
///
/// For other statuses or intent-type filtering, use [`list_outbox_rows`].
pub async fn list_pending_outbox(pool: &PgPool) -> Result<Vec<OutboxRow>, PgError> {
    let rows: Vec<OutboxRowRaw> = sqlx::query_as(OUTBOX_SELECT_ALL_COLUMNS)
        .bind("pending")
        .fetch_all(pool)
        .await
        .map_err(classify)?;
    rows.into_iter().map(decode_outbox_row).collect()
}

/// Return outbox rows filtered by status and/or intent type. Both
/// filters are optional: `None` drops that predicate entirely (any
/// status, including the worker's internal compensation states; any
/// intent type). Order matches [`list_pending_outbox`].
///
/// Lets a reader ask "what failed?" or "what is in flight?" without
/// custom SQL; used by `morpholog inspect outbox` for non-pending rows.
pub async fn list_outbox_rows(
    pool: &PgPool,
    status_filter: Option<&str>,
    intent_type_filter: Option<&str>,
) -> Result<Vec<OutboxRow>, PgError> {
    // `sqlx::query_as` has no optional bind parameters, so each filter
    // combination is a distinct statement.
    let rows: Vec<OutboxRowRaw> = match (status_filter, intent_type_filter) {
        (Some(status), Some(intent_type)) => sqlx::query_as(
            "SELECT intent_id, transition_id, intent_type, arguments,
                    idempotency_key, status, attempt_count, enqueued_at,
                    last_attempt_at, delivered_at, failed_at, failure_reason,
                    next_attempt_at, compensation_transition_id, locked_by,
                    lock_expires_at
             FROM morpholog.outbox
             WHERE status = $1 AND intent_type = $2
             ORDER BY enqueued_at, intent_id",
        )
        .bind(status)
        .bind(intent_type)
        .fetch_all(pool)
        .await
        .map_err(classify)?,
        (Some(status), None) => sqlx::query_as(OUTBOX_SELECT_ALL_COLUMNS)
            .bind(status)
            .fetch_all(pool)
            .await
            .map_err(classify)?,
        (None, Some(intent_type)) => sqlx::query_as(
            "SELECT intent_id, transition_id, intent_type, arguments,
                    idempotency_key, status, attempt_count, enqueued_at,
                    last_attempt_at, delivered_at, failed_at, failure_reason,
                    next_attempt_at, compensation_transition_id, locked_by,
                    lock_expires_at
             FROM morpholog.outbox
             WHERE intent_type = $1
             ORDER BY enqueued_at, intent_id",
        )
        .bind(intent_type)
        .fetch_all(pool)
        .await
        .map_err(classify)?,
        (None, None) => sqlx::query_as(
            "SELECT intent_id, transition_id, intent_type, arguments,
                    idempotency_key, status, attempt_count, enqueued_at,
                    last_attempt_at, delivered_at, failed_at, failure_reason,
                    next_attempt_at, compensation_transition_id, locked_by,
                    lock_expires_at
             FROM morpholog.outbox
             ORDER BY enqueued_at, intent_id",
        )
        .fetch_all(pool)
        .await
        .map_err(classify)?,
    };
    rows.into_iter().map(decode_outbox_row).collect()
}

/// Single source of truth for the outbox column list so the
/// `OutboxRowRaw` tuple, `decode_outbox_row`, and the SQL evolve
/// together.
const OUTBOX_SELECT_ALL_COLUMNS: &str = "SELECT intent_id, transition_id, intent_type, arguments,
            idempotency_key, status, attempt_count, enqueued_at,
            last_attempt_at, delivered_at, failed_at, failure_reason,
            next_attempt_at, compensation_transition_id, locked_by,
            lock_expires_at
     FROM morpholog.outbox
     WHERE status = $1
     ORDER BY enqueued_at, intent_id";

type OutboxRowRaw = (
    Uuid,                  // intent_id
    Uuid,                  // transition_id
    String,                // intent_type
    serde_json::Value,     // arguments
    String,                // idempotency_key
    String,                // status
    i32,                   // attempt_count
    DateTime<Utc>,         // enqueued_at
    Option<DateTime<Utc>>, // last_attempt_at
    Option<DateTime<Utc>>, // delivered_at
    Option<DateTime<Utc>>, // failed_at
    Option<String>,        // failure_reason
    Option<DateTime<Utc>>, // next_attempt_at
    Option<Uuid>,          // compensation_transition_id
    Option<String>,        // locked_by
    Option<DateTime<Utc>>, // lock_expires_at
);

fn decode_outbox_row(row: OutboxRowRaw) -> Result<OutboxRow, PgError> {
    let (
        intent_id,
        transition_id,
        intent_type,
        args_json,
        idempotency_key,
        status,
        attempt_count,
        enqueued_at,
        last_attempt_at,
        delivered_at,
        failed_at,
        failure_reason,
        next_attempt_at,
        compensation_transition_id,
        locked_by,
        lock_expires_at,
    ) = row;
    Ok(OutboxRow {
        intent_id,
        transition_id,
        intent_type,
        arguments: serde_json::from_value(args_json)?,
        idempotency_key,
        status,
        attempt_count,
        enqueued_at,
        last_attempt_at,
        delivered_at,
        failed_at,
        failure_reason,
        next_attempt_at,
        compensation_transition_id,
        locked_by,
        lock_expires_at,
    })
}

// ===========================================================================
// Outbox delivery-state mutators
// ===========================================================================
//
// Helpers that move an outbox row through the delivery state machine.
// All `mark_*` helpers gate on the worker holding a valid lease
// (`locked_by = worker_id AND lock_expires_at > now()`) and return
// `OutboxUpdate::LeaseLost` if another worker has taken the lease over.
// `record_compensation` errors instead of returning `LeaseLost`,
// because recording compensation against a non-failed or
// already-compensated row is a programming bug, not an operational
// condition.

/// Mark a successfully-delivered outbox row.
///
/// Transitions `status` to `'delivered'`, sets `delivered_at = now()`,
/// increments `attempt_count`, and clears the lease fields so the row
/// is unambiguously done. Returns `Applied`, or `LeaseLost` if the
/// worker no longer holds the lease.
///
/// Internal substrate of [`process_one_outbox_row`]; use that unless
/// driving the state machine manually.
#[doc(hidden)]
pub async fn mark_outbox_delivered(
    pool: &PgPool,
    intent_id: Uuid,
    worker_id: &str,
) -> Result<OutboxUpdate, PgError> {
    let rows = sqlx::query(
        "UPDATE morpholog.outbox
         SET status='delivered',
             delivered_at=now(),
             attempt_count=attempt_count+1,
             locked_by=NULL,
             lock_expires_at=NULL
         WHERE intent_id=$1
           AND locked_by=$2
           AND lock_expires_at > now()",
    )
    .bind(intent_id)
    .bind(worker_id)
    .execute(pool)
    .await
    .map_err(classify)?;
    Ok(if rows.rows_affected() == 1 {
        OutboxUpdate::Applied
    } else {
        OutboxUpdate::LeaseLost
    })
}

/// Record a transient delivery failure: schedule the row for retry
/// at `next_attempt_at`. The row goes back to `status='pending'`
/// (released from its lease) so another worker can pick it up at
/// the scheduled time, or the same worker can on its next claim.
///
/// `next_attempt_at` is a wall-clock instant the caller computes
/// (current time plus retry-after plus jitter); the row stays
/// invisible to claims until that moment.
///
/// **No upfront validation of `next_attempt_at`**: a past or
/// equal-now retry instant is accepted. Re-claim protection lives at
/// [`claim_pending_outbox_row`]'s `claim_before` bound, not here.
/// Validating here would conflict with the helper contract (lease loss
/// surfaces as [`OutboxUpdate::LeaseLost`], not [`PgError`]) and would
/// spuriously fail a slow legitimate delivery whose retry instant
/// elapses in transit.
///
/// Internal substrate of [`process_one_outbox_row`]; use that unless
/// driving the state machine manually.
#[doc(hidden)]
pub async fn mark_outbox_transient_attempt(
    pool: &PgPool,
    intent_id: Uuid,
    worker_id: &str,
    next_attempt_at: DateTime<Utc>,
) -> Result<OutboxUpdate, PgError> {
    let rows = sqlx::query(
        "UPDATE morpholog.outbox
         SET status='pending',
             attempt_count=attempt_count+1,
             last_attempt_at=now(),
             next_attempt_at=$3,
             locked_by=NULL,
             lock_expires_at=NULL
         WHERE intent_id=$1
           AND locked_by=$2
           AND lock_expires_at > now()",
    )
    .bind(intent_id)
    .bind(worker_id)
    .bind(next_attempt_at)
    .execute(pool)
    .await
    .map_err(classify)?;
    Ok(if rows.rows_affected() == 1 {
        OutboxUpdate::Applied
    } else {
        OutboxUpdate::LeaseLost
    })
}

/// Mark a non-retryable delivery failure. The row moves to
/// `status='failed'`, captures `failed_at` and `failure_reason`,
/// and releases its lease. A compensating transformation can then
/// be invoked and recorded via [`record_compensation`].
///
/// Internal substrate of [`process_one_outbox_row`]; use that unless
/// driving the state machine manually.
#[doc(hidden)]
pub async fn mark_outbox_failed(
    pool: &PgPool,
    intent_id: Uuid,
    worker_id: &str,
    reason: &str,
) -> Result<OutboxUpdate, PgError> {
    let rows = sqlx::query(
        "UPDATE morpholog.outbox
         SET status='failed',
             failed_at=now(),
             failure_reason=$3,
             attempt_count=attempt_count+1,
             last_attempt_at=now(),
             locked_by=NULL,
             lock_expires_at=NULL
         WHERE intent_id=$1
           AND locked_by=$2
           AND lock_expires_at > now()",
    )
    .bind(intent_id)
    .bind(worker_id)
    .bind(reason)
    .execute(pool)
    .await
    .map_err(classify)?;
    Ok(if rows.rows_affected() == 1 {
        OutboxUpdate::Applied
    } else {
        OutboxUpdate::LeaseLost
    })
}

/// Link a compensating transformation to a failed outbox row.
///
/// Gated by two SQL `WHERE` preconditions: the row must be
/// `status='failed'` and must not already carry a
/// `compensation_transition_id`. Violating either is a programming bug
/// and surfaces as [`PgError::InvalidState`], not a silent no-op.
///
/// `compensation_transition_id` must reference a row in
/// `morpholog.audit` (foreign-key-enforced); the worker invokes the
/// compensating transformation via [`propose_against_pg`] and passes
/// the resulting `transition_id` here.
///
/// Does NOT gate on a lease: by the time compensation is recorded the
/// row is in `failed` and the lease was already released by
/// [`mark_outbox_failed`].
///
/// **This is a lineage setter, not a duplicate-invocation guard.** The
/// `compensation_transition_id IS NULL` predicate only stops a second
/// *record* call from overwriting the first; it does not stop a second
/// *compensating transformation* from committing via
/// [`propose_against_pg`] first. If two workers race the same `failed`
/// row, both can commit independent compensations - only the second
/// `record_compensation` fails, by which point a duplicate is already
/// in `morpholog.audit`.
///
/// Preventing that is the caller's responsibility: either retain lease
/// ownership across the failed -> commit -> record arc, or guard the
/// compensating transformation with an `original_intent_id` invariant.
/// See `docs/outbox-sketch.md`.
///
/// Internal substrate of [`process_one_outbox_row`]; use that unless
/// driving the state machine manually.
#[doc(hidden)]
pub async fn record_compensation(
    pool: &PgPool,
    intent_id: Uuid,
    compensation_transition_id: Uuid,
) -> Result<(), PgError> {
    let rows = sqlx::query(
        "UPDATE morpholog.outbox
         SET compensation_transition_id=$2
         WHERE intent_id=$1
           AND status='failed'
           AND compensation_transition_id IS NULL",
    )
    .bind(intent_id)
    .bind(compensation_transition_id)
    .execute(pool)
    .await
    .map_err(classify)?;
    if rows.rows_affected() == 1 {
        Ok(())
    } else {
        Err(PgError::InvalidState(format!(
            "record_compensation({intent_id}): 0 rows matched. The outbox \
             row was either not found, not in status='failed', or already \
             carries a compensation_transition_id."
        )))
    }
}

/// Atomically claim one due-pending (or expired-leased) outbox row
/// of the given `intent_type` for delivery by `worker_id`.
///
/// The row is selected with `FOR UPDATE SKIP LOCKED` inside an
/// `UPDATE ... RETURNING` so concurrent workers cannot race on the
/// same row; if two workers run this query at the same moment, one
/// claims the row, the other skips it and either finds the next
/// candidate or returns `None`.
///
/// Claim eligibility:
/// - `status='pending'` AND (`next_attempt_at IS NULL OR <= claim_before`):
///   a row whose retry backoff has elapsed (or which has no
///   scheduled retry) is eligible.
/// - OR `status='in_progress' AND lock_expires_at < now()`: a row
///   whose previous worker crashed mid-delivery and whose lease
///   has expired is also eligible. Reclaim is transparent.
///
/// `claim_before` is the upper bound for retry eligibility. One-shot
/// callers pass `Utc::now()`. A drain loop captures `Utc::now()` once
/// at the top of the pass and supplies that same instant every
/// iteration, so rows deferred *during* the pass (a deliverer
/// returning `Transient { next_attempt_at: now() + 1ms }`) stay
/// invisible until the next pass. Without this, a sub-second retry
/// would let the drain re-claim the same row indefinitely; the worker
/// would never sleep or observe shutdown.
///
/// Lease-expiry reclaim of `in_progress` rows still uses live `now()`:
/// those are dead-worker recoveries, not scheduling decisions.
///
/// On claim: sets `status='in_progress'`, `locked_by=worker_id`,
/// `lock_expires_at=now()+lease_duration`, and returns the full
/// `OutboxRow`.
///
/// `lease_duration` is the window during which the claiming worker has
/// exclusive rights to mutate the row through the `mark_*` helpers.
/// Choosing it is the worker's responsibility: long enough to cover
/// the deliverer's latency plus headroom, short enough that a crashed
/// worker's rows become reclaimable in reasonable time.
///
/// The deliverer must run **outside** any database transaction; this
/// helper opens and closes the only transaction the claim needs (a
/// single atomic UPDATE ... RETURNING), and the lease is held via the
/// `locked_by`/`lock_expires_at` columns rather than a held row lock.
///
/// Internal substrate of [`process_one_outbox_row`]; use that unless
/// driving the state machine manually.
#[doc(hidden)]
pub async fn claim_pending_outbox_row(
    pool: &PgPool,
    worker_id: &str,
    intent_type: &str,
    lease_duration: std::time::Duration,
    claim_before: DateTime<Utc>,
) -> Result<Option<OutboxRow>, PgError> {
    let lease_secs = lease_duration_to_secs(lease_duration)?;
    let row_opt: Option<OutboxRowRaw> = sqlx::query_as(
        "UPDATE morpholog.outbox
         SET status='in_progress',
             locked_by=$1,
             lock_expires_at=now() + ($2 * interval '1 second')
         WHERE intent_id = (
             SELECT intent_id
             FROM morpholog.outbox
             WHERE intent_type=$3
               AND (
                   (status='pending'
                    AND (next_attempt_at IS NULL OR next_attempt_at <= $4))
                OR (status='in_progress'
                    AND lock_expires_at < now())
               )
             ORDER BY enqueued_at, intent_id
             LIMIT 1
             FOR UPDATE SKIP LOCKED
         )
         RETURNING intent_id, transition_id, intent_type, arguments,
                   idempotency_key, status, attempt_count, enqueued_at,
                   last_attempt_at, delivered_at, failed_at, failure_reason,
                   next_attempt_at, compensation_transition_id, locked_by,
                   lock_expires_at",
    )
    .bind(worker_id)
    .bind(lease_secs)
    .bind(intent_type)
    .bind(claim_before)
    .fetch_optional(pool)
    .await
    .map_err(classify)?;

    row_opt.map(decode_outbox_row).transpose()
}

/// Release a held lease without resolving the row to a terminal
/// state. The row returns to `status='pending'`, claimable by another
/// worker on its next pass.
///
/// For shutdown paths: a worker dying gracefully releases its
/// in-flight claims so they re-pick immediately rather than waiting
/// for lease expiry. Returns `LeaseLost` if the worker no longer holds
/// the lease (expected when a slow worker shuts down after expiry).
///
/// Internal substrate of the worker shutdown path; rarely needed
/// directly.
#[doc(hidden)]
pub async fn release_outbox_claim(
    pool: &PgPool,
    intent_id: Uuid,
    worker_id: &str,
) -> Result<OutboxUpdate, PgError> {
    let rows = sqlx::query(
        "UPDATE morpholog.outbox
         SET status='pending',
             locked_by=NULL,
             lock_expires_at=NULL
         WHERE intent_id=$1
           AND locked_by=$2
           AND lock_expires_at > now()",
    )
    .bind(intent_id)
    .bind(worker_id)
    .execute(pool)
    .await
    .map_err(classify)?;
    Ok(if rows.rows_affected() == 1 {
        OutboxUpdate::Applied
    } else {
        OutboxUpdate::LeaseLost
    })
}

/// Soonest future `next_attempt_at` over pending rows of the given
/// `intent_type`. Returns `None` if no such row exists.
///
/// A polling worker uses this after an empty drain to wake exactly
/// when the soonest scheduled retry becomes due (but no later than the
/// base poll interval, so newly-enqueued due rows are still picked up
/// promptly) instead of always sleeping the full interval.
///
/// `next_attempt_at` is filtered to `> now()`: a row whose retry
/// instant has already passed would have been claimed by the drain
/// that just ran.
pub async fn earliest_pending_retry(
    pool: &PgPool,
    intent_type: &str,
) -> Result<Option<DateTime<Utc>>, PgError> {
    let row: Option<(Option<DateTime<Utc>>,)> = sqlx::query_as(
        "SELECT min(next_attempt_at)
         FROM morpholog.outbox
         WHERE status='pending'
           AND intent_type=$1
           AND next_attempt_at IS NOT NULL
           AND next_attempt_at > now()",
    )
    .bind(intent_type)
    .fetch_optional(pool)
    .await
    .map_err(classify)?;
    Ok(row.and_then(|(t,)| t))
}

fn lease_duration_to_secs(lease_duration: std::time::Duration) -> Result<i64, PgError> {
    let lease_secs: i64 = lease_duration
        .as_secs()
        .try_into()
        .map_err(|_| PgError::InvalidState("lease_duration too large for i64".to_string()))?;
    if lease_secs < 1 {
        return Err(PgError::InvalidState(format!(
            "lease_duration must be at least 1 second (got {lease_duration:?}); \
             a sub-second lease would expire before the claiming worker could \
             call any mark_* / complete_* helper, leaving the row effectively \
             un-updatable"
        )));
    }
    Ok(lease_secs)
}

/// Atomically claim the right to run a compensating transformation
/// for a previously-failed outbox row.
///
/// Eligible rows are `status='failed' AND compensation_transition_id
/// IS NULL`. The claim transitions the row to `compensation_in_progress`
/// and sets the lease. Once held, the worker invokes the compensating
/// transformation via [`propose_against_pg`] and resolves the row with
/// [`complete_compensation`] (on `Committed`) or
/// [`mark_compensation_failed`] (on `Rejected`).
///
/// `SELECT ... FOR UPDATE SKIP LOCKED` guarantees at most one worker
/// holds the compensation lease at a time. Returns `Ok(None)` when no
/// eligible row exists for `intent_id` (missing, not `failed`, already
/// compensated, or locked by another worker mid-claim).
///
/// **Does NOT transparently reclaim expired-lease
/// compensation_in_progress rows** (unlike [`claim_pending_outbox_row`]
/// for `in_progress`). Reclaim would risk duplicate compensation if a
/// worker crashed *after* committing the compensating transformation
/// but *before* `complete_compensation`; a stuck row requires operator
/// intervention instead. The lease narrows the duplicate-compensation
/// race to the window between commit and `complete_compensation`;
/// programs needing full immunity should additionally guard the
/// compensating transformation with a
/// `CompensationApplied(original_intent_id)` invariant. See
/// `docs/outbox-sketch.md`.
///
/// Internal substrate of [`process_one_outbox_row`]'s compensation
/// arm; use that unless driving the state machine manually.
#[doc(hidden)]
pub async fn begin_compensation(
    pool: &PgPool,
    intent_id: Uuid,
    worker_id: &str,
    lease_duration: std::time::Duration,
) -> Result<Option<OutboxRow>, PgError> {
    let lease_secs = lease_duration_to_secs(lease_duration)?;
    let row_opt: Option<OutboxRowRaw> = sqlx::query_as(
        "UPDATE morpholog.outbox
         SET status='compensation_in_progress',
             locked_by=$1,
             lock_expires_at=now() + ($2 * interval '1 second')
         WHERE intent_id = (
             SELECT intent_id
             FROM morpholog.outbox
             WHERE intent_id=$3
               AND status='failed'
               AND compensation_transition_id IS NULL
             FOR UPDATE SKIP LOCKED
         )
         RETURNING intent_id, transition_id, intent_type, arguments,
                   idempotency_key, status, attempt_count, enqueued_at,
                   last_attempt_at, delivered_at, failed_at, failure_reason,
                   next_attempt_at, compensation_transition_id, locked_by,
                   lock_expires_at",
    )
    .bind(worker_id)
    .bind(lease_secs)
    .bind(intent_id)
    .fetch_optional(pool)
    .await
    .map_err(classify)?;

    row_opt.map(decode_outbox_row).transpose()
}

/// Resolve a compensation_in_progress row on success: transitions it
/// back to `failed` with `compensation_transition_id` recorded, and
/// releases the lease.
///
/// Gated on the worker holding the lease; returns
/// `OutboxUpdate::LeaseLost` otherwise. `compensation_transition_id`
/// must reference a row in `morpholog.audit` (foreign-key-enforced),
/// typically the `transition_id` [`propose_against_pg`] returned when
/// the compensating transformation committed.
///
/// Internal substrate of [`process_one_outbox_row`]'s compensation
/// arm; use that unless driving the state machine manually.
#[doc(hidden)]
pub async fn complete_compensation(
    pool: &PgPool,
    intent_id: Uuid,
    worker_id: &str,
    compensation_transition_id: Uuid,
) -> Result<OutboxUpdate, PgError> {
    let rows = sqlx::query(
        "UPDATE morpholog.outbox
         SET status='failed',
             compensation_transition_id=$3,
             locked_by=NULL,
             lock_expires_at=NULL
         WHERE intent_id=$1
           AND status='compensation_in_progress'
           AND locked_by=$2
           AND lock_expires_at > now()",
    )
    .bind(intent_id)
    .bind(worker_id)
    .bind(compensation_transition_id)
    .execute(pool)
    .await
    .map_err(classify)?;
    Ok(if rows.rows_affected() == 1 {
        OutboxUpdate::Applied
    } else {
        OutboxUpdate::LeaseLost
    })
}

/// Resolve a compensation_in_progress row on failure: transitions it
/// to `compensation_failed` with `reason` recorded, and releases the
/// lease.
///
/// Use this when the compensating transformation was itself rejected
/// by an invariant ([`propose_against_pg`] returned `Rejected`). This
/// is the genuinely-broken state - the original delivery failed AND
/// the compensation cannot be admitted - and stays in
/// `compensation_failed` until operator intervention.
///
/// Gated on the worker holding the lease; returns
/// `OutboxUpdate::LeaseLost` otherwise.
///
/// `reason` **overwrites** the original delivery `failure_reason`,
/// which is then lost to the morpholog tables: state mutators write no
/// audit rows (only transformations do). Callers needing both reasons
/// must capture the original externally before calling this.
///
/// Internal substrate of [`process_one_outbox_row`]'s compensation
/// arm; use that unless driving the state machine manually.
#[doc(hidden)]
pub async fn mark_compensation_failed(
    pool: &PgPool,
    intent_id: Uuid,
    worker_id: &str,
    reason: &str,
) -> Result<OutboxUpdate, PgError> {
    let rows = sqlx::query(
        "UPDATE morpholog.outbox
         SET status='compensation_failed',
             failure_reason=$3,
             locked_by=NULL,
             lock_expires_at=NULL
         WHERE intent_id=$1
           AND status='compensation_in_progress'
           AND locked_by=$2
           AND lock_expires_at > now()",
    )
    .bind(intent_id)
    .bind(worker_id)
    .bind(reason)
    .execute(pool)
    .await
    .map_err(classify)?;
    Ok(if rows.rows_affected() == 1 {
        OutboxUpdate::Applied
    } else {
        OutboxUpdate::LeaseLost
    })
}

/// Enumerate a derived claim's extension against the current durable state.
///
/// Loads only the admitted claims for predicates the derived claim's
/// body references (via [`list_claims_for_predicates`] and
/// [`morpholog_core::predicates_referenced_by_derived`]), wraps them
/// in an in-memory [`State`], and calls the synchronous
/// [`enumerate_derived`] kernel primitive. The result is a
/// [`ClaimInstance`] per distinct key binding the `domain` produces,
/// with each `DerivedValue` evaluated and appended to the key
/// positions.
///
/// Read-only: no claims written, no audit row, no outbox row. Repeated
/// calls recompute from scratch; there is no materialised view.
///
/// The predicate-scoped load is safe because the footprint analysis's
/// exhaustive `match` fails to compile if a new predicate-referencing
/// `Prop` or `ValueExpr` variant is added without handling it, so this
/// read path cannot silently produce wrong answers under a partial state.
///
/// Errors:
/// - [`PgError::Database`] / [`PgError::Encoding`] from the underlying
///   `list_claims_for_predicates` call.
/// - [`PgError::Kernel`] if the kernel rejects the derived claim's body
///   (type mismatch in a `DerivedValue.expr`, unbound variable in
///   `domain`, etc.). Each is a programmer error in the derived claim's
///   definition, not a runtime data condition.
///
/// Output ordering matches the kernel's contract: sorted by the
/// concatenated `(keys ++ computed values)` tuple under structural
/// `EvalValue` ordering, so results are deterministic across runs for a
/// given state.
pub async fn list_derived(
    pool: &PgPool,
    derived: &DerivedClaim,
) -> Result<Vec<ClaimInstance>, PgError> {
    let footprint: Vec<String> = predicates_referenced_by_derived(derived)
        .into_iter()
        .map(|p| p.to_string())
        .collect();
    let claims = list_claims_for_predicates(pool, &footprint).await?;
    let state = State::from_claims(claims);
    let rows = enumerate_derived(derived, &state)?;
    Ok(rows)
}

// ===========================================================================
// As-of helpers - audit-log replay to recover historical state
// ===========================================================================
//
// These helpers reconstruct the `State` that existed immediately after a
// chosen `transition_id` committed, by replaying every audit row up to and
// including that transition in causal order. The kernel is unchanged;
// as-of evaluation is just a question of which `State` you hand it.
//
// The coordinate is "as of *this actual committed transition*". An
// unknown id - smaller, larger, or between known ids - is rejected with
// `PgError::TransitionNotFound`; there is no fallback to current state.
//
// Replay is O(transitions up to T); full replay, no materialisation.

/// Reconstruct the full [`State`] that existed immediately after
/// `transition_id` committed.
///
/// Two queries: first looks up the target audit row to obtain its
/// `(committed_at, transition_id)` pair (and to verify the
/// transition exists); second replays every audit row whose
/// `(committed_at, transition_id)` tuple is less than or equal to
/// the target's tuple, in causal order.
///
/// Within each replayed transition, retractions are applied before
/// assertions - matching the kernel's `build_candidate_state`
/// semantics. Assertions are set-valued: asserting an already-present
/// claim is an idempotent no-op (matches the PG adapter's
/// `INSERT ... ON CONFLICT DO NOTHING` on commit).
///
/// Errors:
/// - [`PgError::TransitionNotFound`] if `transition_id` does not name
///   an existing audit row.
/// - [`PgError::Database`] / [`PgError::Encoding`] from the underlying
///   queries.
///
/// Replay cost is O(transitions up to T).
pub async fn reconstruct_state_at(pool: &PgPool, transition_id: Uuid) -> Result<State, PgError> {
    reconstruct_inner(pool, transition_id, None).await
}

/// Like [`reconstruct_state_at`] but only retains claims whose
/// predicate is in `predicates`. Used internally by
/// [`list_derived_at`] to load only the predicates the derived
/// claim's body references - the as-of analogue of
/// [`list_claims_for_predicates`].
///
/// Unlike the public [`reconstruct_state_at`], the resulting [`State`]
/// is **partial**: callers must not query predicates outside the
/// supplied set, since the kernel would report zero matches because
/// those claims were never added, not because they do not exist.
///
/// Empty `predicates` short-circuits to an empty `State`, but the
/// target `transition_id` must still exist (otherwise
/// [`PgError::TransitionNotFound`]). Mirrors
/// [`list_claims_for_predicates`].
pub(crate) async fn reconstruct_state_at_for_predicates(
    pool: &PgPool,
    transition_id: Uuid,
    predicates: &[String],
) -> Result<State, PgError> {
    if predicates.is_empty() {
        // The "as of this committed transition" contract still
        // requires the target to exist, even with an empty footprint.
        let target: Option<(Uuid,)> =
            sqlx::query_as("SELECT transition_id FROM morpholog.audit WHERE transition_id = $1")
                .bind(transition_id)
                .fetch_optional(pool)
                .await
                .map_err(classify)?;
        target.ok_or(PgError::TransitionNotFound(transition_id))?;
        return Ok(State::default());
    }
    reconstruct_inner(pool, transition_id, Some(predicates)).await
}

/// Returns the claims admitted as of `transition_id`, in causal
/// first-asserted order (the replay loop's construction order).
/// Differs from [`list_claims`] in two ways: the state is historical,
/// and the ordering is replay causality rather than `(asserted_at,
/// predicate_name, args)`.
///
/// Errors propagate from [`reconstruct_state_at`].
pub async fn list_claims_at(
    pool: &PgPool,
    transition_id: Uuid,
) -> Result<Vec<ClaimInstance>, PgError> {
    let state = reconstruct_state_at(pool, transition_id).await?;
    Ok(state.claims().to_vec())
}

/// Returns the claims of the given predicates admitted as of
/// `transition_id` - the historical counterpart of
/// [`list_claims_for_predicates`], replaying only the named
/// predicates rather than reconstructing the full state and
/// filtering after.
///
/// Empty `predicates` still validates that `transition_id` exists
/// (an unknown id is [`PgError::TransitionNotFound`], same as the
/// unscoped read) and then returns no claims: an empty footprint is
/// meaningful, not an error. Unknown predicate names simply match
/// nothing - the claims table is the authority here, not any
/// programme's declared vocabulary.
pub async fn list_claims_at_for_predicates(
    pool: &PgPool,
    transition_id: Uuid,
    predicates: &[String],
) -> Result<Vec<ClaimInstance>, PgError> {
    let state = reconstruct_state_at_for_predicates(pool, transition_id, predicates).await?;
    Ok(state.claims().to_vec())
}

/// Resolve a wall-clock instant to the last transition committed at or
/// before it - the timestamp form of an as-of coordinate. Uses the
/// `(committed_at, transition_id)` ordering, the same total order the
/// replay helpers use, so the answer is exact even when several
/// transitions share a `committed_at` under concurrent commits.
///
/// A timestamp earlier than every committed transition is
/// [`PgError::NoTransitionAtOrBefore`]: there is no state to
/// reconstruct at or before that instant.
pub async fn resolve_transition_at_or_before(
    pool: &PgPool,
    at: DateTime<Utc>,
) -> Result<Uuid, PgError> {
    let row: Option<(Uuid,)> = sqlx::query_as(
        "SELECT transition_id FROM morpholog.audit
         WHERE committed_at <= $1
         ORDER BY committed_at DESC, transition_id DESC
         LIMIT 1",
    )
    .bind(at)
    .fetch_optional(pool)
    .await
    .map_err(classify)?;
    row.map(|(tid,)| tid)
        .ok_or(PgError::NoTransitionAtOrBefore(at))
}

/// The outcome of replaying the audit log against the claims table.
///
/// The two tables are independent records of the same history: the
/// claims table is current state maintained write-by-write, the audit
/// log is the journal those writes came from. Replaying the journal
/// must land on the same claim set; a difference is evidence that one
/// of them was modified outside the runtime.
///
/// `Serialize` uses the same `status`-tagged representation as the
/// other CLI envelopes.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum VerifyOutcome {
    /// Replay reproduces the claims table exactly.
    Consistent {
        /// Committed transitions replayed.
        transitions: i64,
        /// Currently-admitted claims confirmed.
        claims: usize,
    },
    /// The two records disagree.
    Divergent {
        /// Claims present in the claims table that replaying the audit
        /// log does not produce - out-of-band inserts or edits.
        only_in_claims_table: Vec<ClaimInstance>,
        /// Claims the audit log says should be current but the claims
        /// table lacks - out-of-band deletes or edits.
        only_in_replay: Vec<ClaimInstance>,
    },
}

/// Replay the audit log to its latest transition and compare the
/// reconstructed state against the claims table.
///
/// An empty database (no transitions, no claims) is trivially
/// consistent. The comparison is a multiset diff, order-insensitive:
/// replay order is causal while the claims table orders by
/// `(asserted_at, ...)`, and neither order is part of the contract.
pub async fn verify_replay(pool: &PgPool) -> Result<VerifyOutcome, PgError> {
    let latest: Option<(Uuid,)> = sqlx::query_as(
        "SELECT transition_id FROM morpholog.audit
         ORDER BY committed_at DESC, transition_id DESC
         LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .map_err(classify)?;
    let (transitions,): (i64,) = sqlx::query_as("SELECT count(*) FROM morpholog.audit")
        .fetch_one(pool)
        .await
        .map_err(classify)?;

    let replayed = match latest {
        Some((tid,)) => reconstruct_state_at(pool, tid).await?.claims().to_vec(),
        None => Vec::new(),
    };
    let current = list_claims(pool).await?;

    // Multiset diff: +1 per current claim, -1 per replayed claim.
    // Positive residue exists only in the claims table, negative only
    // in the replay.
    let mut counts: HashMap<&ClaimInstance, i64> = HashMap::new();
    for c in &current {
        *counts.entry(c).or_default() += 1;
    }
    for c in &replayed {
        *counts.entry(c).or_default() -= 1;
    }
    let mut only_in_claims_table = Vec::new();
    let mut only_in_replay = Vec::new();
    for (claim, n) in counts {
        for _ in 0..n.abs() {
            if n > 0 {
                only_in_claims_table.push(claim.clone());
            } else if n < 0 {
                only_in_replay.push(claim.clone());
            }
        }
    }

    if only_in_claims_table.is_empty() && only_in_replay.is_empty() {
        Ok(VerifyOutcome::Consistent {
            transitions,
            claims: current.len(),
        })
    } else {
        Ok(VerifyOutcome::Divergent {
            only_in_claims_table,
            only_in_replay,
        })
    }
}

/// Enumerate a derived claim's extension against the state that
/// existed immediately after `transition_id` committed.
///
/// Mirrors [`list_derived`] but against historical state: the
/// derived claim's predicate footprint is computed via
/// [`morpholog_core::predicates_referenced_by_derived`], the audit
/// log is replayed up to `transition_id` keeping only claims of
/// those predicates, and `enumerate_derived` runs against the
/// resulting partial state.
///
/// Output is byte-identical to what [`list_derived`] would have
/// returned at the moment `transition_id` committed.
pub async fn list_derived_at(
    pool: &PgPool,
    derived: &DerivedClaim,
    transition_id: Uuid,
) -> Result<Vec<ClaimInstance>, PgError> {
    let footprint: Vec<String> = predicates_referenced_by_derived(derived)
        .into_iter()
        .map(|p| p.to_string())
        .collect();
    let state = reconstruct_state_at_for_predicates(pool, transition_id, &footprint).await?;
    let rows = enumerate_derived(derived, &state)?;
    Ok(rows)
}

/// Shared implementation behind [`reconstruct_state_at`] (full state)
/// and [`reconstruct_state_at_for_predicates`] (partial state). The
/// `predicates` parameter is `None` for the full case and
/// `Some(slice)` for the scoped case; the loop checks membership
/// during replay and skips both asserts and retracts whose predicate
/// is not in the set, so the scoped case never materialises
/// out-of-footprint claims in memory.
async fn reconstruct_inner(
    pool: &PgPool,
    transition_id: Uuid,
    predicates: Option<&[String]>,
) -> Result<State, PgError> {
    // Resolve the target transition's (committed_at, transition_id)
    // tuple. Missing target -> TransitionNotFound; this is the
    // contract that lets every other unknown id also be an error.
    let target: Option<(DateTime<Utc>, Uuid)> = sqlx::query_as(
        "SELECT committed_at, transition_id FROM morpholog.audit WHERE transition_id = $1",
    )
    .bind(transition_id)
    .fetch_optional(pool)
    .await
    .map_err(classify)?;
    let (target_committed_at, target_transition_id) =
        target.ok_or(PgError::TransitionNotFound(transition_id))?;

    // Precompute the scope as a HashSet so each in-loop membership
    // check is O(1) regardless of footprint size or audit-log length.
    let scope_set: Option<HashSet<&str>> =
        predicates.map(|preds| preds.iter().map(String::as_str).collect());

    // Replay every transition with a `(committed_at, transition_id)`
    // tuple less than or equal to the target's. PostgreSQL row
    // comparison (`(a, b) <= (c, d)`) is lexicographic; ordering by
    // the same two columns guarantees a deterministic replay.
    type Row = (serde_json::Value, serde_json::Value);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT asserted_claims, retracted_claims
         FROM morpholog.audit
         WHERE (committed_at, transition_id) <= ($1, $2)
         ORDER BY committed_at, transition_id",
    )
    .bind(target_committed_at)
    .bind(target_transition_id)
    .fetch_all(pool)
    .await
    .map_err(classify)?;

    let mut replay = ReplaySet::new();
    for (asserted_json, retracted_json) in rows {
        let asserted: Vec<ClaimInstance> = serde_json::from_value(asserted_json)?;
        let retracted: Vec<ClaimInstance> = serde_json::from_value(retracted_json)?;

        // Within each transition: retractions first, then assertions.
        // Matches build_candidate_state in the kernel.
        for r in &retracted {
            if !predicate_in_scope_set(r.predicate.as_str(), scope_set.as_ref()) {
                continue;
            }
            replay.retract(r);
        }
        for a in &asserted {
            if !predicate_in_scope_set(a.predicate.as_str(), scope_set.as_ref()) {
                continue;
            }
            replay.assert(a);
        }
    }
    Ok(replay.into_state())
}

/// Predicate-scope check. `None` (full reconstruction) accepts
/// everything; `Some(set)` accepts only predicates whose name is in
/// the set. The set is precomputed once per reconstruction in
/// [`reconstruct_inner`], so each check is O(1).
fn predicate_in_scope_set(predicate: &str, scope: Option<&HashSet<&str>>) -> bool {
    match scope {
        None => true,
        Some(set) => set.contains(predicate),
    }
}

/// Working state for audit-log replay. Keeps claims in first-asserted
/// order (the contract `list_claims_at` documents) while making both
/// `assert` and `retract` O(1) amortised - a plain `Vec` with linear
/// dedupe and `retain` would be O(N^2) over a full replay.
///
/// Internals:
/// - `claims` holds every claim ever asserted during this replay, in
///   first-asserted order. Never shrinks; compacted once at the end
///   via [`into_state`].
/// - `index` maps `claim -> position in claims`, used by both `assert`
///   (re-assertion detection) and `retract` (entry to mark dead).
/// - `live[i]` is `true` iff `claims[i]` is currently asserted.
///   Retraction flips it `false`; re-assertion flips it back.
struct ReplaySet {
    claims: Vec<ClaimInstance>,
    index: HashMap<ClaimInstance, usize>,
    live: Vec<bool>,
}

impl ReplaySet {
    fn new() -> Self {
        Self {
            claims: Vec::new(),
            index: HashMap::new(),
            live: Vec::new(),
        }
    }

    /// Assert a claim. First-time claims are appended and marked live;
    /// a previously-seen claim (live or retracted) has its existing
    /// slot flipped back to live. Re-asserting an already-live claim is
    /// a no-op, matching the set semantics the kernel pins.
    ///
    /// Re-assertion is zero-clone (only the `live` bit changes).
    /// First-time assertion is two clones - one for the `claims` Vec,
    /// one for the owned `index` key - the cost of keeping `claims`
    /// contiguous. The clone is cheap relative to the JSON decode that
    /// produced the input.
    fn assert(&mut self, claim: &ClaimInstance) {
        if let Some(&i) = self.index.get(claim) {
            self.live[i] = true;
        } else {
            let i = self.claims.len();
            self.claims.push(claim.clone());
            self.live.push(true);
            self.index.insert(claim.clone(), i);
        }
    }

    /// Retract a claim by marking its `live` slot `false`. If the
    /// claim was never asserted in this replay, the call is a no-op
    /// (matches the kernel's `Stmt::Retract` semantics: retracting
    /// a non-existent claim is an idempotent no-op).
    fn retract(&mut self, claim: &ClaimInstance) {
        if let Some(&i) = self.index.get(claim) {
            self.live[i] = false;
        }
    }

    /// Compact into a `State` containing only the live claims, in
    /// their first-asserted order. Runs once at the end of replay;
    /// O(|all-ever-asserted|).
    fn into_state(self) -> State {
        let claims: Vec<ClaimInstance> = self
            .claims
            .into_iter()
            .zip(self.live)
            .filter_map(|(c, alive)| alive.then_some(c))
            .collect();
        State::from_claims(claims)
    }
}

/// Outcome a [`Deliverer`] returns from a single delivery attempt.
///
/// The processor uses this to route the row through the
/// delivery-state machine: `Delivered` -> `delivered`, `Transient`
/// -> back to `pending` with the requested `next_attempt_at`,
/// `NonRetryable` -> `failed` (and then optional compensation).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// Delivery succeeded. The processor will mark the row
    /// `delivered`.
    Delivered,
    /// Delivery failed but should be retried no sooner than
    /// `next_attempt_at`. The processor returns the row to
    /// `pending` and sets that timestamp; the claim helper will
    /// then skip the row until the timestamp has passed. The
    /// deliverer is responsible for backoff policy (constant,
    /// exponential, jittered, etc.); the processor stores
    /// whatever instant the deliverer chose.
    Transient { next_attempt_at: DateTime<Utc> },
    /// Delivery failed in a way that should not be retried (the
    /// counterparty rejected the request authoritatively, the
    /// recipient does not exist, etc.). The processor marks the
    /// row `failed` with `reason` recorded. If the processor was
    /// supplied a [`CompensationSpec`], it then tries to claim the
    /// compensation lease via [`begin_compensation`] and run the
    /// compensating transformation.
    NonRetryable { reason: String },
}

/// A delivery target. Implementors define how to take one
/// admitted-and-enqueued intent and push it to the external world.
///
/// The processor passes the full [`OutboxRow`] - enough context for
/// retry/jitter decisions and any per-target idempotency-key handling
/// the receiver needs.
///
/// Implementors MUST NOT mutate any morpholog tables from `deliver`:
/// the processor owns the state machine, the deliverer owns only the
/// external side effect.
///
/// `Send + Sync` on the implementor and `Send` on the returned future
/// are baked in so polling loops can `tokio::spawn(deliverer.deliver(...))`
/// against an arbitrary `D: Deliverer`. RPITIT does not let callers add
/// the future's `Send` bound later, so it is fixed here.
pub trait Deliverer: Send + Sync {
    fn deliver(&self, row: &OutboxRow)
    -> impl std::future::Future<Output = DeliveryOutcome> + Send;
}

/// Closure mapping the just-failed outbox row to the arguments the
/// compensating transformation is invoked with. Boxed rather than
/// generic so `process_one_outbox_row`'s `Option<&CompensationSpec>`
/// has a single concrete type (callers pass `None` without an
/// inference workaround).
pub type CompensationArgsFromRow = Box<dyn Fn(&OutboxRow) -> Vec<EvalValue> + Send + Sync>;

/// Configuration the processor consults when delivery returns
/// `NonRetryable` and the row is moved to `failed`.
///
/// `args_from_row` is invoked AFTER [`begin_compensation`] has claimed
/// the lease, so the row it receives carries `failure_reason` from the
/// just-failed attempt and the closure can fold it into the
/// compensating transformation's arguments.
///
/// The compensating transformation goes through [`propose_against_pg`]
/// like any other - every invariant check, its own audit row, its own
/// outbox intents - so the audit log preserves the full lineage:
/// original commit, the `compensation_transition_id` linkage, and the
/// compensation's audit row.
pub struct CompensationSpec {
    pub transformation: Transformation,
    pub invariants: Vec<Invariant>,
    pub args_from_row: CompensationArgsFromRow,
}

/// Outcome of one [`process_one_outbox_row`] cycle. Surfaces enough
/// information that operational tooling and tests can assert which
/// branch was taken without re-querying the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessOutcome {
    /// No row was claimable: the outbox has no pending or
    /// expired-leased row of the requested `intent_type` whose
    /// `next_attempt_at` is due. The processor did nothing.
    NoRowAvailable,
    /// Delivery succeeded; the row is now `delivered`.
    Delivered { intent_id: Uuid },
    /// Delivery returned `Transient`; the row is back to `pending`
    /// with the supplied retry instant.
    TransientRetry {
        intent_id: Uuid,
        next_attempt_at: DateTime<Utc>,
    },
    /// Delivery returned `NonRetryable` and no [`CompensationSpec`]
    /// was supplied; the row is `failed`.
    Failed { intent_id: Uuid, reason: String },
    /// Delivery returned `NonRetryable` and a compensation was
    /// configured, but another worker already holds the
    /// compensation lease (or compensation already ran on this
    /// row). The processor did not invoke the compensating
    /// transformation. The row is `failed` (or further along) and
    /// this processor cycle is done.
    CompensationDeferred { intent_id: Uuid },
    /// Compensation ran and committed. The row is back to `failed`
    /// with `compensation_transition_id` pointing at the
    /// compensation's audit row.
    Compensated {
        intent_id: Uuid,
        compensation_transition_id: Uuid,
    },
    /// Compensation ran but was rejected by an invariant. The row
    /// is in `compensation_failed`. This is the genuinely-broken
    /// state requiring operator intervention.
    CompensationFailed { intent_id: Uuid, reason: String },
    /// A state-mutating helper returned [`OutboxUpdate::LeaseLost`]:
    /// the deliverer (or compensation arm) ran to completion, but the
    /// lease had expired and another worker reclaimed the row first.
    ///
    /// Not an error - the honest answer when a slow deliverer races the
    /// lease clock. Calling code should log and alert (the orphan-audit
    /// case during compensation, where the compensating transformation
    /// committed but the row's pointer never landed, is the most
    /// noteworthy; reconcile from the audit log) and move on.
    LeaseLost { intent_id: Uuid },
}

/// Drive one outbox row through the full delivery-and-compensation
/// state machine. Intended to be called in a loop by a worker
/// process (one call = one row processed; the loop owns scheduling).
///
/// The cycle:
/// 1. Claim a due row of the requested `intent_type` via
///    [`claim_pending_outbox_row`]. If none is claimable, return
///    [`ProcessOutcome::NoRowAvailable`].
/// 2. Invoke `deliverer.deliver(&row).await`.
/// 3. Route the [`DeliveryOutcome`]:
///    - `Delivered` -> [`mark_outbox_delivered`].
///    - `Transient` -> [`mark_outbox_transient_attempt`].
///    - `NonRetryable` -> [`mark_outbox_failed`], then if a
///      [`CompensationSpec`] is supplied, attempt
///      [`begin_compensation`] + invoke the compensating
///      transformation via [`propose_against_pg`] + resolve via
///      [`complete_compensation`] or [`mark_compensation_failed`].
///
/// Concurrency: safe across processes. Both [`claim_pending_outbox_row`]
/// and [`begin_compensation`] use `SELECT ... FOR UPDATE SKIP LOCKED`,
/// so at most one worker claims a given row or invokes the
/// compensating transformation for a given failed row.
///
/// The compensation race is closed under normal operation; a worker
/// crashing between `propose_against_pg` commit and
/// `complete_compensation` leaves the row stuck for operator recovery
/// rather than risking a duplicate (see [`begin_compensation`]).
///
/// `claim_before` is passed through to [`claim_pending_outbox_row`];
/// see it for the drain-loop safety rationale.
#[allow(clippy::too_many_arguments)]
pub async fn process_one_outbox_row<D>(
    pool: &PgPool,
    worker_id: &str,
    intent_type: &str,
    lease_duration: std::time::Duration,
    deliverer: &D,
    compensation: Option<&CompensationSpec>,
    claim_before: DateTime<Utc>,
) -> Result<ProcessOutcome, PgError>
where
    D: Deliverer,
{
    let Some(row) =
        claim_pending_outbox_row(pool, worker_id, intent_type, lease_duration, claim_before)
            .await?
    else {
        return Ok(ProcessOutcome::NoRowAvailable);
    };
    let intent_id = row.intent_id;

    match deliverer.deliver(&row).await {
        DeliveryOutcome::Delivered => {
            match mark_outbox_delivered(pool, intent_id, worker_id).await? {
                OutboxUpdate::Applied => Ok(ProcessOutcome::Delivered { intent_id }),
                OutboxUpdate::LeaseLost => Ok(ProcessOutcome::LeaseLost { intent_id }),
            }
        }
        DeliveryOutcome::Transient { next_attempt_at } => {
            match mark_outbox_transient_attempt(pool, intent_id, worker_id, next_attempt_at).await?
            {
                OutboxUpdate::Applied => Ok(ProcessOutcome::TransientRetry {
                    intent_id,
                    next_attempt_at,
                }),
                OutboxUpdate::LeaseLost => Ok(ProcessOutcome::LeaseLost { intent_id }),
            }
        }
        DeliveryOutcome::NonRetryable { reason } => {
            match mark_outbox_failed(pool, intent_id, worker_id, &reason).await? {
                OutboxUpdate::LeaseLost => {
                    // The row is no longer ours; compensation must not
                    // run because we never moved it to 'failed', which
                    // begin_compensation requires.
                    return Ok(ProcessOutcome::LeaseLost { intent_id });
                }
                OutboxUpdate::Applied => {}
            }
            let Some(spec) = compensation else {
                return Ok(ProcessOutcome::Failed { intent_id, reason });
            };
            // Re-claim the compensation lease that mark_outbox_failed
            // just released. SKIP LOCKED ensures at most one worker
            // wins under a concurrent recovery scan.
            let claimed = begin_compensation(pool, intent_id, worker_id, lease_duration).await?;
            let Some(failed_row) = claimed else {
                return Ok(ProcessOutcome::CompensationDeferred { intent_id });
            };
            let args = (spec.args_from_row)(&failed_row);
            let compensation_transition = Transition {
                transformation_name: spec.transformation.name.clone(),
                args,
                actor: system_actor(),
            };
            let outcome = propose_against_pg(
                pool,
                &spec.transformation,
                &compensation_transition,
                &spec.invariants,
            )
            .await?;
            match outcome {
                PgProposalOutcome::Committed { transition_id, .. } => {
                    match complete_compensation(pool, intent_id, worker_id, transition_id).await? {
                        OutboxUpdate::Applied => Ok(ProcessOutcome::Compensated {
                            intent_id,
                            compensation_transition_id: transition_id,
                        }),
                        OutboxUpdate::LeaseLost => Ok(ProcessOutcome::LeaseLost { intent_id }),
                    }
                }
                PgProposalOutcome::Rejected { reason } => {
                    match mark_compensation_failed(pool, intent_id, worker_id, &reason).await? {
                        OutboxUpdate::Applied => {
                            Ok(ProcessOutcome::CompensationFailed { intent_id, reason })
                        }
                        OutboxUpdate::LeaseLost => Ok(ProcessOutcome::LeaseLost { intent_id }),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_serialization_failure_code;

    /// Pins the `"40001"` magic string so the retry contract cannot
    /// regress silently.
    #[test]
    fn sqlstate_40001_classified_as_serialization_failure() {
        assert!(is_serialization_failure_code(Some("40001")));
    }

    /// Negative cases: other SQLSTATEs and the absence of a code must not
    /// be treated as retryable serialization failures.
    #[test]
    fn other_sqlstates_are_not_serialization_failures() {
        assert!(!is_serialization_failure_code(None));
        assert!(!is_serialization_failure_code(Some("40000")));
        assert!(!is_serialization_failure_code(Some("23505"))); // unique_violation
        assert!(!is_serialization_failure_code(Some("40P01"))); // deadlock_detected
    }
}
