//! Shared test helpers for the morpholog-postgres integration tests.
//!
//! Sync helpers (constructors, default actor, in-memory propose
//! wrappers) come from `morpholog-test-support` via the re-export
//! below. This file owns the **async** PG-specific wrappers
//! (`propose_pg_*`) because they depend on `morpholog-postgres`
//! itself - putting them in test-support would create a dep cycle
//! and would also force tokio/sqlx into every consumer of the
//! support crate.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{CompiledProgram, EvalValue, Program, Subject, Transformation, Transition};
use morpholog_postgres::{
    PgError, PgPool, PgProposalOutcome, PgTracedOutcome, Proposal, propose_against_pg,
    propose_against_pg_with_trace,
};
use uuid::Uuid;

/// Connect to the integration-test database named by `DATABASE_URL`.
/// These suites share one schema and TRUNCATE it on entry, so point
/// `DATABASE_URL` at a disposable database (`postgres:///morpholog_dev`).
pub async fn test_pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must be set for morpholog-postgres integration tests \
         (e.g. postgres:///morpholog_dev)",
    );
    let url = morpholog_postgres::with_default_user(&url);
    PgPool::connect(&url)
        .await
        .expect("failed to connect to PostgreSQL test database")
}

/// Truncate the governed `morpholog.*` tables - the default reset every
/// integration test runs on entry.
///
/// First waits for other open transactions on the database to drain: a
/// previous test's pool closes its connections asynchronously, and a
/// straggler still inside a transaction lowers the audit watermark, so
/// a checkpoint taken by THIS test can otherwise cover none of its own
/// rows (withhold-never-lose working as designed, against a transaction
/// the test cannot see). Bounded, then proceeds - a wait this long means
/// something is genuinely stuck and the test should fail visibly.
pub async fn reset_db(pool: &PgPool) {
    drain_open_transactions(pool).await;
    sqlx::query(morpholog_postgres::testing::RESET_SQL)
        .execute(pool)
        .await
        .expect("failed to truncate test DB");
}

/// `reset_db` plus the `morpholog_read.*` derived cache. **Only** for the
/// tests that exercise the derived read cache or SQL views; do not make
/// this the default reset - everything else uses [`reset_db`].
pub async fn reset_db_and_read_cache(pool: &PgPool) {
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "{}; TRUNCATE morpholog_read.derived_claims, morpholog_read.derived_active, \
                  morpholog_read.derived_refreshes CASCADE;",
        morpholog_postgres::testing::RESET_SQL
    )))
    .execute(pool)
    .await
    .expect("reset");
}

/// Unwrap a committed outcome's transition id, panicking on rejection -
/// the common shape for tests that set up state they expect to commit.
pub fn expect_committed(outcome: PgProposalOutcome) -> Uuid {
    match outcome {
        PgProposalOutcome::Committed { transition_id, .. } => transition_id,
        PgProposalOutcome::Rejected { reason, .. } => {
            panic!("expected Committed; got Rejected({reason})")
        }
    }
}

// Re-export the test-support surface so per-test files can `use
// common::{subj, dec, ...};` rather than depending on
// morpholog-test-support directly. The `allow(unused_imports)` is
// necessary because each per-test file pulls a different subset:
// without it, every binary that doesn't use the full set generates
// noise pointing at the re-export rather than the file that's
// actually missing the import.
#[allow(unused_imports)]
pub use morpholog_test_support::{
    bool_, claim_instance, coll, date, dec, dec_str, has_claim, intent_instance, role, subj,
    test_actor, test_transition,
};

/// Compile a test programme (validates + indexes). The facade-based
/// propose path now takes a `&CompiledProgram`; tests build one from an
/// example's `program()` and reuse it across that test's proposals.
pub fn compiled(program: Program) -> CompiledProgram {
    CompiledProgram::new(program).expect("test programme is valid")
}

/// Wrap a kernel transition in a gateway-attested proposal - the shape
/// the durable commit paths accept.
pub fn attested(transition: &Transition) -> Proposal {
    Proposal::gateway(transition)
}

/// Convenience for tests: build the `Transition` with `test_actor()` and
/// propose through the `CompiledProgram` facade. `transformation` names
/// the transition; the programme's rule slices come from `compiled`.
pub async fn propose_pg_with_test_actor(
    pool: &PgPool,
    compiled: &CompiledProgram,
    transformation: &Transformation,
    args: Vec<EvalValue>,
) -> Result<PgProposalOutcome, PgError> {
    let transition = test_transition(transformation, args);
    propose_against_pg(pool, compiled, &attested(&transition)).await
}

