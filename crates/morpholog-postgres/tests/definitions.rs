//! Scoped loading through definitions, against real PostgreSQL: a gate or
//! invariant that reaches predicates only through a definition must still
//! have them loaded. If the read path stopped at the call, the gate would
//! silently evaluate against claims never loaded.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{reset_db, test_pool};

use common::subj;
use morpholog_core::Program;
use morpholog_postgres::{PgPool, PgProposalOutcome};
use morpholog_surface::parse_program;

// `ship` names only `Shipped` directly; `Box` and `Sealed` are read only
// through `sealed_box`, from the gate and from the invariant. The
// invariant's call sits two definitions deep, to test more than one hop.
const SHIPPING: &str = r#"
program shipping

predicate Box(item: Subject)
predicate Sealed(item: Subject)
predicate Shipped(item: Subject)

define sealed_box(item):
    Box(item)
    and Sealed(item)

define shippable(item):
    sealed_box(item)

invariant shipped_means_shippable:
    Shipped(item) implies shippable(item)

transformation register(item):
    admit Box(item)

transformation seal(item):
    admit Sealed(item)

transformation ship(item):
    require shippable(item)
    admit Shipped(item)
"#;

fn shipping_program() -> Program {
    let p = parse_program(SHIPPING).expect("shipping programme parses");
    p.validate().expect("shipping programme validates");
    p
}

async fn run(pool: &PgPool, p: &Program, transformation: &str, item: &str) -> PgProposalOutcome {
    let t = p.transformation(transformation).unwrap();
    common::propose_pg_with_test_actor(pool, &common::compiled(p.clone()), t, vec![subj(item)])
        .await
        .expect("propose_against_pg should not error")
}

#[tokio::test]
async fn a_gate_behind_two_definition_levels_sees_its_claims() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = shipping_program();

    assert!(matches!(
        run(&pool, &p, "register", "crate_1").await,
        PgProposalOutcome::Committed { .. }
    ));
    assert!(matches!(
        run(&pool, &p, "seal", "crate_1").await,
        PgProposalOutcome::Committed { .. }
    ));
    // This commits only if the read path loaded Box and Sealed through
    // `shippable` -> `sealed_box`.
    assert!(matches!(
        run(&pool, &p, "ship", "crate_1").await,
        PgProposalOutcome::Committed { .. }
    ));
}

#[tokio::test]
async fn the_same_gate_refuses_honestly_when_the_condition_is_unmet() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    let p = shipping_program();

    assert!(matches!(
        run(&pool, &p, "register", "crate_2").await,
        PgProposalOutcome::Committed { .. }
    ));
    // Registered but never sealed: the gate finds Box but not Sealed and
    // rejects for the right reason.
    assert!(matches!(
        run(&pool, &p, "ship", "crate_2").await,
        PgProposalOutcome::Rejected { .. }
    ));
}
