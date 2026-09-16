//! What a proposal-path error says about whether anything committed.
//! Attacker capability: none - these are infrastructure failures, and
//! the claim under test is that each is named for what it is: a known
//! non-commit, or a decided-but-unrecorded rejection.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_postgres::{PgError, PgPool, PgProposalOutcome};
use morpholog_surface::parse_program;

mod common;
use common::{reset_db, test_pool};

async fn try_entry(pool: &PgPool, id: &str) -> Result<PgProposalOutcome, PgError> {
    let compiled = common::compiled(morpholog_examples::double_entry_ledger::program());
    let t = morpholog_examples::double_entry_ledger::post_simple_entry();
    common::propose_pg_with_test_actor(
        pool,
        &compiled,
        &t,
        vec![
            morpholog_test_support::subj(id),
            morpholog_test_support::subj("d_2026_05_17"),
            morpholog_test_support::subj("p1"),
            morpholog_test_support::subj(&format!("cash_{id}")),
            morpholog_test_support::subj(&format!("rev_{id}")),
            morpholog_test_support::dec(100),
        ],
    )
    .await
}

/// A gated admission whose gate the second call fails: the rejection
/// phase's lawful refusal.
const GATED: &str = r#"
program commit_boundary_fixture

predicate Entry(entry_id: Subject, amount: Decimal)
    unique by (entry_id)

transformation post(entry_id, amount):
    admit Entry(entry_id, amount)
"#;

async fn try_gated(pool: &PgPool, id: &str, amount: i64) -> Result<PgProposalOutcome, PgError> {
    let program = parse_program(GATED).unwrap();
    program.validate().unwrap();
    let post = program.transformation("post").unwrap().clone();
    common::propose_pg_with_test_actor(
        pool,
        &common::compiled(program),
        &post,
        vec![
            morpholog_test_support::subj(id),
            morpholog_test_support::dec(amount),
        ],
    )
    .await
}

async fn ddl(pool: &PgPool, sql: &'static str) {
    sqlx::raw_sql(sql).execute(pool).await.expect(sql);
}

async fn audit_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM morpholog.audit")
        .fetch_one(pool)
        .await
        .unwrap()
}

/// One test, three phases, because each phase alters the shared schema
/// and the phases must not overlap: a failure writing the delta, a
/// server error answered at COMMIT, and a rejection the log cannot
/// record. Each is an ordinary error with a definite meaning - never
/// the commit-unknown variant, which only a lost server verdict earns.
#[tokio::test]
async fn proposal_path_errors_say_what_they_know() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    ddl(
        &pool,
        "DROP TRIGGER IF EXISTS probe_at_commit ON morpholog.audit",
    )
    .await;
    ddl(&pool, "DROP FUNCTION IF EXISTS probe_refuse()").await;
    ddl(
        &pool,
        "ALTER TABLE morpholog.audit DROP CONSTRAINT IF EXISTS probe",
    )
    .await;
    ddl(
        &pool,
        "ALTER TABLE morpholog.rejections DROP CONSTRAINT IF EXISTS probe",
    )
    .await;

    // Phase 1: the delta write itself is refused by the server (a CHECK
    // on the audit table). Nothing was committed, and the caller is
    // told so by an ordinary database error.
    ddl(
        &pool,
        "ALTER TABLE morpholog.audit ADD CONSTRAINT probe CHECK (false)",
    )
    .await;
    let err = try_entry(&pool, "e1").await.unwrap_err();
    assert!(
        matches!(err, PgError::Database(_)),
        "a known non-commit: {err}"
    );
    assert_eq!(audit_rows(&pool).await, 0);
    ddl(&pool, "ALTER TABLE morpholog.audit DROP CONSTRAINT probe").await;
    assert!(
        matches!(
            try_entry(&pool, "e1").await.unwrap(),
            PgProposalOutcome::Committed { .. }
        ),
        "the pool is healthy afterwards"
    );

    // Phase 2: the delta writes cleanly and the server refuses at
    // COMMIT (a deferred constraint trigger). PostgreSQL answered, so
    // the transaction is rolled back: a known non-commit, not unknown.
    ddl(
        &pool,
        "CREATE FUNCTION probe_refuse() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN RAISE EXCEPTION 'refused at commit'; END $$",
    )
    .await;
    ddl(
        &pool,
        "CREATE CONSTRAINT TRIGGER probe_at_commit AFTER INSERT ON morpholog.audit
         DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION probe_refuse()",
    )
    .await;
    let before = audit_rows(&pool).await;
    let err = try_entry(&pool, "e2").await.unwrap_err();
    assert!(
        matches!(&err, PgError::Database(e) if e.to_string().contains("refused at commit")),
        "the server's own verdict at COMMIT is a known non-commit: {err}"
    );
    assert_eq!(
        audit_rows(&pool).await,
        before,
        "nothing survived the refused COMMIT"
    );
    ddl(&pool, "DROP TRIGGER probe_at_commit ON morpholog.audit").await;
    ddl(&pool, "DROP FUNCTION probe_refuse()").await;
    assert!(matches!(
        try_entry(&pool, "e2").await.unwrap(),
        PgProposalOutcome::Committed { .. }
    ));

    // Phase 3: a lawful rejection whose operational record cannot be
    // written. The verdict was decided and rolled back; the failure
    // carries its own provenance so no surface can call it a
    // pre-decision failure.
    assert!(matches!(
        try_gated(&pool, "g1", 1).await.unwrap(),
        PgProposalOutcome::Committed { .. }
    ));
    ddl(
        &pool,
        "ALTER TABLE morpholog.rejections ADD CONSTRAINT probe CHECK (false)",
    )
    .await;
    let err = try_gated(&pool, "g1", 2).await.unwrap_err();
    assert!(
        matches!(err, PgError::RejectionLogFailure(_)),
        "decided, unrecorded: {err}"
    );
    ddl(
        &pool,
        "ALTER TABLE morpholog.rejections DROP CONSTRAINT probe",
    )
    .await;
    assert!(matches!(
        try_gated(&pool, "g1", 2).await.unwrap(),
        PgProposalOutcome::Rejected { .. }
    ));
}
