//! Shared test helpers for the morpholog-postgres integration tests.
//!
//! Sync helpers come from `morpholog-test-support`, re-exported below.
//! The async PostgreSQL wrappers live here: in test-support they would
//! create a dependency cycle and pull tokio and sqlx into every user.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{
    ClaimInstance, CompiledProgram, EvalValue, Program, Subject, Transformation, Transition,
};
use morpholog_postgres::{
    PgError, PgPool, PgProgram, PgProposalOutcome, PgTracedOutcome, Proposal, propose_against_pg,
    propose_against_pg_with_trace,
};
use uuid::Uuid;

/// Insert claims straight into the table, as a pre-state for a test.
/// They carry the nil transition id, which marks them as fixture rows.
pub async fn seed_claims(pool: &PgPool, claims: &[ClaimInstance]) {
    for claim in claims {
        sqlx::query(
            "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
             VALUES ($1, $2, $3)",
        )
        .bind(claim.predicate.as_str())
        .bind(serde_json::to_value(&claim.args).unwrap())
        .bind(Uuid::nil())
        .execute(pool)
        .await
        .unwrap();
    }
}

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
/// First waits for other open transactions to end. A previous test's
/// pool closes asynchronously, and a transaction still open lowers the
/// audit watermark, so this test's checkpoints could cover none of its
/// own rows.
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

// Each test file uses a different subset, so unused imports are allowed.
#[allow(unused_imports)]
pub use morpholog_test_support::{
    bool_, claim_instance, coll, date, dec, dec_str, has_claim, intent_instance, role, subj,
    test_actor, test_transition,
};

/// Compile a test programme into the adapter's programme object, on
/// whatever route it is eligible for.
pub fn compiled(program: Program) -> PgProgram {
    PgProgram::new(CompiledProgram::new(program).expect("test programme is valid"))
}

/// Wrap a kernel transition in a gateway-attested proposal - the shape
/// the durable commit paths accept.
pub fn attested(transition: &Transition) -> Proposal {
    Proposal::gateway(transition)
}

/// Propose `transformation` with `args` as `test_actor()`.
pub async fn propose_pg_with_test_actor(
    pool: &PgPool,
    compiled: &PgProgram,
    transformation: &Transformation,
    args: Vec<EvalValue>,
) -> Result<PgProposalOutcome, PgError> {
    let transition = test_transition(transformation, args);
    propose_against_pg(pool, compiled, &attested(&transition)).await
}

/// Admit an `AuditSigningKey(key_id, purpose, public_key)` claim, so a
/// checkpoint signed with that key verifies as authorised.
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

/// Retract an `AuditSigningKey(...)` claim, revoking the key.
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

/// `propose_pg_with_test_actor` plus structured trace.
pub async fn propose_pg_with_trace_using_test_actor(
    pool: &PgPool,
    compiled: &PgProgram,
    transformation: &Transformation,
    args: Vec<EvalValue>,
) -> Result<PgTracedOutcome, PgError> {
    let transition = test_transition(transformation, args);
    propose_against_pg_with_trace(pool, compiled, &attested(&transition)).await
}

