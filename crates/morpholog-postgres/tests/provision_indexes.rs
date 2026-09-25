//! `provision indexes`: the reconciliation states, each provoked.
//!
//! Attacker capability where one is modelled: an operator with DDL on
//! the claims table, who may have created indexes of their own, in
//! Morpholog's namespace or outside it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{compiled, reset_db, session_is_superuser, test_pool};
use morpholog_core::ir_builder::program;
use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{IndexAction, PgPool, PgProgram, plan_indexes, provision_indexes};
use sqlx::Row as _;

/// The ledger's requirement: its compiled checks' seeks, the case column
/// of its lineage check, and the two positions its transformations' gates
/// key on.
const LEDGER_INDEXES: usize = 6;

fn ledger() -> PgProgram {
    compiled(double_entry_ledger::program())
}

async fn catalogue_names(pool: &PgPool) -> Vec<(String, bool)> {
    sqlx::query(
        "SELECT c.relname, i.indisvalid FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
         WHERE i.indrelid = 'morpholog.claims'::regclass AND c.relname LIKE 'morpholog_ci_%'
         ORDER BY c.relname",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|r| (r.get(0), r.get(1)))
    .collect()
}

async fn drop_our_indexes(pool: &PgPool) {
    for (name, _) in catalogue_names(pool).await {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP INDEX IF EXISTS morpholog.\"{name}\""
        )))
        .execute(pool)
        .await
        .unwrap();
    }
    sqlx::query("DROP INDEX IF EXISTS morpholog.operator_made_this")
        .execute(pool)
        .await
        .unwrap();
}

async fn registry_counts(pool: &PgPool) -> (i64, i64) {
    let managed: i64 = sqlx::query_scalar("SELECT count(*) FROM morpholog.managed_index")
        .fetch_one(pool)
        .await
        .unwrap();
    let required: i64 = sqlx::query_scalar("SELECT count(*) FROM morpholog.index_requirement")
        .fetch_one(pool)
        .await
        .unwrap();
    (managed, required)
}

fn actions(report: &morpholog_postgres::ProvisionReport) -> Vec<IndexAction> {
    report.entries.iter().map(|e| e.action).collect()
}

#[tokio::test]
async fn a_dry_run_names_every_index_to_create_and_creates_none() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    let report = plan_indexes(&pool, &ledger()).await.unwrap();
    assert_eq!(
        actions(&report),
        vec![IndexAction::Create; LEDGER_INDEXES],
        "{report:?}"
    );
    assert!(!report.applied);
    assert!(catalogue_names(&pool).await.is_empty());
    assert_eq!(registry_counts(&pool).await, (0, 0));
}

#[tokio::test]
async fn provisioning_creates_registers_and_then_keeps() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    let first = provision_indexes(&pool, &ledger(), false).await.unwrap();
    assert_eq!(
        actions(&first),
        vec![IndexAction::Create; LEDGER_INDEXES],
        "{first:?}"
    );
    let built = catalogue_names(&pool).await;
    assert_eq!(built.len(), LEDGER_INDEXES, "{built:?}");
    assert!(built.iter().all(|(_, valid)| *valid));
    assert_eq!(
        registry_counts(&pool).await,
        (LEDGER_INDEXES as i64, LEDGER_INDEXES as i64)
    );

    let again = provision_indexes(&pool, &ledger(), false).await.unwrap();
    assert_eq!(
        actions(&again),
        vec![IndexAction::Keep; LEDGER_INDEXES],
        "{again:?}"
    );
    assert_eq!(
        registry_counts(&pool).await,
        (LEDGER_INDEXES as i64, LEDGER_INDEXES as i64)
    );
}

/// A crash between the build and the registry write leaves a correct
/// index unrecorded; the next run adopts it rather than rebuilding.
#[tokio::test]
async fn an_unrecorded_matching_index_is_adopted() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    provision_indexes(&pool, &ledger(), false).await.unwrap();
    sqlx::raw_sql("DELETE FROM morpholog.index_requirement; DELETE FROM morpholog.managed_index")
        .execute(&pool)
        .await
        .unwrap();
    let report = provision_indexes(&pool, &ledger(), false).await.unwrap();
    assert_eq!(
        actions(&report),
        vec![IndexAction::Keep; LEDGER_INDEXES],
        "{report:?}"
    );
    assert_eq!(
        registry_counts(&pool).await,
        (LEDGER_INDEXES as i64, LEDGER_INDEXES as i64)
    );
}

