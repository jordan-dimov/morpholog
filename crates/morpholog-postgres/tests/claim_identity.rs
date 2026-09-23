//! A claim's identity is a digest of its arguments, so argument size is
//! never a limit the database imposes behind the model's back.
//!
//! A btree index row may not exceed 2704 bytes, so indexing the raw
//! arguments would refuse a long free-text subject with a raw database
//! error. The digest must hold for hostile text - quotes, backslashes,
//! non-ASCII, and enough entropy that compression cannot hide the size -
//! across admit, a repeat admit as a no-op, and retract.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{compiled, propose_pg_with_test_actor, reset_db, subj, test_pool};

use morpholog_core::Program;
use morpholog_postgres::{PgPool, PgProposalOutcome, VerifyOutcome, verify_replay};
use morpholog_surface::parse_program;
use std::fmt::Write as _;

const FIXTURE: &str = r#"
program claim_identity_fixture

predicate Statement(subject: Subject, statement: Subject)

transformation record(subject, statement):
    admit Statement(subject, statement)

transformation withdraw(subject, statement):
    require Statement(subject, statement)
    retract Statement(subject, statement)
"#;

fn fixture() -> Program {
    let p = parse_program(FIXTURE).expect("parses");
    p.validate().expect("validates");
    p
}

/// Text too long to index raw: a deterministic pseudo-random stream (so it
/// does not compress under the limit) salted with every awkward character
/// class the digest must carry intact.
fn hostile_statement(bytes: usize) -> String {
    let alphabet: Vec<char> =
        "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 \"\\/'ü€日\n\t"
            .chars()
            .collect();
    let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
    let mut out = String::new();
    while out.len() < bytes {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let idx = usize::try_from(seed % u64::try_from(alphabet.len()).unwrap()).unwrap();
        let _ = write!(out, "{}", alphabet[idx]);
    }
    out
}

async fn claim_rows(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM morpholog.claims WHERE predicate_name = 'Statement'")
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn propose(pool: &PgPool, p: &Program, name: &str, statement: &str) -> PgProposalOutcome {
    propose_pg_with_test_actor(
        pool,
        &compiled(p.clone()),
        p.transformation(name).unwrap(),
        vec![subj("case-17"), subj(statement)],
    )
    .await
    .expect("the proposal is decided, never refused by the substrate")
}

#[tokio::test]
async fn a_long_hostile_argument_is_admitted_once_and_retracted_by_identity() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = fixture();
    let statement = hostile_statement(4_000);
    assert!(
        statement.len() > 2_704,
        "anti-vacuity: the statement must exceed the btree row ceiling"
    );

    assert!(matches!(
        propose(&pool, &p, "record", &statement).await,
        PgProposalOutcome::Committed { .. }
    ));
    assert_eq!(claim_rows(&pool).await, 1);

    // Set semantics: the same claim again is a no-op, decided by identity.
    assert!(matches!(
        propose(&pool, &p, "record", &statement).await,
        PgProposalOutcome::Committed { .. }
    ));
    assert_eq!(claim_rows(&pool).await, 1, "an admitted claim admits once");

    // A statement differing in one byte is a different claim.
    let mut sibling = statement.clone();
    sibling.pop();
    sibling.push('!');
    assert!(matches!(
        propose(&pool, &p, "record", &sibling).await,
        PgProposalOutcome::Committed { .. }
    ));
    assert_eq!(claim_rows(&pool).await, 2);

    // Retraction finds the row by the same identity, and only that row.
    assert!(matches!(
        propose(&pool, &p, "withdraw", &statement).await,
        PgProposalOutcome::Committed { .. }
    ));
    assert_eq!(claim_rows(&pool).await, 1);
    let remaining: serde_json::Value = sqlx::query_scalar(
        "SELECT arguments -> 1 -> 'value' FROM morpholog.claims WHERE predicate_name = 'Statement'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(remaining, serde_json::Value::String(sibling));
}

/// The verify replay pages the claims table by its key, reading each row
/// once. Rows are inserted without audit entries on purpose, so the replay
/// reports each as present only in the table, and the count proves the
/// paging.
#[tokio::test]
async fn verify_pages_the_claims_table_by_its_key() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let rows: i64 = 1_500;
    sqlx::query(
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         SELECT 'Statement',
                jsonb_build_array(jsonb_build_object('type', 'subject', 'value', i::text)),
                gen_random_uuid()
         FROM generate_series(1, $1) AS i",
    )
    .bind(rows)
    .execute(&pool)
    .await
    .unwrap();

    match verify_replay(&pool).await.unwrap() {
        VerifyOutcome::Divergent {
            only_in_claims_table,
            only_in_replay,
        } => {
            assert!(only_in_replay.is_empty());
            let mut seen = std::collections::HashSet::new();
            for c in &only_in_claims_table {
                assert!(seen.insert(c.clone()), "a row was paged twice: {c:?}");
            }
            assert_eq!(
                i64::try_from(only_in_claims_table.len()).unwrap(),
                rows,
                "every row past the first page must be read exactly once"
            );
        }
        VerifyOutcome::Consistent { .. } => panic!("rows beneath the audit log must diverge"),
    }
}