/// Admit an `AuditSigningKey(key_id, purpose, public_key)` claim through a
/// minimal key-governance programme - the keys-as-claims authorisation a
/// signed checkpoint is verified against. Lets the signing tests assert
/// `Intact` on a checkpoint whose key the ledger actually authorised.
pub async fn authorize_signing_key(pool: &PgPool, key_id: &str, purpose: &str, public_key: &str) {
    use morpholog_core::ir_builder::{assert_, params, predicate, program, transformation, var};
    let t = transformation(
        "authorize_signing_key",
        params(&["key_id", "purpose", "public_key"]),
        vec![assert_(
            "AuditSigningKey",
            vec![var("key_id"), var("purpose"), var("public_key")],
        )],
    );
    let prog = program("key_governance")
        .predicates(vec![
            predicate("AuditSigningKey")
                .subject("key_id")
                .subject("purpose")
                .subject("public_key")
                .build(),
        ])
        .transformations(vec![t.clone()])
        .build();
    let outcome = propose_pg_with_test_actor(
        pool,
        &compiled(prog),
        &t,
        vec![subj(key_id), subj(purpose), subj(public_key)],
    )
    .await
    .unwrap();
    expect_committed(outcome);
}

/// Retract an `AuditSigningKey(...)` claim - the revocation half of the
/// keys-as-claims lifecycle, for tests that pin authority as-of a prefix.
pub async fn retract_signing_key(pool: &PgPool, key_id: &str, purpose: &str, public_key: &str) {
    use morpholog_core::ir_builder::{params, predicate, program, retract, transformation, var};
    let t = transformation(
        "retract_signing_key",
        params(&["key_id", "purpose", "public_key"]),
        vec![retract(
            "AuditSigningKey",
            vec![var("key_id"), var("purpose"), var("public_key")],
        )],
    );
    let prog = program("key_governance")
        .predicates(vec![
            predicate("AuditSigningKey")
                .subject("key_id")
                .subject("purpose")
                .subject("public_key")
                .build(),
        ])
        .transformations(vec![t.clone()])
        .build();
    let outcome = propose_pg_with_test_actor(
        pool,
        &compiled(prog),
        &t,
        vec![subj(key_id), subj(purpose), subj(public_key)],
    )
    .await
    .unwrap();
    expect_committed(outcome);
}

/// `propose_pg_with_test_actor` plus structured trace. Uses the shared
/// `test_actor()` for tests that don't model authority.
pub async fn propose_pg_with_trace_using_test_actor(
    pool: &PgPool,
    compiled: &CompiledProgram,
    transformation: &Transformation,
    args: Vec<EvalValue>,
) -> Result<PgTracedOutcome, PgError> {
    let transition = test_transition(transformation, args);
    propose_against_pg_with_trace(pool, compiled, &attested(&transition)).await
}

/// Variant that lets the caller supply an explicit actor. Used by
/// authority tests that need to assert on which actor proposed which
/// transition.
pub async fn propose_pg_as(
    pool: &PgPool,
    compiled: &CompiledProgram,
    transformation: &Transformation,
    args: Vec<EvalValue>,
    actor: impl Into<Subject>,
) -> Result<PgProposalOutcome, PgError> {
    let transition = Transition {
        transformation_name: transformation.name.clone(),
        args,
        actor: actor.into(),
    };
    propose_against_pg(pool, compiled, &attested(&transition)).await
}

