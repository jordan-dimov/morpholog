//! The durable half of the discipline contract: a generated invariant
//! rejects atomically against real PostgreSQL, and a committed
//! transition's audit row lists the generated invariant - under its
//! traceable name - among the rules that governed admission.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{reset_db, test_pool};

use common::{dec, subj};
use morpholog_core::Program;
use morpholog_postgres::{PgProposalOutcome, list_audit_rows};
use morpholog_surface::parse_program;

const LEDGER: &str = r#"
program discipline_ledger

predicate Entry(entry_id: Subject, amount: Decimal)
    unique by (entry_id)
    append only

transformation post(entry_id, amount):
    admit Entry(entry_id, amount)
"#;

fn ledger() -> Program {
    let p = parse_program(LEDGER).expect("parses");
    p.validate().expect("validates");
    p
}

#[tokio::test]
async fn a_generated_invariant_rejects_durably_and_signs_the_audit_row() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = ledger();
    let post = p.transformation("post").unwrap();

    // The first entry commits, and its audit row records that the
    // generated invariant governed the admission - the declaration's
    // commitment, named in the permanent record.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &common::compiled(p.clone()),
        post,
        vec![subj("e1"), dec(100)],
    )
    .await
    .expect("propose should not error");
    assert!(matches!(outcome, PgProposalOutcome::Committed { .. }));
    let audit = list_audit_rows(&pool).await.expect("audit readable");
    assert!(
        audit[0]
            .invariants_checked
            .iter()
            .any(|c| c.name == "entry_unique_by_entry_id"),
        "the audit row names the generated invariant: {:?}",
        audit[0].invariants_checked
    );

    // A conflicting entry under the same id is refused atomically:
    // rejected with the generated name, no claims changed, no second
    // audit row.
    let outcome = common::propose_pg_with_test_actor(
        &pool,
        &common::compiled(p.clone()),
        post,
        vec![subj("e1"), dec(999)],
    )
    .await
    .expect("propose should not error");
    match outcome {
        PgProposalOutcome::Rejected { reason, .. } => {
            assert!(reason.contains("entry_unique_by_entry_id"), "got: {reason}")
        }
        other @ PgProposalOutcome::Committed { .. } => {
            panic!("a duplicate entry id must be refused, got {other:?}")
        }
    }
    let audit = list_audit_rows(&pool).await.expect("audit readable");
    assert_eq!(audit.len(), 1, "a rejection writes no audit row");
}
