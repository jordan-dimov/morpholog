//! Spike step 5 smoke: the compiled propose path commits and rejects on
//! the ledger exactly like the interpreted path, at both stages.

mod common;

use common::{dec, expect_committed, propose_pg_with_test_actor, reset_db, subj, test_pool};
use morpholog_core::EvalValue;
use morpholog_examples::double_entry_ledger;
use morpholog_postgres::spike::{Stage, compile_invariants, propose_against_pg_compiled};
use morpholog_postgres::{PgPool, PgProposalOutcome};

async fn propose_compiled(
    pool: &PgPool,
    transformation: &morpholog_core::Transformation,
    args: Vec<EvalValue>,
    stage: Stage,
) -> PgProposalOutcome {
    let compiled = common::compiled(double_entry_ledger::program());
    let sql_set = compile_invariants(compiled.program()).expect("ledger compiles");
    let transition = morpholog_test_support::test_transition(transformation, args);
    propose_against_pg_compiled(pool, &compiled, &sql_set, &common::attested(&transition), stage)
        .await
        .expect("no operational error")
}

fn balanced_args(id: &str) -> Vec<EvalValue> {
    vec![
        subj(id),
        subj("d1"),
        subj("p1"),
        subj("cash"),
        subj("rev"),
        dec(100),
    ]
}

fn unbalanced_split(id: &str) -> Vec<EvalValue> {
    vec![
        subj(id),
        subj("d1"),
        subj("p1"),
        subj("cash"),
        dec(100),
        subj("pay"),
        dec(60),
        subj("tax"),
        dec(30),
    ]
}

async fn claim_count(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM morpholog.claims")
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn compiled_path_commits_balanced_entries_at_both_stages() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let t = double_entry_ledger::post_simple_entry();

    for (stage, id) in [(Stage::Stage1, "e_s1"), (Stage::Stage2, "e_s2")] {
        let outcome = propose_compiled(&pool, &t, balanced_args(id), stage).await;
        let PgProposalOutcome::Committed { transition_id, .. } = outcome else {
            panic!("balanced entry must commit at {stage:?}");
        };
        // The audit row exists inside the same commit.
        let audited: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM morpholog.audit WHERE transition_id = $1",
        )
        .bind(transition_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(audited, 1);
    }
    // Two entries x three claims.
    assert_eq!(claim_count(&pool).await, 6);
}

#[tokio::test]
async fn compiled_rejection_matches_interpreted_rejection_and_rolls_back() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let t = double_entry_ledger::post_split_entry();
    let compiled = common::compiled(double_entry_ledger::program());

    // Interpreted verdict on the same state.
    let interpreted = propose_pg_with_test_actor(&pool, &compiled, &t, unbalanced_split("e_bad"))
        .await
        .unwrap();
    let PgProposalOutcome::Rejected {
        reason: i_reason,
        rule: i_rule,
        witness: i_witness,
    } = interpreted
    else {
        panic!("interpreted must reject");
    };

    let before = claim_count(&pool).await;
    for stage in [Stage::Stage1, Stage::Stage2] {
        let outcome = propose_compiled(&pool, &t, unbalanced_split("e_bad"), stage).await;
        let PgProposalOutcome::Rejected {
            reason,
            rule,
            witness,
        } = outcome
        else {
            panic!("compiled must reject at {stage:?}");
        };
        assert_eq!(reason, i_reason, "reason parity at {stage:?}");
        assert_eq!(rule, i_rule, "rule parity at {stage:?}");
        assert_eq!(witness, i_witness, "witness parity at {stage:?}");
        assert_eq!(claim_count(&pool).await, before, "rollback at {stage:?}");
    }

    // Each rejection also reached the rejection log (1 interpreted + 2 compiled).
    let logged: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM morpholog.rejections WHERE rule = 'balanced_posted_entry'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(logged, 3);
}

#[tokio::test]
async fn body_gate_rejection_flows_identically() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let t = double_entry_ledger::close_period();
    expect_committed(propose_compiled(&pool, &t, vec![subj("p9")], Stage::Stage2).await);

    // Re-closing gates on `require not PeriodClosed(period)`.
    let outcome = propose_compiled(&pool, &t, vec![subj("p9")], Stage::Stage2).await;
    let PgProposalOutcome::Rejected { reason, .. } = outcome else {
        panic!("re-close must reject at the gate");
    };
    assert!(reason.contains("require"), "gate rejection, not invariant: {reason}");
}
