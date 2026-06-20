//! The rejection log's recording contract: a lawful rejection writes
//! exactly one row to `morpholog.rejections` with the kind, rule, and
//! version taken from the structured reason - while every other
//! outcome (commit, kernel error, PG-layer error) writes nothing.
//! The record lands AFTER the refusing transaction rolled back, so
//! these tests are also the proof that the post-rollback insert
//! actually runs on every public propose path.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{reset_db, test_pool};

use common::{dec, subj, test_transition};
use morpholog_core::Program;
use morpholog_postgres::{
    PgPool, PgProposalOutcome, PgTracedOutcome, propose_against_pg_with_rejection_state,
};
use morpholog_surface::parse_program;

const FIXTURE: &str = r#"
program rejection_log_fixture

predicate Entry(entry_id: Subject, amount: Decimal)
    unique by (entry_id)
predicate Approved(entry_id: Subject)

transformation post(entry_id, amount):
    admit Entry(entry_id, amount)

transformation post_approved(entry_id, amount):
    require Approved(entry_id)
    admit Entry(entry_id, amount)

transformation repost(entry_id, amount):
    bind Entry(entry_id, prior)
    retract Entry(entry_id, prior)
    admit Entry(entry_id, amount)
"#;

fn fixture() -> Program {
    let p = parse_program(FIXTURE).expect("parses");
    p.validate().expect("validates");
    p
}

/// (transformation_name, kind, rule, invariant_version, reason,
/// arguments, actor) in `(rejected_at, rejection_id)` order - the
/// raw table contents, read without going through any adapter API.
type RejectionTuple = (
    String,
    String,
    String,
    Option<i64>,
    String,
    serde_json::Value,
    serde_json::Value,
);

async fn rejection_rows(pool: &PgPool) -> Vec<RejectionTuple> {
    sqlx::query_as(
        "SELECT transformation_name, kind, rule, invariant_version, reason, arguments, actor
         FROM morpholog.rejections
         ORDER BY rejected_at, rejection_id",
    )
    .fetch_all(pool)
    .await
    .expect("rejections readable")
}

#[tokio::test]
async fn an_invariant_rejection_writes_one_structured_row() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = fixture();
    let post = p.transformation("post").unwrap();

    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &common::compiled(p.clone()),
        post,
        vec![subj("e1"), dec(100)],
    )
    .await
    .expect("first post commits");
    assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));

    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &common::compiled(p.clone()),
        post,
        vec![subj("e1"), dec(999)],
    )
    .await
    .expect("rejection is a lawful outcome, not an error");
    let PgProposalOutcome::Rejected { reason } = outcome else {
        panic!("a conflicting entry id must be refused");
    };

    let rows = rejection_rows(&pool).await;
    assert_eq!(rows.len(), 1, "exactly one rejection row");
    let (name, kind, rule, version, recorded_reason, args, actor) = &rows[0];
    assert_eq!(name, "post");
    assert_eq!(kind, "invariant");
    assert_eq!(rule, "entry_unique_by_entry_id");
    assert_eq!(*version, Some(1), "generated discipline invariants are v1");
    assert_eq!(
        recorded_reason, &reason,
        "the recorded reason is the exact envelope string"
    );
    assert_eq!(
        args,
        &serde_json::json!([
            {"type": "subject", "value": "e1"},
            {"type": "decimal", "value": "999"}
        ]),
        "arguments round-trip through the same codec as audit"
    );
    assert_eq!(
        actor,
        &serde_json::json!({"type": "subject", "value": "test_actor"})
    );
}

#[tokio::test]
async fn a_require_rejection_records_the_gate_kind_and_rendered_rule() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = fixture();
    let post_approved = p.transformation("post_approved").unwrap();

    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &common::compiled(p.clone()),
        post_approved,
        vec![subj("e1"), dec(100)],
    )
    .await
    .expect("rejection is a lawful outcome");
    assert!(matches!(outcome, PgProposalOutcome::Rejected { .. }));

    let rows = rejection_rows(&pool).await;
    assert_eq!(rows.len(), 1);
    let (_, kind, rule, version, _, _, _) = &rows[0];
    assert_eq!(kind, "require");
    assert_eq!(rule, "Approved(entry_id)");
    assert_eq!(*version, None, "gate kinds carry no invariant version");
}

#[tokio::test]
async fn a_bind_rejection_records_the_bind_kind() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = fixture();
    let repost = p.transformation("repost").unwrap();

    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &common::compiled(p.clone()),
        repost,
        vec![subj("missing"), dec(5)],
    )
    .await
    .expect("rejection is a lawful outcome");
    assert!(matches!(outcome, PgProposalOutcome::Rejected { .. }));

    let rows = rejection_rows(&pool).await;
    assert_eq!(rows.len(), 1);
    let (_, kind, rule, version, _, _, _) = &rows[0];
    assert_eq!(kind, "bind");
    assert_eq!(rule, "Entry(entry_id, prior)");
    assert_eq!(*version, None);
}

