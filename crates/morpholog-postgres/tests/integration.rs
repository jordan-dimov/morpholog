//! Integration tests for `propose_against_pg`.
//!
//! These tests require a running PostgreSQL 17 server with the
//! `morpholog` schema applied (from `crates/morpholog-core/sql/schema.sql`).
//! The connection string is read from the `DATABASE_URL` environment
//! variable; tests panic if it is not set.
//!
//! Each test calls `reset_db` to TRUNCATE all three tables before
//! running its scenario. Run with `cargo test -- --test-threads=1` so
//! tests do not race on the shared schema.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{
    Claim, ClaimInstance, EvalValue, Expr, Intent, Invariant, Stmt, Term, Transformation,
};
use morpholog_postgres::{PgProposalOutcome, propose_against_pg};
use rust_decimal::Decimal;
use sqlx::PgPool;
use uuid::Uuid;

// ============================================================
// Test infrastructure
// ============================================================

async fn test_pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must be set for morpholog-postgres integration tests \
         (e.g. postgres:///morpholog_dev or postgres://postgres:postgres@localhost:5432/postgres)",
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

async fn insert_pre_state(pool: &PgPool, claims: Vec<ClaimInstance>) {
    // Pre-state claims need a non-null `asserted_in` UUID. We use a
    // fixed synthetic UUID so the rows are easy to identify if a test
    // inspects them; it carries no semantic meaning.
    let fixture_transition = Uuid::nil();
    for claim in claims {
        let args_json = serde_json::to_value(&claim.args).unwrap();
        sqlx::query(
            "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
             VALUES ($1, $2, $3)",
        )
        .bind(&claim.predicate)
        .bind(&args_json)
        .bind(fixture_transition)
        .execute(pool)
        .await
        .unwrap();
    }
}

fn subj(s: &str) -> EvalValue {
    EvalValue::Subject(s.to_string())
}

fn dec(n: i64) -> EvalValue {
    EvalValue::Decimal(Decimal::new(n, 0))
}

fn claim(predicate: &str, args: Vec<EvalValue>) -> ClaimInstance {
    ClaimInstance {
        predicate: predicate.to_string(),
        args,
    }
}

// ============================================================
// Settlement-netting IR
//
// TODO: extract these to a `pub mod examples` in morpholog-core so
// morpholog-core's own tests and these integration tests can share
// the constructors. Duplicated here for now to keep the PG-adapter
// PR focused on persistence.
// ============================================================

fn net_settlement_has_lines() -> Invariant {
    let var = |s: &str| Term::Var(s.to_string());
    Invariant {
        name: "net_settlement_has_lines".to_string(),
        version: 1,
        body: Expr::Implies {
            left: Box::new(Expr::Claim {
                predicate: "NetSettlement".to_string(),
                args: vec![var("net"), Term::Wildcard, Term::Wildcard, Term::Wildcard],
            }),
            right: Box::new(Expr::Exists {
                binding: "line".to_string(),
                body: Box::new(Expr::Claim {
                    predicate: "SettlementLine".to_string(),
                    args: vec![var("line"), var("net"), Term::Wildcard],
                }),
            }),
        },
    }
}

fn net_amount_equals_lines() -> Invariant {
    let var = |s: &str| Term::Var(s.to_string());
    Invariant {
        name: "net_amount_equals_lines".to_string(),
        version: 1,
        body: Expr::Implies {
            left: Box::new(Expr::Claim {
                predicate: "NetSettlement".to_string(),
                args: vec![var("net"), Term::Wildcard, Term::Wildcard, var("amount")],
            }),
            right: Box::new(Expr::Eq(
                Box::new(Expr::Term(var("amount"))),
                Box::new(Expr::Sum {
                    value: var("x"),
                    binding: "x".to_string(),
                    body: Box::new(Expr::Claim {
                        predicate: "SettlementLine".to_string(),
                        args: vec![Term::Wildcard, var("net"), var("x")],
                    }),
                }),
            )),
        },
    }
}

fn no_double_netting() -> Invariant {
    let var = |s: &str| Term::Var(s.to_string());
    Invariant {
        name: "no_double_netting".to_string(),
        version: 1,
        body: Expr::Implies {
            left: Box::new(Expr::Claim {
                predicate: "SettlementLine".to_string(),
                args: vec![var("line"), var("net"), Term::Wildcard],
            }),
            right: Box::new(Expr::Not(Box::new(Expr::Exists {
                binding: "other".to_string(),
                body: Box::new(Expr::And(vec![
                    Expr::Claim {
                        predicate: "SettlementLine".to_string(),
                        args: vec![var("line"), var("other"), Term::Wildcard],
                    },
                    Expr::Neq(var("other"), var("net")),
                ])),
            }))),
        },
    }
}

fn create_net_settlement() -> Transformation {
    let var = |s: &str| Term::Var(s.to_string());
    Transformation {
        name: "create_net_settlement".to_string(),
        parameters: vec![
            "party_a".to_string(),
            "party_b".to_string(),
            "lines".to_string(),
        ],
        body: vec![
            Stmt::Require(Expr::Forall {
                binding: "line".to_string(),
                source: Box::new(Expr::In(var("line"), var("lines"))),
                body: Box::new(Expr::And(vec![
                    Expr::Claim {
                        predicate: "ApprovedSettlementLine".to_string(),
                        args: vec![var("line")],
                    },
                    Expr::Claim {
                        predicate: "Between".to_string(),
                        args: vec![var("line"), var("party_a"), var("party_b")],
                    },
                    Expr::Not(Box::new(Expr::Claim {
                        predicate: "Netted".to_string(),
                        args: vec![var("line")],
                    })),
                ])),
            }),
            Stmt::LetNewSubject {
                name: "net".to_string(),
            },
            Stmt::Let {
                name: "amount".to_string(),
                value: Expr::Sum {
                    value: var("x"),
                    binding: "x".to_string(),
                    body: Box::new(Expr::And(vec![
                        Expr::In(var("line"), var("lines")),
                        Expr::Claim {
                            predicate: "LineAmount".to_string(),
                            args: vec![var("line"), var("x")],
                        },
                    ])),
                },
            },
            Stmt::Assert(Claim {
                predicate: "NetSettlement".to_string(),
                args: vec![var("net"), var("party_a"), var("party_b"), var("amount")],
            }),
            Stmt::For {
                binding: "line".to_string(),
                collection: Expr::Term(var("lines")),
                body: vec![
                    Stmt::Let {
                        name: "amt".to_string(),
                        value: Expr::ValueOf {
                            predicate: "LineAmount".to_string(),
                            args: vec![var("line"), Term::Wildcard],
                            default: None,
                        },
                    },
                    Stmt::Assert(Claim {
                        predicate: "SettlementLine".to_string(),
                        args: vec![var("line"), var("net"), var("amt")],
                    }),
                    Stmt::Assert(Claim {
                        predicate: "Netted".to_string(),
                        args: vec![var("line")],
                    }),
                ],
            },
            Stmt::Emit(Intent {
                name: "NetSettlementCreated".to_string(),
                args: vec![var("net")],
            }),
        ],
    }
}

fn all_netting_invariants() -> Vec<Invariant> {
    vec![
        net_settlement_has_lines(),
        net_amount_equals_lines(),
        no_double_netting(),
    ]
}

fn netting_pre_state_claims() -> Vec<ClaimInstance> {
    vec![
        claim("ApprovedSettlementLine", vec![subj("l1")]),
        claim(
            "Between",
            vec![subj("l1"), subj("party_a"), subj("party_b")],
        ),
        claim("LineAmount", vec![subj("l1"), dec(60)]),
        claim("ApprovedSettlementLine", vec![subj("l2")]),
        claim(
            "Between",
            vec![subj("l2"), subj("party_a"), subj("party_b")],
        ),
        claim("LineAmount", vec![subj("l2"), dec(40)]),
    ]
}

fn netting_args() -> Vec<EvalValue> {
    vec![
        subj("party_a"),
        subj("party_b"),
        EvalValue::Collection(vec![subj("l1"), subj("l2")]),
    ]
}

// ============================================================
// Tests
// ============================================================

#[tokio::test]
async fn settlement_netting_happy_path_commits_claims_audit_and_outbox() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    insert_pre_state(&pool, netting_pre_state_claims()).await;

    let outcome = propose_against_pg(
        &pool,
        &create_net_settlement(),
        netting_args(),
        &all_netting_invariants(),
    )
    .await
    .expect("propose_against_pg should not error");

    let PgProposalOutcome::Committed {
        transition_id,
        asserted_claims,
        retracted_claims,
        emitted_intents,
    } = outcome
    else {
        panic!("expected Committed, got {outcome:?}");
    };

    // Outcome shape: 1 NetSettlement + (1 SettlementLine + 1 Netted) per line = 5 asserts.
    assert_eq!(asserted_claims.len(), 5);
    assert_eq!(retracted_claims.len(), 0);
    assert_eq!(emitted_intents.len(), 1);
    assert_eq!(emitted_intents[0].name, "NetSettlementCreated");

    // DB state.
    let claim_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM morpholog.claims")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(claim_count, 11, "6 pre-state + 5 newly asserted");

    let audit_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM morpholog.audit")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(audit_count, 1);

    let audit_transition: Uuid =
        sqlx::query_scalar("SELECT transition_id FROM morpholog.audit LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(audit_transition, transition_id);

    let audit_epoch: i32 =
        sqlx::query_scalar("SELECT invariant_epoch FROM morpholog.audit LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(audit_epoch, 1);

    let outbox_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM morpholog.outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(outbox_count, 1);

    let outbox_intent_type: String =
        sqlx::query_scalar("SELECT intent_type FROM morpholog.outbox LIMIT 1")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(outbox_intent_type, "NetSettlementCreated");
}