/// Commit one balanced double-entry-ledger posting and return its
/// transition id - the fixture opener the tamper-evidence and
/// evaluate suites share.
pub async fn commit_entry(pool: &PgPool, id: &str) -> Uuid {
    let compiled = compiled(morpholog_examples::double_entry_ledger::program());
    let t = morpholog_examples::double_entry_ledger::post_simple_entry();
    let outcome = propose_pg_with_test_actor(
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
    .unwrap();
    expect_committed(outcome)
}

/// Wait (bounded) for other sessions' open transactions to end: a
/// straggler from a prior test's pool lowers the audit watermark, so
/// a checkpoint or tail taken now can otherwise cover none of this
/// test's own rows - withhold-never-lose working as designed, against
/// a transaction the test cannot see.
///
/// On expiry this PANICS with a census of the offending sessions.
/// Proceeding silently instead is how the watermark flake reached CI
/// as "signing key is not authorised ... as of tree_size 0" - a
/// misleading downstream symptom; the straggler's pid and query are
/// the actual diagnosis.
pub async fn drain_open_transactions(pool: &PgPool) {
    for _ in 0..300 {
        let open: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname = current_database()
               AND pid != pg_backend_pid()
               AND xact_start IS NOT NULL",
        )
        .fetch_one(pool)
        .await
        .expect("failed to count open transactions");
        if open == 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let census: Vec<(i32, String, String)> = sqlx::query_as(
        "SELECT pid, coalesce(state, '?'),
                left(coalesce(query, ''), 120) || ' [open ' ||
                round(extract(epoch FROM now() - xact_start))::text || 's]'
         FROM pg_stat_activity
         WHERE datname = current_database()
           AND pid != pg_backend_pid()
           AND xact_start IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    .expect("failed to census open transactions");
    panic!(
        "foreign open transaction(s) on the test database did not drain; \
         they lower the audit watermark and any checkpoint/tail this test \
         takes will see an empty tree. Offenders: {census:?}"
    );
}

/// Unwrap a created checkpoint; a no-new-rows outcome is a fixture
/// bug, not a scenario. One drain-and-retry absorbs the mid-test
/// straggler (see [`drain_open_transactions`]) that otherwise makes
/// this the suite's known flake.
pub async fn make_checkpoint(pool: &PgPool) -> morpholog_postgres::Checkpoint {
    for attempt in 0..2 {
        match morpholog_postgres::create_checkpoint(pool, None, None)
            .await
            .unwrap()
        {
            morpholog_postgres::CheckpointOutcome::Created(c) => return c,
            other @ morpholog_postgres::CheckpointOutcome::NoNewRows(_) => {
                if attempt == 1 {
                    panic!("expected a created checkpoint, got {other:?}")
                }
                drain_open_transactions(pool).await;
            }
        }
    }
    unreachable!("the loop returns or panics")
}

/// Round-trip a serialisable value through a JSON edit - the tamper
/// harness every pack suite uses.
pub fn edit_json<T: serde::Serialize + serde::de::DeserializeOwned>(
    value: &T,
    edit: impl FnOnce(&mut serde_json::Value),
) -> T {
    let mut v = serde_json::to_value(value).unwrap();
    edit(&mut v);
    serde_json::from_value(v).unwrap()
}

/// The connecting role's name.
pub async fn session_user(pool: &PgPool) -> String {
    let (name,): (String,) = sqlx::query_as("SELECT session_user::text")
        .fetch_one(pool)
        .await
        .unwrap();
    name
}

/// Hand-write one attested audit row inside an open transaction - the
/// in-flight writer the watermark race tests need. committed_at takes
/// the schema default: the writer's transaction start.
pub async fn insert_in_flight_audit_row(conn: &mut sqlx::PgConnection, transition_id: Uuid) {
    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents,
            attestation
         ) VALUES ($1, 'post', '[]'::jsonb,
                   '{\"type\":\"subject\",\"value\":\"in_flight\"}'::jsonb,
                   1, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb,
                   '{\"mode\":\"gateway\",\"authenticated_by\":\"test\"}'::jsonb)",
    )
    .bind(transition_id)
    .execute(conn)
    .await
    .unwrap();
}

/// Whether the connecting role is a superuser. Two suites gate on it:
/// the tests that need a role identity of their own can only get one
/// by assuming it, and only a superuser may.
pub async fn session_is_superuser(pool: &PgPool) -> bool {
    let (rolsuper,): (bool,) =
        sqlx::query_as("SELECT rolsuper FROM pg_roles WHERE rolname = session_user")
            .fetch_one(pool)
            .await
            .unwrap();
    rolsuper
}

/// Drop, then recreate, the named roles with the caller's own setup
/// statements.
///
/// Roles are cluster-global: they outlive `reset_db`, outlive the test
/// binary, and are visible to every other suite. One left behind that
/// can write `morpholog.audit` joins the writer-role census and fails
/// an assertion somewhere else entirely, so every test names roles of
/// its own and drops them on the way out.
pub async fn recreate_roles(pool: &PgPool, roles: &[&str], setup: &[&str]) {
    drop_roles_if_present(pool, roles).await;
    for statement in setup {
        // Audited: `setup` is a literal slice each caller writes inline.
        sqlx::raw_sql(sqlx::AssertSqlSafe(statement.to_string()))
            .execute(pool)
            .await
            .unwrap();
    }
}

/// Drop roles that exist, leaving absent ones alone - the entry half
/// of [`recreate_roles`], and the safe form for a cleanup path that
/// may run after a test failed partway.
pub async fn drop_roles_if_present(pool: &PgPool, roles: &[&str]) {
    for role in roles {
        // A role name reaches DDL, which takes no bind parameters, so
        // it is checked here rather than trusted and quoted when it
        // gets there. Callers pass literals today; the first one to
        // build a name from data should meet this assertion rather
        // than a syntax error, or worse.
        assert!(
            !role.is_empty()
                && role
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "test role names are plain identifiers: got `{role}`"
        );
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
                .bind(role)
                .fetch_one(pool)
                .await
                .unwrap();
        if exists {
            // Audited: `role` is asserted above to be a plain
            // identifier, and is quoted here regardless.
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "DROP OWNED BY \"{role}\"; DROP ROLE \"{role}\""
            )))
            .execute(pool)
            .await
            .unwrap();
        }
    }
}
