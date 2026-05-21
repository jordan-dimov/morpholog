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
    ClaimInstance, DerivedClaim, EvalError, EvalValue, IntentInstance, Invariant, Outcome, State,
    TraceEntry, TracedProposal, Transformation, Transition, enumerate_derived,
    predicates_referenced_by_derived, propose, propose_with_trace,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Transaction};
use std::collections::{HashMap, HashSet};
use uuid::Uuid;

/// Re-export of `sqlx::PgPool` so downstream crates (notably
/// `morpholog-cli`) can use the connection-pool type that the public
/// async functions in this crate take, without pulling `sqlx` in as a
/// direct dependency.
pub use sqlx::PgPool;

/// Sentinel actor for transitions the runtime itself initiates, with
/// no user under whose authority the transition is being proposed.
/// Used by the outbox compensation path: when a delivery fails
/// non-retryably and a [`CompensationSpec`] is configured, the
/// compensating transformation is proposed by the runtime, not by
/// the actor of the original commit.
///
/// First-class authority modeling (granting and consulting actor
/// standing) is a later concern; the sentinel keeps the audit row's
/// `actor` column populated meaningfully until then.
pub fn system_actor() -> EvalValue {
    EvalValue::Subject("morpholog-system".to_string())
}

/// Errors returned by the PostgreSQL adapter.
///
/// Lawful business rejection is **not** an error — it is returned as
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
    /// Returned by the as-of helpers ([`reconstruct_state_at`],
    /// [`list_claims_at`], [`list_derived_at`]) when the caller asks
    /// for state at a coordinate that does not correspond to any
    /// committed transition. The contract is "exists or error" -
    /// every unknown id, smaller, larger, or between known ids, is
    /// rejected with this variant.
    #[error("transition_id {0} not found in morpholog.audit")]
    TransitionNotFound(Uuid),
}

/// The result of proposing a transformation against PostgreSQL.
///
/// On `Committed`, the database transaction has already been committed:
/// claims have been mutated, one audit row written, and one outbox row
/// per emitted intent. On `Rejected`, the transaction has been rolled
/// back and no governed state has changed.
///
/// `Serialize` is derived with serde's internally-tagged enum
/// representation so the CLI can emit outcomes directly as JSON with
/// a `status` discriminant. A committed outcome serialises as
/// `{"status":"committed","transition_id":"...","asserted_claims":[...],
/// "retracted_claims":[...],"emitted_intents":[...]}`; a rejected one as
/// `{"status":"rejected","reason":"..."}`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum PgProposalOutcome {
    Committed {
        transition_id: Uuid,
        actor: EvalValue,
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
/// The proposal is given as a [`Transition`] - which bundles the
/// transformation name (verified against `transformation.name`), the
/// arguments, and the actor under whose authority the transition is
/// being proposed. On `Committed`, the actor is persisted to the
/// `morpholog.audit.actor` column alongside the other audit fields.
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
/// kernel before the error is raised is **not** discarded. This is
/// the worst debugging case (multi-match `BindOne`, type-mismatch
/// `DateLe`, multi-match `ValueOf`, unbound actor) and exactly the
/// situation where the trace is most valuable; the previous
/// `Result<(_, trace), PgError>` shape dropped it.
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
    /// ran before the error - exactly the diagnostic surface that
    /// would otherwise be lost.
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
///
/// The CLI's `--trace` flag uses this function and emits a JSON
/// object on stdout for each kernel-side variant:
/// `{"result": <PgProposalOutcome>, "trace": [...]}` on Outcome,
/// `{"result": {"status": "errored", "error": "..."}, "trace": [...]}`
/// on KernelErrored. PG-layer errors surface via the existing anyhow
/// stderr error chain.
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
            // Roll back the open SERIALIZABLE transaction before
            // returning. The transaction would drop anyway, but
            // doing it explicitly keeps the connection available
            // sooner and surfaces any rollback-time DB failure as a
            // distinct `PgError::Database` rather than swallowing
            // it.
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

/// Predicate: is this SQLSTATE the PostgreSQL serialization-failure
/// code (`40001`) returned by SSI when a SERIALIZABLE transaction
/// cannot be linearised? Extracted as a pure function so the magic
/// string can be unit-tested without mocking `sqlx::DatabaseError`.
fn is_serialization_failure_code(code: Option<&str>) -> bool {
    code == Some("40001")
}

/// Maps a `sqlx::Error` to a [`PgError`], recognising SQLSTATE 40001
/// (PostgreSQL SSI serialization failure) as the distinct retryable
/// variant. All other errors propagate as [`PgError::Database`].
fn classify(err: sqlx::Error) -> PgError {
    let code = err.as_database_error().and_then(|e| e.code());
    if is_serialization_failure_code(code.as_deref()) {
        return PgError::SerializationFailure;
    }
    PgError::Database(err)
}

/// Load the pre-state for a `propose_against_pg` call, scoped to a
/// specific set of predicate names.
///
/// `scope` is the list of predicate names the transformation body
/// (per [`morpholog_core::predicates_read_by_stmt`]) and the active
/// invariants (per [`morpholog_core::predicates_referenced_by_expr`])
/// will consult. Claims of any other predicate are not loaded - they
/// cannot affect the kernel's evaluation of this transformation.
///
/// Empty scope returns an empty state without issuing a query
/// (mirrors [`list_claims_for_predicates`]'s contract). A
/// transformation with no body statements that read state and no
/// invariants will see an empty `State`; that is correct behaviour,
/// not a bug.
///
/// The scoping is a substantial perf win on large claim tables: a
/// transformation that touches three predicates with low cardinality
/// no longer pays the linear cost of fetching and decoding every
/// row in `morpholog.claims`.
async fn load_state(
    tx: &mut Transaction<'_, Postgres>,
    scope: &[String],
) -> Result<State, PgError> {
    if scope.is_empty() {
        return Ok(State::default());
    }

    let rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT predicate_name, arguments
         FROM morpholog.claims
         WHERE predicate_name = ANY($1)",
    )
    .bind(scope)
    .fetch_all(&mut **tx)
    .await
    .map_err(classify)?;

    let mut claims = Vec::with_capacity(rows.len());
    for (predicate, args_json) in rows {
        let args: Vec<EvalValue> = serde_json::from_value(args_json)?;
        claims.push(ClaimInstance { predicate, args });
    }
    Ok(State::from_claims(claims))
}