/// Propose as an explicit actor.
pub async fn propose_pg_as(
    pool: &PgPool,
    compiled: &PgProgram,
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
/// transition id.
pub async fn commit_entry(pool: &PgPool, id: &str) -> Uuid {
    let compiled = compiled(morpholog_examples::double_entry_ledger::program());
    let t = morpholog_examples::double_entry_ledger::post_simple_entry();
    let outcome = propose_pg_with_test_actor(pool, &compiled, &t, ledger_args(id))
        .await
        .unwrap();
    expect_committed(outcome)
}

/// A balanced ledger posting's arguments, keyed by `id` so each is its own
/// entry and pair of accounts.
pub fn ledger_args(id: &str) -> Vec<EvalValue> {
    vec![
        morpholog_test_support::subj(id),
        morpholog_test_support::subj("d_2026_05_17"),
        morpholog_test_support::subj("p1"),
        morpholog_test_support::subj(&format!("cash_{id}")),
        morpholog_test_support::subj(&format!("rev_{id}")),
        morpholog_test_support::dec(100),
    ]
}

/// Wait (bounded) for other sessions' open transactions to end. A
/// leftover from a prior test lowers the audit watermark, so a checkpoint
/// or tail taken now could cover none of this test's rows.
///
/// On timeout this panics, listing the offending sessions. Carrying on
/// would fail later with a misleading symptom instead.
pub async fn drain_open_transactions(pool: &PgPool) {
    for _ in 0..300 {
        let open: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
             WHERE datname = current_database()
               AND pid != pg_backend_pid()
               AND xact_start IS NOT NULL
               AND backend_type IS DISTINCT FROM 'autovacuum worker'",
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
           AND xact_start IS NOT NULL
           AND backend_type IS DISTINCT FROM 'autovacuum worker'",
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

/// A checkpoint over every audit row committed so far. Rows the
/// watermark withholds are waited for, not silently left out.
pub async fn make_checkpoint(pool: &PgPool) -> morpholog_postgres::Checkpoint {
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM morpholog.audit")
        .fetch_one(pool)
        .await
        .expect("count audit rows");
    assert!(rows > 0, "make_checkpoint needs committed rows to cover");
    make_checkpoint_at(pool, rows).await
}

/// A checkpoint covering exactly `tree_size` rows.
///
/// A transaction still open elsewhere can hold the watermark back, so
/// each attempt drains them first. Only an attempt that created nothing
/// is retried. A short checkpoint that was created is persisted, and a
/// retry would chain onto it, so that fails at once, naming who held
/// the watermark back.
pub async fn make_checkpoint_at(pool: &PgPool, tree_size: i64) -> morpholog_postgres::Checkpoint {
    use morpholog_postgres::CheckpointOutcome::{Created, NoNewRows};
    for attempt in 0..3 {
        drain_open_transactions(pool).await;
        match morpholog_postgres::create_checkpoint(pool, None, None)
            .await
            .unwrap()
        {
            Created(c) | NoNewRows(c) if c.tree_size == tree_size => return c,
            Created(c) => panic!(
                "a checkpoint at tree size {} was persisted where {tree_size} was wanted; \
                 a retry would chain onto it. Open transactions lowering the watermark: {:?}",
                c.tree_size,
                open_transaction_census(pool).await
            ),
            NoNewRows(c) => {
                let census = open_transaction_census(pool).await;
                assert!(
                    attempt < 2,
                    "expected a checkpoint at tree size {tree_size}, still at {}; \
                     open transactions: {census:?}",
                    c.tree_size
                );
                eprintln!(
                    "no checkpoint beyond tree size {} (wanted {tree_size}); retrying. \
                     Open transactions lowering the watermark: {census:?}",
                    c.tree_size
                );
            }
        }
    }
    unreachable!("the loop returns or panics")
}

async fn open_transaction_census(pool: &PgPool) -> Vec<(i32, String, String)> {
    sqlx::query_as(
        "SELECT pid, coalesce(state, '?'), left(coalesce(query, ''), 120)
         FROM pg_stat_activity
         WHERE datname = current_database()
           AND pid != pg_backend_pid()
           AND xact_start IS NOT NULL
           AND backend_type IS DISTINCT FROM 'autovacuum worker'",
    )
    .fetch_all(pool)
    .await
    .expect("census open transactions")
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

/// Hand-write one attested audit row inside an open transaction, to act
/// as an in-flight writer. `committed_at` defaults to the transaction's
/// start.
pub async fn insert_in_flight_audit_row(conn: &mut sqlx::PgConnection, transition_id: Uuid) {
    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents,
            attestation, parameters
         ) VALUES ($1, 'post', '[]'::jsonb,
                   '{\"type\":\"subject\",\"value\":\"in_flight\"}'::jsonb,
                   1, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb,
                   '{\"mode\":\"gateway\",\"authenticated_by\":\"test\"}'::jsonb,
                   '[]'::jsonb)",
    )
    .bind(transition_id)
    .execute(conn)
    .await
    .unwrap();
}

/// Whether the connecting role is a superuser. Tests that need to act
/// as another role need this.
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
/// Roles are cluster-wide and outlive `reset_db` and the test binary. A
/// leftover role that can write `morpholog.audit` breaks the writer-role
/// census in other suites, so each test uses its own roles and drops them.
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
        // DDL takes no bind parameters, so the name is checked here.
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