/// The first specification the ledger requires, as the public report
/// states it: name, expression, partial predicate.
async fn first_spec(pool: &PgPool) -> (String, String, String) {
    let entry = plan_indexes(pool, &ledger())
        .await
        .unwrap()
        .entries
        .remove(0);
    (
        entry.index_name,
        entry.expression_sql,
        entry.partial_predicate_sql,
    )
}

/// An operator's equivalent index under another name satisfies the
/// requirement, is never adopted, and is never pruned.
#[tokio::test]
async fn an_equivalent_index_under_another_name_satisfies_and_is_never_pruned() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    let (_, expression, partial) = first_spec(&pool).await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE INDEX operator_made_this ON morpholog.claims USING btree (({expression})) WHERE {partial}"
    )))
    .execute(&pool)
    .await
    .unwrap();

    let report = provision_indexes(&pool, &ledger(), true).await.unwrap();
    assert_eq!(
        actions(&report),
        std::iter::once(IndexAction::SatisfiedExternally)
            .chain(std::iter::repeat_n(IndexAction::Create, LEDGER_INDEXES - 1))
            .collect::<Vec<_>>(),
        "{report:?}"
    );
    assert!(report.entries[0].detail.contains("operator_made_this"));
    assert_eq!(
        catalogue_names(&pool).await.len(),
        LEDGER_INDEXES - 1,
        "ours are only the ones it created"
    );
    assert_eq!(
        registry_counts(&pool).await,
        (LEDGER_INDEXES as i64 - 1, LEDGER_INDEXES as i64),
        "one fewer managed than required: a requirement outlives the index that serves it"
    );
    let still_there: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pg_class WHERE relname = 'operator_made_this')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        still_there,
        "an external index is never Morpholog's to prune"
    );
}

/// Morpholog's own name over a different definition is a conflict. It is
/// reported and the run applies nothing: a partial run would drop this
/// programme's requirement, and a later prune could take the operator's
/// index for stale.
#[tokio::test]
async fn a_conflicting_definition_under_our_name_applies_nothing() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    let (name, _, partial) = first_spec(&pool).await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE INDEX \"{name}\" ON morpholog.claims USING btree ((arguments -> 7 ->> 'value')) WHERE {partial}"
    )))
    .execute(&pool)
    .await
    .unwrap();

    let report = provision_indexes(&pool, &ledger(), true).await.unwrap();
    assert_eq!(
        report.entries[0].action,
        IndexAction::Conflict,
        "{report:?}"
    );
    assert!(report.has_conflict());
    assert!(!report.applied, "a conflict fails closed");
    assert!(
        report.entries[0].detail.contains("arguments -> 7"),
        "{}",
        report.entries[0].detail
    );
    let definition: String =
        sqlx::query_scalar("SELECT pg_get_indexdef(c.oid) FROM pg_class c WHERE c.relname = $1")
            .bind(&name)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        definition.contains("-> 7"),
        "left exactly as found: {definition}"
    );
    assert_eq!(
        catalogue_names(&pool).await.len(),
        1,
        "no other index was built"
    );
    assert_eq!(registry_counts(&pool).await, (0, 0), "no registry write");
}

/// A requirement met by an operator's index still counts. If that index
/// goes and another programme has Morpholog build the same one, the first
/// programme's requirement protects it from the second's prune.
#[tokio::test]
async fn a_requirement_once_satisfied_externally_still_protects_the_index() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    let (_, expression, partial) = first_spec(&pool).await;
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE INDEX operator_made_this ON morpholog.claims USING btree (({expression})) WHERE {partial}"
    )))
    .execute(&pool)
    .await
    .unwrap();
    // A: the ledger, its first specification satisfied by the operator.
    provision_indexes(&pool, &ledger(), false).await.unwrap();
    // The operator removes their index; B, another book, has Morpholog build it.
    sqlx::query("DROP INDEX morpholog.operator_made_this")
        .execute(&pool)
        .await
        .unwrap();
    let another = |name: &str| {
        let mut p = double_entry_ledger::program();
        p.name = name.into();
        compiled(p)
    };
    let built = provision_indexes(&pool, &another("another_book"), false)
        .await
        .unwrap();
    assert_eq!(built.entries[0].action, IndexAction::Create, "{built:?}");
    // B stops needing anything and prunes: A still requires all of them.
    let nobody = compiled(program("another_book").build());
    let pruned = provision_indexes(&pool, &nobody, true).await.unwrap();
    assert!(pruned.pruned.is_empty(), "{pruned:?}");
    assert_eq!(
        catalogue_names(&pool).await.len(),
        LEDGER_INDEXES,
        "A's requirements protect every index"
    );
}

