//! Spike for as-of evaluation. Pairs with `docs/as-of-sketch.md`.
//!
//! This test exists to demonstrate the gap and pin the target
//! behaviour, not to exercise a kernel API. There is currently no
//! Morpholog-native way to ask "what did the derived view look like
//! at past transition T?" - every caller has to write the
//! `reconstruct_state_at` helper themselves. This file is that
//! helper, wrapped in one end-to-end test. Once the implementation
//! PR lands, `reconstruct_state_at` (or whatever it ends up named)
//! moves into `morpholog-postgres` proper and this file is either
//! retired or kept as a black-box equivalence check against the
//! production helper.
//!
//! The scenario uses the double-entry ledger:
//!
//! 1. Post `entry_001` at amount 100.
//! 2. Post `entry_002` at amount 200.
//! 3. Restate `entry_001` to amount 150 (creates `entry_001_v2`,
//!    asserts `Supersedes(entry_001_v2, entry_001)`; does NOT retract
//!    the original entry or lines - history is append-only).
//!
//! Three distinct trial-balance answers exist in the same database,
//! and only the last one is reachable through the current
//! `list_derived` API.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::examples::double_entry_ledger;
use morpholog_core::{ClaimInstance, EvalValue, State, enumerate_derived};
use morpholog_postgres::{PgPool, PgProposalOutcome, list_derived, propose_against_pg};
use rust_decimal::Decimal;
use uuid::Uuid;

// ============================================================
// Test infrastructure (small copy of the integration.rs helpers;
// cargo treats each tests/*.rs as a separate compilation unit, so
// shared use without a `common/mod.rs` requires duplication)
// ============================================================

async fn test_pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must be set for morpholog-postgres integration tests \
         (e.g. postgres:///morpholog_dev)",
    );
    PgPool::connect(&url)
        .await
        .expect("failed to connect to PostgreSQL test database")
}

async fn reset_db(pool: &PgPool) {
    sqlx::query("TRUNCATE morpholog.outbox, morpholog.claims, morpholog.audit CASCADE")
        .execute(pool)
        .await
        .expect("failed to truncate test DB");
}

fn subj(s: &str) -> EvalValue {
    EvalValue::Subject(s.to_string())
}

fn dec(n: i64) -> EvalValue {
    EvalValue::Decimal(Decimal::new(n, 0))
}

/// The hand-rolled gap. Every caller wanting as-of evaluation today
/// has to write something equivalent to this: scan the audit table
/// up to the chosen `transition_id`, apply each transition's
/// asserted and retracted claims in causal order, return the
/// reconstructed `State`.
///
/// The implementation PR's job is to move this (or its better-shaped
/// equivalent) into `morpholog-postgres` proper, with all the
/// open-question answers pinned: coordinate type, inclusive/exclusive
/// of `as_of`, ordering by `committed_at` vs `transition_id`, error
/// behaviour for unknown ids, interaction with predicate-scoped
/// loading, etc.
///
/// Caveats this spike does not address:
/// - Uses `transition_id <= $1` rather than `(committed_at, transition_id) <= ...`.
///   For a single-client test with sequential commits this gives the
///   right answer (UUIDv7's bytes are timestamp-prefixed and
///   byte-wise compare is time-ordered for a single generator), but
///   under concurrent commits the two predicates can diverge.
/// - Unknown `as_of` returns whatever the SQL query finds (probably
///   the empty state or the full state depending on whether the
///   supplied UUID is smaller or larger than every real one). The
///   implementation PR should reject unknown ids explicitly.
/// - Replays the whole audit log every call. No materialisation.
async fn reconstruct_state_at(pool: &PgPool, as_of: Uuid) -> State {
    type Row = (Uuid, serde_json::Value, serde_json::Value);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT transition_id, asserted_claims, retracted_claims
         FROM morpholog.audit
         WHERE transition_id <= $1
         ORDER BY committed_at, transition_id",
    )
    .bind(as_of)
    .fetch_all(pool)
    .await
    .unwrap();

    let mut claims: Vec<ClaimInstance> = Vec::new();
    for (_, asserted_json, retracted_json) in rows {
        let asserted: Vec<ClaimInstance> = serde_json::from_value(asserted_json).unwrap();
        let retracted: Vec<ClaimInstance> = serde_json::from_value(retracted_json).unwrap();

        // Retractions first: each retracted claim must be present
        // (the kernel's invariant), so we remove it before adding
        // new claims in the same transition.
        for r in &retracted {
            claims.retain(|c| c != r);
        }
        // Assertions are set-valued: asserting an already-present
        // claim is an idempotent no-op (matches the PG adapter's
        // INSERT ... ON CONFLICT DO NOTHING).
        for a in &asserted {
            if !claims.iter().any(|c| c == a) {
                claims.push(a.clone());
            }
        }
    }
    State::from_claims(claims)
}

// ============================================================
// The spike
// ============================================================