/// Compute the predicate scope that `load_state` must fetch to
/// evaluate this transformation correctly. The union of:
///
/// - Every predicate read by every statement in the transformation
///   body (via `morpholog_core::predicates_read_by_stmt`).
/// - Every predicate referenced by every invariant body (via
///   `morpholog_core::predicates_referenced_by_expr`). Invariants
///   evaluate against the candidate state - which is built from the
///   pre-state plus asserts minus retracts - so any predicate an
///   invariant inspects must be loaded.
///
/// `Stmt::Assert`'s output predicate is deliberately NOT in the read
/// set; the assert stages a new claim, it doesn't read existing
/// ones. If an invariant *also* references that predicate, it's
/// picked up via the invariant walker and loaded.
fn compute_load_scope(transformation: &Transformation, invariants: &[Invariant]) -> Vec<String> {
    let mut scope = std::collections::BTreeSet::new();
    for stmt in &transformation.body {
        morpholog_core::predicates_read_by_stmt(stmt, &mut scope);
    }
    for inv in invariants {
        morpholog_core::predicates_referenced_by_expr(&inv.body, &mut scope);
    }
    scope.into_iter().collect()
}

/// One entry in an audit row's `invariants_checked` JSONB array. Recorded
/// per committed transformation: the invariant `name` plus the `version`
/// active at admission time. Self-describing audit data is preferred over
/// tuple compactness.
///
/// Named `AuditedInvariantCheck` rather than `InvariantCheck` to
/// disambiguate from the kernel's `TraceEntry::InvariantCheck` variant
/// (in `morpholog_core::TraceEntry`). Both describe "an invariant was
/// checked" but at different layers: this type is the durable audit
/// record persisted alongside a committed transition; the kernel
/// variant is a transient per-call diagnostic entry produced by
/// `propose_with_trace`.
///
/// `Serialize` is derived so the CLI can re-emit audit rows as JSON
/// without an intermediate hand-rolled mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditedInvariantCheck {
    pub name: String,
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
    // Retractions: dedupe, then delete each distinct claim. We expect
    // exactly one row affected per distinct retraction. Zero rows
    // indicates a persistent-state mismatch — either a concurrent
    // transaction has interfered (SSI will catch it later) or the
    // pre-state snapshot disagrees with the live table.
    let mut seen: HashSet<(String, String)> = HashSet::new();
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
        .bind(&claim.predicate)
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
        .bind(&claim.predicate)
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
    .bind(&transformation.name)
    .bind(serde_json::to_value(&transition.args)?)
    .bind(serde_json::to_value(&transition.actor)?)
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
        .bind(&intent.name)
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
/// `canonical_json` is `serde_json` output using the PR #4 pinned wire shape
/// (stable for the current structs because field order is fixed by derived
/// `Serialize` and there are no map-like runtime values).
///
/// The key is unique per `(transition_id, intent.name, intent.args)`. It
/// prevents duplicate outbox rows under retry/redelivery mechanics — not
/// duplicate business events, which would require an idempotency key
/// derived from the inbound request.
///
/// **Duplicate intents within one transformation:** if a transformation
/// emits the same intent (same `name` and `args`) twice, both rows will
/// share an idempotency key and the second `INSERT` will violate the
/// `outbox.idempotency_key` UNIQUE constraint — surfacing as a
/// `PgError::Database` and rolling back the whole transformation. This
/// is intentional for v0: identical duplicate intents are almost always
/// a bug and should not silently produce two outbox rows. If genuinely
/// distinct same-shaped intents are needed later, the `Intent` type
/// will gain a discriminator field (logical key, purpose, sequence) and
/// this docstring should be updated.
pub fn compute_idempotency_key(
    transition_id: Uuid,
    intent: &IntentInstance,
) -> Result<String, serde_json::Error> {
    let args_bytes = serde_json::to_vec(&intent.args)?;
    let mut hasher = Sha256::new();
    hasher.update(transition_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(intent.name.as_bytes());
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
/// JSONB columns (`arguments`, `invariants_checked`, `asserted_claims`,
/// `retracted_claims`, `emitted_intents`) are decoded through the same
/// codec that wrote them, so the round-trip is exact for any value the
/// kernel can represent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditRow {
    pub transition_id: Uuid,
    pub transformation_name: String,
    pub arguments: Vec<EvalValue>,
    pub actor: EvalValue,
    pub invariant_epoch: i32,
    pub invariants_checked: Vec<AuditedInvariantCheck>,
    pub asserted_claims: Vec<ClaimInstance>,
    pub retracted_claims: Vec<ClaimInstance>,
    pub emitted_intents: Vec<IntentInstance>,
    pub committed_at: DateTime<Utc>,
}

/// One row of `morpholog.outbox` decoded into typed runtime values.
///
/// Carries every column on the table. The delivery-state extensions
/// (`failed_at`, `failure_reason`, `next_attempt_at`,
/// `compensation_transition_id`, `locked_by`, `lock_expires_at`)
/// are nullable in the schema and `Option<T>` here; they fill in as
/// a row moves through the delivery state machine. `attempt_count`
/// and `last_attempt_at` retain the original contract: a `pending`
/// row with `attempt_count > 0` and a non-NULL `last_attempt_at` is
/// one a worker has tried and failed (transiently), not a fresh
/// enqueue.
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
/// A worker that does not hold the current lease (because the lease
/// expired and another worker took over, or because the worker
/// supplied the wrong `worker_id`) cannot clobber the row's state.
/// The helper does not error: lease loss is a normal operational
/// condition for a worker that crashed mid-delivery and another
/// worker now owns the row. But the helper does not silently lie
/// about it either - the caller sees [`OutboxUpdate::LeaseLost`]
/// and can choose to log, retry-after-reclaim, or move on.
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
/// Order is `(asserted_at, predicate_name, arguments::text)` — causal
/// admission order, with predicate-then-args as the stable tie-break.
/// Two claims admitted in the same microsecond will appear in a
/// deterministic order across runs.
///
/// This is a `SELECT *` over the entire table. For large states the
/// caller should use SQL directly; the v0 helper is for tests, demos,
/// and small-state inspection.
pub async fn list_claims(pool: &PgPool) -> Result<Vec<ClaimInstance>, PgError> {
    let rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT predicate_name, arguments
         FROM morpholog.claims
         ORDER BY asserted_at, predicate_name, arguments::text",
    )
    .fetch_all(pool)
    .await
    .map_err(classify)?;

    rows.into_iter()
        .map(|(predicate, args_json)| {
            Ok(ClaimInstance {
                predicate,
                args: serde_json::from_value(args_json)?,
            })
        })
        .collect()
}