/// A programme that stops requiring an index leaves it stale: reported,
/// dropped only under prune, and only when no programme requires it.
#[tokio::test]
async fn stale_indexes_are_reported_and_pruned_only_on_request() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    provision_indexes(&pool, &ledger(), false).await.unwrap();
    // The same identity, now needing nothing.
    let successor = compiled(program("double_entry_ledger").build());
    let reported = provision_indexes(&pool, &successor, false).await.unwrap();
    assert_eq!(
        actions(&reported),
        vec![IndexAction::Stale; LEDGER_INDEXES],
        "{reported:?}"
    );
    assert_eq!(
        catalogue_names(&pool).await.len(),
        LEDGER_INDEXES,
        "still physically there"
    );
    assert_eq!(registry_counts(&pool).await, (LEDGER_INDEXES as i64, 0));

    // Another programme that still requires them protects them.
    let other = compiled({
        let mut p = double_entry_ledger::program();
        p.name = "another_book".into();
        p
    });
    provision_indexes(&pool, &other, false).await.unwrap();
    let protected = provision_indexes(&pool, &successor, true).await.unwrap();
    assert!(actions(&protected).is_empty(), "{protected:?}");
    assert_eq!(catalogue_names(&pool).await.len(), LEDGER_INDEXES);

    // Once nobody does, prune drops them.
    let nobody = compiled(program("another_book").build());
    provision_indexes(&pool, &nobody, false).await.unwrap();
    let pruned = provision_indexes(&pool, &successor, true).await.unwrap();
    assert_eq!(
        actions(&pruned),
        vec![IndexAction::Stale; LEDGER_INDEXES],
        "{pruned:?}"
    );
    assert_eq!(pruned.pruned.len(), LEDGER_INDEXES);
    assert!(catalogue_names(&pool).await.is_empty());
    assert_eq!(registry_counts(&pool).await, (0, 0));
}

/// Two provisioners at once serialise on the advisory lock; each index
/// exists once afterwards and both runs succeed.
#[tokio::test]
async fn two_provisioners_serialise() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    let (first, second) = (ledger(), ledger());
    let (a, b) = tokio::join!(
        provision_indexes(&pool, &first, false),
        provision_indexes(&pool, &second, false)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    let mut all = actions(&a);
    all.extend(actions(&b));
    assert_eq!(
        all.iter().filter(|x| **x == IndexAction::Create).count(),
        LEDGER_INDEXES,
        "one of them created each index, the other kept it: {a:?} {b:?}"
    );
    assert_eq!(catalogue_names(&pool).await.len(), LEDGER_INDEXES);
    assert_eq!(
        registry_counts(&pool).await,
        (LEDGER_INDEXES as i64, LEDGER_INDEXES as i64)
    );
}

/// An interrupted concurrent build leaves an invalid index; the next run
/// repairs it, and running again keeps the repaired one.
#[tokio::test]
async fn an_invalid_index_is_repaired_idempotently() {
    let pool = test_pool().await;
    if !session_is_superuser(&pool).await {
        return;
    }
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    provision_indexes(&pool, &ledger(), false).await.unwrap();
    let (name, _, _) = first_spec(&pool).await;
    sqlx::query(
        "UPDATE pg_index SET indisvalid = false
         WHERE indexrelid = (SELECT oid FROM pg_class WHERE relname = $1)",
    )
    .bind(&name)
    .execute(&pool)
    .await
    .unwrap();

    let repaired = provision_indexes(&pool, &ledger(), false).await.unwrap();
    assert_eq!(
        repaired.entries[0].action,
        IndexAction::RepairInvalid,
        "{repaired:?}"
    );
    assert!(catalogue_names(&pool).await.iter().all(|(_, valid)| *valid));
    let again = provision_indexes(&pool, &ledger(), false).await.unwrap();
    assert_eq!(
        actions(&again),
        vec![IndexAction::Keep; LEDGER_INDEXES],
        "{again:?}"
    );
}