#[tokio::test]
async fn commits_and_kernel_errors_write_no_rejection_row() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = fixture();
    let post = p.transformation("post").unwrap();

    // A commit records nothing here - it records in audit.
    let _ = common::propose_pg_with_test_actor(
        &pool,
        &common::compiled(p.clone()),
        post,
        vec![subj("e1"), dec(1)],
    )
    .await
    .expect("commits");
    let _ = common::propose_pg_with_test_actor(
        &pool,
        &common::compiled(p.clone()),
        post,
        vec![subj("e2"), dec(2)],
    )
    .await
    .expect("commits");

    // A kernel error is not a rejection: a `bind` that leaves both
    // columns free now sees two candidate entries - the multi-match
    // EvalError path, which must never reach the rejection insert.
    use morpholog_core::ir_builder::{bind_one, claim, transformation, var};
    let sweep = transformation(
        "sweep",
        vec![],
        vec![bind_one(claim("Entry", vec![var("eid"), var("amt")]))],
    );
    // `sweep` must live in the proposed programme for the facade to reach
    // the kernel (an unknown name would error before evaluation), so add
    // it to the fixture programme.
    let mut with_sweep = fixture();
    with_sweep.transformations.push(sweep.clone());
    common::propose_pg_with_test_actor(&pool, &common::compiled(with_sweep), &sweep, vec![])
        .await
        .expect_err("two candidates must surface a kernel error");

    assert!(
        rejection_rows(&pool).await.is_empty(),
        "neither commits nor kernel errors write rejection rows"
    );
}

#[tokio::test]
async fn a_pg_layer_error_writes_no_rejection_row() {
    use morpholog_core::ir_builder::{emit, subj as lit_subj, transformation};

    let pool = test_pool().await;
    reset_db(&pool).await;

    // Two identical intents collide on the deterministic idempotency
    // key inside write_accepted - a PG-layer error on the ACCEPTED
    // path, the proxy proof that Err(PgError) paths never reach the
    // rejection insert.
    let double_emit = transformation(
        "double_emit",
        vec![],
        vec![
            emit("Ping", vec![lit_subj("p")]),
            emit("Ping", vec![lit_subj("p")]),
        ],
    );
    let prog = {
        use morpholog_core::ir_builder::program;
        use morpholog_core::{ArgDecl, IntentDecl, PredicateArgKind};
        program("dup_emit")
            .intents(vec![IntentDecl {
                name: "Ping".into(),
                args: vec![ArgDecl {
                    name: "p".into(),
                    kind: PredicateArgKind::Subject,
                }],
            }])
            .transformations(vec![double_emit.clone()])
            .build()
    };
    common::propose_pg_with_test_actor(&pool, &common::compiled(prog), &double_emit, vec![])
        .await
        .expect_err("duplicate intents must error");

    assert!(rejection_rows(&pool).await.is_empty());
}

#[tokio::test]
async fn the_trace_and_rejection_state_paths_each_record_exactly_once() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = fixture();
    let post_approved = p.transformation("post_approved").unwrap();

    let traced = common::propose_pg_with_trace_using_test_actor(
        &pool,
        &common::compiled(p.clone()),
        post_approved,
        vec![subj("t1"), dec(1)],
    )
    .await
    .expect("rejection is lawful");
    assert!(matches!(
        traced,
        PgTracedOutcome::Outcome {
            outcome: PgProposalOutcome::Rejected { .. },
            ..
        }
    ));
    assert_eq!(rejection_rows(&pool).await.len(), 1);

    // The explain-on-reject path goes through with_rejection_state -
    // one call, one row, no double recording.
    let transition = test_transition(post_approved, vec![subj("t2"), dec(2)]);
    let result =
        propose_against_pg_with_rejection_state(&pool, &common::compiled(p.clone()), &transition)
            .await
            .expect("rejection is lawful");
    assert!(matches!(result.outcome, PgProposalOutcome::Rejected { .. }));
    assert!(
        result.rejection_state.is_some(),
        "the rejecting state is handed back"
    );
    assert_eq!(rejection_rows(&pool).await.len(), 2);
}

#[tokio::test]
async fn sequential_rejections_each_record_in_replay_order() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = fixture();
    let post_approved = p.transformation("post_approved").unwrap();
    let repost = p.transformation("repost").unwrap();

    for (t, eid) in [(post_approved, "a"), (repost, "b"), (post_approved, "c")] {
        let outcome = common::propose_pg_with_test_actor(
            &pool,
            &common::compiled(p.clone()),
            t,
            vec![subj(eid), dec(1)],
        )
        .await
        .expect("rejection is lawful");
        assert!(matches!(outcome, PgProposalOutcome::Rejected { .. }));
    }

    let rows = rejection_rows(&pool).await;
    let kinds: Vec<&str> = rows.iter().map(|r| r.1.as_str()).collect();
    assert_eq!(
        kinds,
        vec!["require", "bind", "require"],
        "each batch-style sequential rejection records, in order"
    );
}

// The kind/version agreement is enforced by the table itself, not
// only by the writer: a versioned gate or an unversioned invariant
// cannot be inserted even by hand - the operational evidence is hard
// to corrupt manually.
#[tokio::test]
async fn the_table_refuses_kind_version_disagreement() {
    let pool = test_pool().await;
    reset_db(&pool).await;

    for (kind, version) in [("require", Some(2_i64)), ("invariant", None)] {
        let result = sqlx::query(
            "INSERT INTO morpholog.rejections (
                rejection_id, transformation_name, arguments, actor,
                kind, rule, invariant_version, reason
             ) VALUES ($1, $2, '[]'::jsonb, '{\"type\":\"subject\",\"value\":\"x\"}'::jsonb,
                       $3, 'r', $4, 'reason')",
        )
        .bind(uuid::Uuid::now_v7())
        .bind("hand_written")
        .bind(kind)
        .bind(version)
        .execute(&pool)
        .await;
        let err = result.expect_err("kind/version disagreement must be refused");
        assert!(
            err.to_string().contains("rejections_kind_version_agree"),
            "the CHECK constraint names itself: {err}"
        );
    }
}