/// Return every currently-admitted claim whose `predicate_name` is in
/// `predicates`. Empty `predicates` short-circuits to `Ok(vec![])`
/// without issuing a query; an empty input set means the caller has
/// no predicate footprint to read, which is meaningful (e.g. a
/// derived claim whose domain is a no-op) and is not an error.
///
/// Used by [`list_derived`] to load only the claims relevant to the
/// derived claim's footprint - which avoids fetching and decoding the
/// rest of `morpholog.claims` when the derived only needs a few
/// predicates. The footprint analysis lives in
/// [`morpholog_core::predicates_referenced_by_derived`].
///
/// Order matches [`list_claims`]: `(asserted_at, predicate_name,
/// arguments::text)`. Deterministic across runs.
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

    rows.into_iter()
        .map(|(predicate, args_json)| {
            Ok(ClaimInstance {
                predicate,
                args: serde_json::from_value(args_json)?,
            })
        })
        .collect()
}

/// Return every committed audit row from `morpholog.audit`, ordered by
/// `(committed_at, transition_id)` — causal commit order with the
/// `transition_id` PRIMARY KEY (UUIDv7, time-ordered) as the stable
/// tie-break.
///
/// All five JSONB columns are decoded through the codec; the caller
/// receives typed values, not raw `serde_json::Value`. A decoding error
/// surfaces as [`PgError::Encoding`] — that should never happen against
/// a database the runtime itself wrote to, and indicates corruption or
/// out-of-band tampering.
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
                    transformation_name,
                    arguments: serde_json::from_value(args_json)?,
                    actor: serde_json::from_value(actor_json)?,
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
/// `(enqueued_at, intent_id)` — the same causal-order-with-PK-tie-break
/// pattern used elsewhere. This is the natural "what does the worker
/// have to deliver?" query.
///
/// Delivered and failed rows are excluded; a future `list_all_outbox`
/// or status-filtered helper would surface them. v0 only exposes the
/// in-flight queue.
pub async fn list_pending_outbox(pool: &PgPool) -> Result<Vec<OutboxRow>, PgError> {
    let rows: Vec<OutboxRowRaw> = sqlx::query_as(OUTBOX_SELECT_ALL_COLUMNS)
        .bind("pending")
        .fetch_all(pool)
        .await
        .map_err(classify)?;
    rows.into_iter().map(decode_outbox_row).collect()
}

