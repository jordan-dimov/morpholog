//! Morpholog PostgreSQL persistence adapter.
//!
//! Thin async I/O layer around the existing synchronous
//! [`morpholog_core::propose`] kernel. The kernel itself is unchanged.
//!
//! See `crates/morpholog-core/sql/schema.sql` for the canonical schema
//! and `docs/scope-and-ambition.md` for the runtime's positioning.

use chrono::{DateTime, Utc};
use morpholog_core::{
    ClaimInstance, DerivedClaim, EvalError, EvalValue, IntentInstance, Invariant, Outcome, State,
    Transformation, enumerate_derived, predicates_referenced_by_derived, propose,
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
pub async fn propose_against_pg(
    pool: &PgPool,
    transformation: &Transformation,
    args: Vec<EvalValue>,
    invariants: &[Invariant],
) -> Result<PgProposalOutcome, PgError> {
    let mut tx = pool.begin().await.map_err(classify)?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await
        .map_err(classify)?;

    let state = load_state(&mut tx).await?;
    let outcome = propose(transformation, args.clone(), &state, invariants)?;

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
                &args,
                invariants,
                &asserted_claims,
                &retracted_claims,
                &emitted_intents,
            )
            .await?;
            tx.commit().await.map_err(classify)?;
            Ok(PgProposalOutcome::Committed {
                transition_id,
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

async fn load_state(tx: &mut Transaction<'_, Postgres>) -> Result<State, PgError> {
    let rows: Vec<(String, serde_json::Value)> =
        sqlx::query_as("SELECT predicate_name, arguments FROM morpholog.claims")
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

/// One entry in an audit row's `invariants_checked` JSONB array. Recorded
/// per committed transformation: the invariant `name` plus the `version`
/// active at admission time. Self-describing audit data is preferred over
/// tuple compactness.
///
/// `Serialize` is derived so the CLI can re-emit audit rows as JSON
/// without an intermediate hand-rolled mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvariantCheck {
    pub name: String,
    pub version: u32,
}

#[allow(clippy::too_many_arguments)]
async fn write_accepted(
    tx: &mut Transaction<'_, Postgres>,
    transition_id: Uuid,
    transformation: &Transformation,
    args: &[EvalValue],
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
    let checked: Vec<InvariantCheck> = invariants
        .iter()
        .map(|inv| InvariantCheck {
            name: inv.name.clone(),
            version: inv.version,
        })
        .collect();
    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(transition_id)
    .bind(&transformation.name)
    .bind(serde_json::to_value(args)?)
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
    pub invariant_epoch: i32,
    pub invariants_checked: Vec<InvariantCheck>,
    pub asserted_claims: Vec<ClaimInstance>,
    pub retracted_claims: Vec<ClaimInstance>,
    pub emitted_intents: Vec<IntentInstance>,
    pub committed_at: DateTime<Utc>,
}

/// One row of `morpholog.outbox` decoded into typed runtime values.
///
/// `attempt_count` and `last_attempt_at` are included because retry
/// activity is part of the outbox contract — a `pending` row with
/// `attempt_count > 0` and a non-NULL `last_attempt_at` is one a
/// worker has tried and failed, not a fresh enqueue. Inspection
/// helpers should surface that signal.
///
/// `delivered_at` is excluded: [`list_pending_outbox`] filters to
/// `status = 'pending'`, and `delivered_at` is set only when status
/// transitions to `'delivered'`, so it is structurally NULL for
/// every row this helper returns. A future `list_all_outbox` or
/// per-status query would surface it.
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
        i32,
        serde_json::Value,
        serde_json::Value,
        serde_json::Value,
        serde_json::Value,
        DateTime<Utc>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT transition_id, transformation_name, arguments,
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
    type Row = (
        Uuid,
        Uuid,
        String,
        serde_json::Value,
        String,
        String,
        i32,
        DateTime<Utc>,
        Option<DateTime<Utc>>,
    );
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT intent_id, transition_id, intent_type, arguments,
                idempotency_key, status, attempt_count, enqueued_at,
                last_attempt_at
         FROM morpholog.outbox
         WHERE status = 'pending'
         ORDER BY enqueued_at, intent_id",
    )
    .fetch_all(pool)
    .await
    .map_err(classify)?;

    rows.into_iter()
        .map(
            |(
                intent_id,
                transition_id,
                intent_type,
                args_json,
                idempotency_key,
                status,
                attempt_count,
                enqueued_at,
                last_attempt_at,
            )| {
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
                })
            },
        )
        .collect()
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