#[tokio::test]
async fn as_of_spike_recovers_historical_trial_balance_before_restatement() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    let invariants = double_entry_ledger::all_invariants();
    let period = subj("p_bench");

    // Step 1: post entry_001 at amount 100. Cash debit, revenue credit.
    let tid1 = expect_committed(
        propose_against_pg(
            &pool,
            &double_entry_ledger::post_simple_entry(),
            vec![
                subj("entry_001"),
                subj("d_2026_05_01"),
                period.clone(),
                subj("account_cash"),
                subj("account_revenue"),
                dec(100),
            ],
            &invariants,
        )
        .await
        .unwrap(),
    );

    // Step 2: post entry_002 at amount 200. Same accounts, same period.
    let tid2 = expect_committed(
        propose_against_pg(
            &pool,
            &double_entry_ledger::post_simple_entry(),
            vec![
                subj("entry_002"),
                subj("d_2026_05_02"),
                period.clone(),
                subj("account_cash"),
                subj("account_revenue"),
                dec(200),
            ],
            &invariants,
        )
        .await
        .unwrap(),
    );

    // Step 3: restate entry_001 to amount 150. The restatement does
    // not retract the original entry or its lines (history is
    // append-only), so the current trial balance picks up both the
    // original 100 AND the restated 150 for entry_001's accounts.
    let tid3 = expect_committed(
        propose_against_pg(
            &pool,
            &double_entry_ledger::restate_entry(),
            vec![
                subj("entry_001_v2"),
                subj("entry_001"),
                subj("d_2026_05_10"),
                period.clone(),
                subj("account_cash"),
                subj("account_revenue"),
                dec(150),
            ],
            &invariants,
        )
        .await
        .unwrap(),
    );

    let trial_balance = double_entry_ledger::trial_balance_row();

    // Current state: trial balance reflects the post-restatement
    // reality. Cash gets debit 100 (original) + 200 (entry_002) + 150
    // (restated) = 450. Revenue gets credit -450.
    let current = list_derived(&pool, &trial_balance).await.unwrap();
    assert_balance(&current, "account_cash", 450);
    assert_balance(&current, "account_revenue", -450);

    // As-of `tid2`: state right after entry_002 was posted, before the
    // restatement. Cash debit 100 + 200 = 300. Revenue credit -300.
    let state_at_tid2 = reconstruct_state_at(&pool, tid2).await;
    let at_tid2 = enumerate_derived(&trial_balance, &state_at_tid2).unwrap();
    assert_balance(&at_tid2, "account_cash", 300);
    assert_balance(&at_tid2, "account_revenue", -300);

    // As-of `tid1`: only the first entry has committed. Cash 100,
    // revenue -100.
    let state_at_tid1 = reconstruct_state_at(&pool, tid1).await;
    let at_tid1 = enumerate_derived(&trial_balance, &state_at_tid1).unwrap();
    assert_balance(&at_tid1, "account_cash", 100);
    assert_balance(&at_tid1, "account_revenue", -100);

    // As-of `tid3`: replay reaches the same final state as
    // `list_derived` (the current view). Pins the property that
    // "as of the most recent transition" equals "now".
    let state_at_tid3 = reconstruct_state_at(&pool, tid3).await;
    let at_tid3 = enumerate_derived(&trial_balance, &state_at_tid3).unwrap();
    assert_eq!(
        at_tid3, current,
        "as-of the latest transition must match the live derived enumeration"
    );

    // The interesting property: the historical answer at tid2 is not
    // recoverable from the current state alone; the restatement
    // changed the visible balance, but the audit log preserved
    // enough information to reconstruct the pre-restatement view.
    assert_ne!(
        current, at_tid2,
        "current and pre-restatement trial balances must differ; \
         that difference is what as-of evaluation makes addressable"
    );
}

/// Helper: unwrap a `Committed` outcome and return its
/// `transition_id`, panicking with a useful message if the
/// transformation rejected. Each step of the chain is required to
/// commit; a rejection here is a fixture or kernel bug, not a
/// business outcome.
fn expect_committed(outcome: PgProposalOutcome) -> Uuid {
    match outcome {
        PgProposalOutcome::Committed { transition_id, .. } => transition_id,
        PgProposalOutcome::Rejected { reason } => {
            panic!("expected Committed; got Rejected({reason})")
        }
    }
}

/// Helper: find the `TrialBalanceRow` row for `account_name` in
/// `rows` and assert its balance matches the expected `amount`
/// (interpreted as a plain integer decimal). Distinct from
/// `assert_eq!` on the full row set because the bench cares about
/// the per-account values, not the row ordering or the presence of
/// other accounts (there should not be any in this fixture, but the
/// test is robust to it).
fn assert_balance(rows: &[ClaimInstance], account_name: &str, amount: i64) {
    let account = EvalValue::Subject(account_name.to_string());
    let expected = EvalValue::Decimal(Decimal::new(amount, 0));
    let row = rows
        .iter()
        .find(|r| r.predicate == "TrialBalanceRow" && r.args.first() == Some(&account));
    let row = row
        .unwrap_or_else(|| panic!("no TrialBalanceRow for account `{account_name}` in {rows:?}"));
    assert_eq!(
        row.args.get(1),
        Some(&expected),
        "balance for {account_name} did not match expected {amount}: row was {row:?}"
    );
}