/// The full column list returned by every outbox-row read in this
/// module. Single source of truth so the `OutboxRowRaw` tuple shape,
/// the `decode_outbox_row` helper, and the SQL all evolve together.
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
// `OutboxUpdate::LeaseLost` if the lease has been taken over by
// another worker. `record_compensation` is the only helper that
// errors on contract violation (rather than returning `LeaseLost`),
// because attempting to record compensation against a non-failed
// row or a row that already has a compensation linked is a
// programming bug, not an operational condition.

/// Mark a successfully-delivered outbox row.
///
/// Transitions `status` to `'delivered'`, sets `delivered_at = now()`,
/// increments `attempt_count`, and clears the lease fields (`locked_by`,
/// `lock_expires_at`) so the row is unambiguously done.
///
/// Returns `Applied` on success, `LeaseLost` if the worker no longer
/// holds the lease.
///
/// Internal substrate of [`process_one_outbox_row`]; reach for that
/// instead unless you are driving the state machine manually.
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
/// equal-now retry instant is accepted by this helper. The drain
/// loop's protection against re-claiming the same row in the same
/// pass lives at [`claim_pending_outbox_row`]'s
/// `claim_before` upper bound, not here. Adding a validation here
/// would conflict with the helper contract (lease loss must
/// surface as [`OutboxUpdate::LeaseLost`], not as a [`PgError`])
/// and would spuriously fail a slow legitimate delivery whose
/// retry instant elapses during transit.
///
/// Internal substrate of [`process_one_outbox_row`]; reach for that
/// instead unless you are driving the state machine manually.
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
/// Internal substrate of [`process_one_outbox_row`]; reach for that
/// instead unless you are driving the state machine manually.
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
/// Gated by two preconditions, both enforced by the SQL `WHERE`:
/// the row must be in `status='failed'`, and it must not already
/// carry a `compensation_transition_id`. Violating either is a
/// programming bug - attempting to attach compensation to a
/// delivered or pending row, or double-recording compensation -
/// and surfaces as [`PgError::InvalidState`] rather than a silent
/// no-op.
///
/// `compensation_transition_id` must reference a row in
/// `morpholog.audit` (foreign-key-enforced). The worker invokes
/// the compensating transformation via [`propose_against_pg`] and
/// passes the resulting `transition_id` here.
///
/// This helper does NOT gate on a lease. By the time the worker
/// is recording compensation, the original delivery attempt is
/// over and the row is in `failed`; the lease was already released
/// by [`mark_outbox_failed`].
///
/// **Important: this is a lineage setter, not a duplicate-invocation
/// guard.** The `compensation_transition_id IS NULL` predicate only
/// prevents a second *record* call from overwriting the first - it
/// does not prevent a second *compensating transformation* from
/// being committed via [`propose_against_pg`] before either record
/// call runs. If two workers race on the same `failed` row, both
/// can commit independent compensating transformations against the
/// underlying state; only the second `record_compensation` will
/// fail, but by then a duplicate compensation has already landed
/// in `morpholog.audit` and its claims in `morpholog.claims`.
///
/// The single-row processor that invokes this helper is therefore
/// responsible for preventing duplicate compensation upstream -
/// either by retaining lease ownership across the failed -> commit
/// compensation -> record_compensation arc (rather than releasing
/// in [`mark_outbox_failed`]), or by guarding the compensating
/// transformation itself with an invariant over an
/// `original_intent_id` predicate. See `docs/outbox-sketch.md` for
/// the two-mechanism discussion.
///
/// Internal substrate of [`process_one_outbox_row`]; reach for that
/// instead unless you are driving the state machine manually.
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
/// The `claim_before` parameter is the upper bound for retry
/// eligibility. One-shot callers pass `Utc::now()`. A drain loop
/// captures `Utc::now()` once at the top of the pass and supplies
/// that same instant for every iteration, so rows deferred *during*
/// the pass (e.g., a deliverer that returns
/// `Transient { next_attempt_at: now() + 1ms }`) are invisible
/// until the next pass even if wall-clock time has moved past their
/// `next_attempt_at`. Without this discipline a sub-second retry
/// would let the drain re-claim the same row indefinitely; the
/// worker would never sleep, never observe shutdown.
///
/// Lease-expiry reclaim of `in_progress` rows still uses live
/// `now()` - those are dead-worker recoveries, not scheduling
/// decisions, and there is no loop pathology to defend against.
///
/// On claim: sets `status='in_progress'`, `locked_by=worker_id`,
/// `lock_expires_at=now()+lease_duration`. Returns the full
/// `OutboxRow` (now reflecting the lease).
///
/// `lease_duration` is the wall-clock window during which the
/// claiming worker has exclusive rights to mutate the row through
/// `mark_outbox_delivered`, `mark_outbox_transient_attempt`, or
/// `mark_outbox_failed`. Picking the right duration is the worker's
/// responsibility - long enough to cover the deliverer's expected
/// latency plus headroom; short enough that a crashed worker's
/// rows become reclaimable in reasonable time.
///
/// The deliverer must run **outside** any database transaction;
/// this helper opens and closes the only transaction the claim
/// needs (a single atomic UPDATE ... RETURNING), and the caller
/// holds the lease via the `locked_by`/`lock_expires_at` columns
/// rather than a held row lock.
///
/// Internal substrate of [`process_one_outbox_row`]; reach for that
/// instead unless you are driving the state machine manually.
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
/// state. The row returns to `status='pending'` and becomes
/// claimable by another worker on its next pass.
///
/// Useful for shutdown paths (a worker dying gracefully releases
/// its in-flight claims so they can be re-picked immediately
/// rather than waiting for the lease to expire). Returns
/// `LeaseLost` if the worker no longer holds the lease - which is
/// expected if a slow worker is shutting down after its lease
/// already expired.
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
/// A polling worker uses this after a drain returns no work to
/// decide how long to sleep before its next poll: instead of always
/// sleeping the base poll interval, it can wake up exactly when
/// the soonest scheduled retry becomes due, but no later than the
/// base interval (so that newly-enqueued immediately-due rows are
/// still picked up promptly).
///
/// `next_attempt_at` is filtered to `> now()` so a row whose retry
/// instant has already passed is not returned (it would have been
/// claimed by the drain that just ran).
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
/// and sets `locked_by` + `lock_expires_at`. Once held, the worker is
/// expected to invoke the compensating transformation via
/// [`propose_against_pg`] and then resolve the row through one of:
/// - [`complete_compensation`] on `PgProposalOutcome::Committed`
///   (transitions back to `failed` with the
///   `compensation_transition_id` set);
/// - [`mark_compensation_failed`] on `PgProposalOutcome::Rejected`
///   (transitions to `compensation_failed`, the genuinely-broken
///   state requiring operator intervention).
///
/// The `SELECT ... FOR UPDATE SKIP LOCKED` wrapping the row UPDATE
/// guarantees at most one worker holds the compensation lease for a
/// given row at any moment. Returns `Ok(None)` if no eligible row
/// exists for the supplied `intent_id` (either the row is missing,
/// is not in `failed`, has already been compensated, or is currently
/// locked by another worker mid-claim).
///
/// **Important: this helper does NOT transparently reclaim
/// expired-lease compensation_in_progress rows**, unlike
/// [`claim_pending_outbox_row`]'s reclaim of expired in_progress
/// leases. Transparent reclaim would risk duplicate compensation if
/// a previous worker crashed *after* committing the compensating
/// transformation but *before* calling `complete_compensation`. The
/// safer default is: a stuck `compensation_in_progress` row
/// requires operator intervention rather than automatic recovery.
/// The lease pattern reduces the duplicate-compensation race to a
/// narrow window (between `propose_against_pg` commit and
/// `complete_compensation` call); programs that need full immunity
/// should additionally guard the compensating transformation with
/// a `CompensationApplied(original_intent_id)` invariant, per the
/// two-mechanism discussion in `docs/outbox-sketch.md`.
///
/// Internal substrate of [`process_one_outbox_row`]'s compensation
/// arm; reach for that instead unless you are driving the state
/// machine manually.
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

