//! The scoped-loading red line for defined propositions, pinned
//! against real PostgreSQL: a gate (and an invariant) whose predicates
//! are reachable ONLY through a definition's body must still have those
//! predicates loaded into the kernel's pre-state. If any walker on the
//! read path stopped at the call instead of descending, the gate would
//! evaluate against claims that were never loaded - a silent
//! wrong-answer, not a test failure anywhere else.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;
use common::{reset_db, test_pool};

use common::subj;
use morpholog_core::Program;
use morpholog_postgres::{PgPool, PgProposalOutcome};
use morpholog_surface::parse_program;

// `ship`'s own body references only `Shipped` directly; `Box` and
// `Sealed` are consulted exclusively through `sealed_box` - one call
// in the gate, one in the invariant, and the invariant's call sits
// behind a second definition level to pin transitivity, not just one
// hop.
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
    common::propose_pg_with_test_actor(pool, t, vec![subj(item)], &p.invariants, &p.definitions)
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
    // The commit is the proof: `ship` consults Box and Sealed only
    // through `shippable` -> `sealed_box`, so this commits only if the
    // read path loaded both predicates through two call levels.
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
    // Registered but never sealed: the gate's call finds Box but not
    // Sealed, and the proposal is a lawful rejection - the loaded
    // state was complete enough to refuse for the right reason.
    assert!(matches!(
        run(&pool, &p, "ship", "crate_2").await,
        PgProposalOutcome::Rejected { .. }
    ));
}