/// An interpreted programme requires the indexes its loads seek on: the
/// same physical contract as a compiled one, so `provision indexes` builds
/// them whatever `check -v` says about the invariants.
#[tokio::test]
async fn an_interpreted_programme_requires_the_indexes_its_loads_seek_on() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    let interpreted = PgProgram::interpreted(
        morpholog_core::CompiledProgram::new(double_entry_ledger::program()).unwrap(),
    );
    let report = plan_indexes(&pool, &interpreted).await.unwrap();
    let creates: Vec<(String, usize)> = report
        .entries
        .iter()
        .filter(|e| e.action == IndexAction::Create)
        .map(|e| (e.predicate.clone(), e.position))
        .collect();
    // A posting keys its period gate on the period: PeriodClosed[0].
    assert!(
        creates.contains(&("PeriodClosed".to_string(), 0)),
        "the posting's keyed read needs PeriodClosed[0], got {creates:?}"
    );
    // The compiled programme requires the same loads' indexes plus its
    // checks' own.
    let compiled_report = plan_indexes(&pool, &ledger()).await.unwrap();
    let compiled_creates: Vec<(String, usize)> = compiled_report
        .entries
        .iter()
        .filter(|e| e.action == IndexAction::Create)
        .map(|e| (e.predicate.clone(), e.position))
        .collect();
    for spec in &creates {
        assert!(
            compiled_creates.contains(spec),
            "{spec:?} beyond the compiled programme's"
        );
    }
}

/// A programme whose transformation only admits a predicate: on the
/// interpreted route its load seeks that predicate for the admitted
/// claim's membership, so one coordinate of the admit is provisioned
/// although no read keys it. The invariant's `or` keeps the programme
/// interpreted without asking.
#[tokio::test]
async fn an_admit_no_read_keys_still_provisions_one_coordinate() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    let program = morpholog_surface::parse_program(
        "program admit_only
predicate P(k: Subject, v: Decimal)
predicate Flag(k: Subject)
invariant flagged_or_not:
    P(k, _) implies (Flag(k) or not Flag(k))
transformation put(k, v):
    admit P(k, v)
",
    )
    .unwrap();
    let pg = compiled(program);
    assert!(
        matches!(
            pg.plan(),
            morpholog_postgres::InvariantPlan::Interpreted { .. }
        ),
        "the `or` keeps it interpreted"
    );
    let report = plan_indexes(&pool, &pg).await.unwrap();
    let creates: Vec<(String, usize)> = report
        .entries
        .iter()
        .filter(|e| e.action == IndexAction::Create)
        .map(|e| (e.predicate.clone(), e.position))
        .collect();
    assert_eq!(
        creates,
        vec![("P".to_string(), 0)],
        "one coordinate of the admit, the first, and not the amount"
    );
}

/// Statistics over an expression index exist only once the table is
/// analyzed after the build. Every applied run analyzes, so an index the
/// run merely adopts has statistics too; a dry run leaves them alone.
#[tokio::test]
async fn every_applied_run_analyzes_the_claims_table_and_a_dry_run_does_not() {
    let pool = test_pool().await;
    if !session_is_superuser(&pool).await {
        return;
    }
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    sqlx::raw_sql(
        "INSERT INTO morpholog.claims (predicate_name, arguments, asserted_in)
         SELECT 'JournalEntry',
                jsonb_build_array(jsonb_build_object('type', 'subject', 'value', 'e_' || i),
                                  jsonb_build_object('type', 'subject', 'value', 'd_' || i),
                                  jsonb_build_object('type', 'subject', 'value', 'p_' || (i % 7))),
                gen_random_uuid()
         FROM generate_series(1, 300) i",
    )
    .execute(&pool)
    .await
    .unwrap();
    let stats_for = |name: String| {
        let pool = pool.clone();
        async move {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM pg_stats WHERE schemaname = 'morpholog' AND tablename = $1",
            )
            .bind(name)
            .fetch_one(&pool)
            .await
            .unwrap()
        }
    };
    provision_indexes(&pool, &ledger(), false).await.unwrap();
    let (name, _) = catalogue_names(&pool).await.into_iter().next().unwrap();
    assert!(
        stats_for(name.clone()).await > 0,
        "{name} has statistics after the build"
    );

    // An earlier run built the index and stopped before analyzing.
    // The name comes from the catalogue, not from input.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DELETE FROM morpholog.index_requirement; DELETE FROM morpholog.managed_index;
         DELETE FROM pg_statistic WHERE starelid = 'morpholog.{name}'::regclass"
    )))
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(stats_for(name.clone()).await, 0);
    plan_indexes(&pool, &ledger()).await.unwrap();
    assert_eq!(
        stats_for(name.clone()).await,
        0,
        "a dry run analyzes nothing"
    );
    let adopting = provision_indexes(&pool, &ledger(), false).await.unwrap();
    assert_eq!(actions(&adopting), vec![IndexAction::Keep; LEDGER_INDEXES]);
    assert!(
        stats_for(name.clone()).await > 0,
        "{name} has statistics after adoption"
    );
}