/// Resolve a compensation_in_progress row on success: transitions
/// it back to `failed` with the supplied `compensation_transition_id`
/// recorded, and releases the lease.
///
/// Gated by `status='compensation_in_progress' AND locked_by=$worker
/// AND lock_expires_at > now()`. Returns `OutboxUpdate::LeaseLost`
/// if the worker no longer holds the lease (expired or never
/// acquired).
///
/// The `compensation_transition_id` must reference a row in
/// `morpholog.audit` (foreign-key-enforced); typically it is the
/// `transition_id` returned by [`propose_against_pg`] when the
/// compensating transformation committed.
///
/// Internal substrate of [`process_one_outbox_row`]'s compensation
/// arm; reach for that instead unless you are driving the state
/// machine manually.
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

/// Resolve a compensation_in_progress row on failure: transitions
/// it to `compensation_failed` with the supplied `reason` recorded,
/// and releases the lease.
///
/// Use this when the compensating transformation itself was
/// rejected by an invariant (i.e., [`propose_against_pg`] returned
/// `PgProposalOutcome::Rejected`). This is the genuinely-broken
/// state: the original delivery failed, AND the compensation
/// designed to undo its business effect cannot be admitted. No
/// automatic recovery; the row stays in `compensation_failed` until
/// operator intervention (out of v0 scope).
///
/// Gated by `status='compensation_in_progress' AND locked_by=$worker
/// AND lock_expires_at > now()`. Returns `OutboxUpdate::LeaseLost`
/// if the worker no longer holds the lease.
///
/// `reason` is stored in the existing `failure_reason` column,
/// **overwriting** the original delivery failure reason. The
/// original is then lost as far as morpholog tables are concerned:
/// state mutators like `mark_outbox_failed` and `begin_compensation`
/// do NOT write audit rows (only transformations do), so there is
/// nothing in `morpholog.audit` to reconstruct from. If callers
/// need both the delivery failure reason and the compensation
/// rejection reason, they must capture the original externally
/// (an `outbox_event` table, structured logs, etc.) before calling
/// this helper.
///
/// Internal substrate of [`process_one_outbox_row`]'s compensation
/// arm; reach for that instead unless you are driving the state
/// machine manually.
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
/// body actually references (via [`list_claims_for_predicates`] and
/// [`morpholog_core::predicates_referenced_by_derived`]), wraps them
/// in an in-memory [`State`], and calls the synchronous
/// [`enumerate_derived`] kernel primitive. The result is a
/// [`ClaimInstance`] per distinct key binding the derived claim's
/// `domain` produces, with each `DerivedValue` evaluated and
/// appended to the key positions.
///
/// Read-only: no claims are written, no audit row is produced, no
/// outbox row is enqueued. Repeated calls compute the result from
/// scratch - there is no materialised view in v0.
///
/// The predicate-scoped load is safe because the kernel only reads
/// claims whose predicate matches an `Expr::Claim`, `Expr::ValueOf`,
/// or other predicate-referencing expression node inside the
/// derived's body. If a future PR adds a new `Expr` variant that
/// references a predicate, `predicates_referenced_by_expr`'s
/// exhaustive match will fail to compile until the new variant is
/// handled - which prevents this read path from silently producing
/// wrong answers under a partial state.
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
// including that transition in causal order. The kernel does not change;
// `enumerate_derived(&State)` is unchanged. As-of evaluation is a question
// of which `State` you hand to the kernel.
//
// The coordinate is "as of *this actual committed transition*". An
// unknown id - smaller, larger, or between known ids - is rejected with
// `PgError::TransitionNotFound`. There is no magical fallback to current
// state for ids that happen to order past every committed transition.
//
// Replay is O(transitions up to T). v0 ships full replay; materialisation
// is the next-forced optimisation if a bench scenario shows the cost.

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
/// Replay cost is O(transitions up to T). For long audit logs this
/// becomes painful; snapshotting / materialisation is the next-forced
/// optimisation but is deliberately out of scope in v0.
pub async fn reconstruct_state_at(pool: &PgPool, transition_id: Uuid) -> Result<State, PgError> {
    reconstruct_inner(pool, transition_id, None).await
}

/// Like [`reconstruct_state_at`] but only retains claims whose
/// predicate is in `predicates`. Used internally by
/// [`list_derived_at`] to load only the predicates the derived
/// claim's body references - the as-of analogue of
/// [`list_claims_for_predicates`].
///
/// The contract is intentionally distinct from the public
/// [`reconstruct_state_at`]: the resulting [`State`] is **partial**.
/// Callers downstream of this function must not query predicates
/// outside the supplied set against the returned state - the kernel
/// would correctly report zero matches because those claims were
/// never added, not because they do not exist.
///
/// Empty `predicates` short-circuits: the target `transition_id`
/// must still exist (returning [`PgError::TransitionNotFound`]
/// otherwise), but no audit rows are fetched or replayed because
/// the result is unconditionally an empty `State`. Mirrors the
/// short-circuit behaviour of [`list_claims_for_predicates`].
pub(crate) async fn reconstruct_state_at_for_predicates(
    pool: &PgPool,
    transition_id: Uuid,
    predicates: &[String],
) -> Result<State, PgError> {
    if predicates.is_empty() {
        // Still verify the target transition exists; the contract
        // is "as of *this actual committed transition*", and an
        // empty footprint does not change that.
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

/// Returns the claims that were admitted at `transition_id`, in
/// causal first-asserted order (the construction order produced by
/// the replay loop). Differs from [`list_claims`] in two ways: the
/// state is historical, not current; and the ordering is replay
/// causality rather than `(asserted_at, predicate_name, args)`.
///
/// Errors propagate from [`reconstruct_state_at`].
pub async fn list_claims_at(
    pool: &PgPool,
    transition_id: Uuid,
) -> Result<Vec<ClaimInstance>, PgError> {
    let state = reconstruct_state_at(pool, transition_id).await?;
    Ok(state.claims().to_vec())
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

    // Precompute the predicate scope as a HashSet for O(1) lookups
    // inside the replay loop. For derived footprints with one or
    // two predicates the linear scan was fine; precomputing once
    // per reconstruction keeps it cheap regardless of footprint
    // size or audit-log length.
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
            if !predicate_in_scope_set(&r.predicate, scope_set.as_ref()) {
                continue;
            }
            replay.retract(r);
        }
        for a in &asserted {
            if !predicate_in_scope_set(&a.predicate, scope_set.as_ref()) {
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

/// Working state for audit-log replay. Keeps claims in
/// first-asserted order (matching the contract `list_claims_at`
/// documents) while making both `assert` and `retract` `O(1)`
/// amortised.
///
/// Earlier replay used a plain `Vec<ClaimInstance>` with
/// `iter().any` for dedupe and `retain` for retraction; each was
/// `O(|claims|)` per operation, summing to `O(N^2)` over a full
/// replay. That pathology surfaced in the `morpholog-bench as-of`
/// scenario (PR #28): N=10K reconstruction took ~4.6 s. This
/// structure replaces both linear scans with hash-keyed lookups.
///
/// Internals:
/// - `claims` holds every claim ever asserted during this replay,
///   in the order it was first asserted. Never shrinks during
///   replay; compacted once at the end via [`into_state`].
/// - `index` maps `claim -> position in claims`. Used by both
///   `assert` (to detect re-assertion of a previously-seen claim)
///   and `retract` (to find the entry to mark dead).
/// - `live[i]` is `true` iff `claims[i]` is currently asserted.
///   Retraction flips it to `false`; re-assertion flips it back.
///
/// The compaction in [`into_state`] walks `claims` once and keeps
/// only the live entries, preserving original insertion order.
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

    /// Assert a claim. If it has never been asserted before, append
    /// it to `claims` and mark it live. If it has been seen (whether
    /// currently live or retracted), flip its existing slot back to
    /// live. Re-asserting an already-live claim is a no-op (the set
    /// semantics the kernel pins; an `INSERT ... ON CONFLICT DO
    /// NOTHING` on the write side).
    ///
    /// Clone counts:
    /// - Re-assertion of an already-seen claim (live or retracted):
    ///   **zero clones**. The `index.get(claim)` borrows the input;
    ///   only the `live` bit is touched.
    /// - First-time assertion: **two clones**. One into the `claims`
    ///   vector (we need owned storage there) and one into the
    ///   `index` HashMap as the key (HashMap keys must be owned).
    ///   Single-clone is not reachable without `Rc`/`Arc` or
    ///   borrow-checker gymnastics; the dual storage is the cost of
    ///   keeping `claims` as a contiguous Vec.
    ///
    /// The clone of a `ClaimInstance` is cheap relative to the
    /// JSON-decode that produced the input; for the common
    /// asserts-only audit log every claim takes the two-clone path,
    /// which is what the bench measures.
    fn assert(&mut self, claim: &ClaimInstance) {
        if let Some(&i) = self.index.get(claim) {
            // Already seen: re-activate the existing slot. Zero
            // clones; just a HashMap lookup and a Vec index write.
            self.live[i] = true;
        } else {
            // First time seen: two clones (one for the Vec, one for
            // the HashMap key).
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
/// The processor passes the full [`OutboxRow`] so implementors can
/// access `arguments`, `attempt_count`, `enqueued_at`,
/// `last_attempt_at`, etc. - enough context for retry/jitter
/// decisions and for any per-target idempotency-key handling the
/// receiver needs.
///
/// Implementors MUST NOT mutate any morpholog tables from within
/// `deliver`. The processor owns the state machine; the deliverer
/// owns only the external side effect.
///
/// The trait bakes in `Send + Sync` on the implementor and `Send`
/// on the returned future so that polling loops that spawn one
/// worker per intent type can `tokio::spawn(deliverer.deliver(...))`
/// against an arbitrary `D: Deliverer`. RPITIT (return position impl
/// trait in trait) does not let callers add a `Send` bound on the
/// anonymous future later, so the bound is fixed here rather than
/// reintroduced as a breaking change.
pub trait Deliverer: Send + Sync {
    fn deliver(&self, row: &OutboxRow)
    -> impl std::future::Future<Output = DeliveryOutcome> + Send;
}

/// Closure mapping the just-failed outbox row to the arguments the
/// compensating transformation should be invoked with. Wrapped in
/// a `Box<dyn ...>` rather than expressed as a generic so that
/// `process_one_outbox_row`'s `Option<&CompensationSpec>` parameter
/// has a single concrete type (callers can pass `None` without an
/// inference workaround).
pub type CompensationArgsFromRow = Box<dyn Fn(&OutboxRow) -> Vec<EvalValue> + Send + Sync>;

/// Configuration the processor consults when delivery returns
/// `NonRetryable` and the row is moved to `failed`.
///
/// `args_from_row` is invoked AFTER [`begin_compensation`] has
/// claimed the compensation lease on the row, so the row passed in
/// is the one carrying `failure_reason` from the just-failed
/// delivery attempt; the closure can read it to incorporate the
/// reason into the compensating transformation's arguments.
///
/// The compensating transformation is invoked via
/// [`propose_against_pg`] just like any other transformation - it
/// goes through every invariant check, writes its own audit row,
/// and stages its own outbox intents. The audit log then preserves
/// the full lineage: original commit, the
/// `compensation_transition_id` linkage on the failed outbox row,
/// and the compensation's audit row.
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
    /// A state-mutating helper returned [`OutboxUpdate::LeaseLost`].
    /// The deliverer (or the compensation arm) ran to completion,
    /// but by the time the processor went to write the result the
    /// lease had already expired and another worker had reclaimed
    /// the row.
    ///
    /// LeaseLost is NOT an error. It is the honest answer when a
    /// slow deliverer races the lease clock. Calling code should
    /// log + alert (the orphan-audit case during compensation -
    /// where the compensating transformation committed but the
    /// row's pointer never landed - is the most operationally
    /// noteworthy variety; reconcile from the audit log) and move
    /// on to the next row.
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
/// Concurrency: safe under concurrent invocation across processes.
/// `claim_pending_outbox_row` uses `SELECT ... FOR UPDATE SKIP
/// LOCKED` so at most one worker claims a given row;
/// `begin_compensation` uses the same pattern so at most one worker
/// invokes the compensating transformation for a given failed row.
///
/// The compensation race is closed under normal operation. A
/// crashing worker between `propose_against_pg` commit and
/// `complete_compensation` would leave the row in
/// `compensation_in_progress`; the conservative reclaim policy
/// (see [`begin_compensation`]'s docs) keeps such rows stuck and
/// requires operator intervention rather than risking a duplicate
/// compensation. Programs that need full immunity should
/// additionally guard the compensating transformation with a
/// `CompensationApplied(original_intent_id)` invariant.
///
/// `claim_before` is the upper bound for retry eligibility passed
/// through to [`claim_pending_outbox_row`]. One-shot callers (a
/// Lambda invocation, a CLI consumer) pass `Utc::now()`. A drain
/// loop captures `Utc::now()` once at the top of the pass and
/// supplies the same instant on every iteration; see
/// [`claim_pending_outbox_row`] for the loop-safety rationale.
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
    let row =
        match claim_pending_outbox_row(pool, worker_id, intent_type, lease_duration, claim_before)
            .await?
        {
            Some(r) => r,
            None => return Ok(ProcessOutcome::NoRowAvailable),
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
                    // Row is no longer in our hands. Whatever the
                    // new lease holder does next replaces what we
                    // were trying to do. Compensation must not
                    // run: begin_compensation requires status =
                    // 'failed', and we never moved the row there.
                    return Ok(ProcessOutcome::LeaseLost { intent_id });
                }
                OutboxUpdate::Applied => {}
            }
            let Some(spec) = compensation else {
                return Ok(ProcessOutcome::Failed { intent_id, reason });
            };
            // The lease was just released by mark_outbox_failed.
            // Re-claim the right to compensate via the
            // failed -> compensation_in_progress lease. Another
            // worker may beat us here under a concurrent recovery
            // scan; SKIP LOCKED ensures at most one wins.
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

    /// Pins the SQLSTATE used to identify PostgreSQL SSI serialization
    /// failures. If anyone changes the magic string `"40001"` this test
    /// fails — the retry contract cannot regress silently.
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
