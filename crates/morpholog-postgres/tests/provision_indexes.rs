//! `provision indexes`: the reconciliation states, each provoked.
//!
//! Attacker capability where one is modelled: an operator with DDL on
//! the claims table, who may have created indexes of their own, in
//! Morpholog's namespace or outside it.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{pg_program, reset_db, session_is_superuser, test_pool};
use morpholog_core::ir_builder::program;
use morpholog_examples::double_entry_ledger;
use morpholog_postgres::{
    IndexAction, PgPool, PgProgram, StatisticsAction, plan_indexes, provision_indexes,
};
use sqlx::Row as _;

/// The ledger's requirement: its compiled checks' seeks, the case column
/// of its lineage check, and the two positions its transformations' gates
/// key on.
const LEDGER_INDEXES: usize = 6;

fn ledger() -> PgProgram {
    pg_program(double_entry_ledger::program())
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
    let report = plan_indexes(&pool, &[&ledger()], false).await.unwrap();
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
    let first = provision_indexes(&pool, &[&ledger()], false).await.unwrap();
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

    let again = provision_indexes(&pool, &[&ledger()], false).await.unwrap();
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
    provision_indexes(&pool, &[&ledger()], false).await.unwrap();
    sqlx::raw_sql("DELETE FROM morpholog.index_requirement; DELETE FROM morpholog.managed_index")
        .execute(&pool)
        .await
        .unwrap();
    let report = provision_indexes(&pool, &[&ledger()], false).await.unwrap();
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
    let entry = plan_indexes(pool, &[&ledger()], false)
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

    let report = provision_indexes(&pool, &[&ledger()], true).await.unwrap();
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

    let report = provision_indexes(&pool, &[&ledger()], true).await.unwrap();
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
    provision_indexes(&pool, &[&ledger()], false).await.unwrap();
    // The operator removes their index; B, another book, has Morpholog build it.
    sqlx::query("DROP INDEX morpholog.operator_made_this")
        .execute(&pool)
        .await
        .unwrap();
    let another = |name: &str| {
        let mut p = double_entry_ledger::program();
        p.name = name.into();
        pg_program(p)
    };
    let built = provision_indexes(&pool, &[&another("another_book")], false)
        .await
        .unwrap();
    assert_eq!(built.entries[0].action, IndexAction::Create, "{built:?}");
    // B stops needing anything and prunes: A still requires all of them.
    let nobody = pg_program(program("another_book").build());
    let pruned = provision_indexes(&pool, &[&nobody], true).await.unwrap();
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
    provision_indexes(&pool, &[&ledger()], false).await.unwrap();
    // The same identity, now needing nothing.
    let successor = pg_program(program("double_entry_ledger").build());
    let reported = provision_indexes(&pool, &[&successor], false)
        .await
        .unwrap();
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
    let other = pg_program({
        let mut p = double_entry_ledger::program();
        p.name = "another_book".into();
        p
    });
    provision_indexes(&pool, &[&other], false).await.unwrap();
    let protected = provision_indexes(&pool, &[&successor], true).await.unwrap();
    assert!(actions(&protected).is_empty(), "{protected:?}");
    assert_eq!(catalogue_names(&pool).await.len(), LEDGER_INDEXES);

    // Once nobody does, prune drops them.
    let nobody = pg_program(program("another_book").build());
    provision_indexes(&pool, &[&nobody], false).await.unwrap();
    let pruned = provision_indexes(&pool, &[&successor], true).await.unwrap();
    assert_eq!(
        actions(&pruned),
        vec![IndexAction::Stale; LEDGER_INDEXES],
        "{pruned:?}"
    );
    assert_eq!(
        pruned
            .pruned
            .iter()
            .filter(|n| n.starts_with("morpholog_ci_"))
            .count(),
        LEDGER_INDEXES
    );
    assert!(catalogue_names(&pool).await.is_empty());
    assert!(
        our_statistics(&pool).await.is_empty(),
        "their statistics go with them"
    );
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
    let (first, second) = ([&first], [&second]);
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
    provision_indexes(&pool, &[&ledger()], false).await.unwrap();
    let (name, _, _) = first_spec(&pool).await;
    sqlx::query(
        "UPDATE pg_index SET indisvalid = false
         WHERE indexrelid = (SELECT oid FROM pg_class WHERE relname = $1)",
    )
    .bind(&name)
    .execute(&pool)
    .await
    .unwrap();

    let repaired = provision_indexes(&pool, &[&ledger()], false).await.unwrap();
    assert_eq!(
        repaired.entries[0].action,
        IndexAction::RepairInvalid,
        "{repaired:?}"
    );
    assert!(catalogue_names(&pool).await.iter().all(|(_, valid)| *valid));
    let again = provision_indexes(&pool, &[&ledger()], false).await.unwrap();
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
        morpholog_core::PreparedProgram::new(double_entry_ledger::program()).unwrap(),
    );
    let report = plan_indexes(&pool, &[&interpreted], false).await.unwrap();
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
    let compiled_report = plan_indexes(&pool, &[&ledger()], false).await.unwrap();
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
    let pg = pg_program(program);
    assert!(
        matches!(
            pg.plan(),
            morpholog_postgres::InvariantPlan::Interpreted { .. }
        ),
        "the `or` keeps it interpreted"
    );
    let report = plan_indexes(&pool, &[&pg], false).await.unwrap();
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
    provision_indexes(&pool, &[&ledger()], false).await.unwrap();
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
    plan_indexes(&pool, &[&ledger()], false).await.unwrap();
    assert_eq!(
        stats_for(name.clone()).await,
        0,
        "a dry run analyzes nothing"
    );
    let adopting = provision_indexes(&pool, &[&ledger()], false).await.unwrap();
    assert_eq!(actions(&adopting), vec![IndexAction::Keep; LEDGER_INDEXES]);
    assert!(
        stats_for(name.clone()).await > 0,
        "{name} has statistics after adoption"
    );
}

async fn our_statistics(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT stxname::text FROM pg_statistic_ext
         WHERE stxnamespace = 'morpholog'::regnamespace AND stxname LIKE 'morpholog\\_cs\\_%'
         ORDER BY stxname",
    )
    .fetch_all(pool)
    .await
    .unwrap()
}

fn statistics_actions(
    report: &morpholog_postgres::ProvisionReport,
) -> Vec<(String, StatisticsAction)> {
    report
        .statistics
        .iter()
        .map(|s| (s.statistics_name.clone(), s.action))
        .collect()
}

/// Statistics follow positions, not indexes: one object per position any
/// required index seeks on, whatever predicates share it.
#[tokio::test]
async fn statistics_are_planned_per_position_created_once_and_then_kept() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    let plan = plan_indexes(&pool, &[&ledger()], false).await.unwrap();
    let positions: std::collections::BTreeSet<usize> =
        plan.entries.iter().map(|e| e.position).collect();
    let expected: Vec<String> = positions
        .iter()
        .map(|p| format!("morpholog_cs_vk1_p{p}"))
        .collect();
    assert!(
        positions.len() < plan.entries.len(),
        "the ledger shares a position across predicates"
    );
    assert_eq!(
        statistics_actions(&plan),
        expected
            .iter()
            .map(|n| (n.clone(), StatisticsAction::Create))
            .collect::<Vec<_>>()
    );
    assert!(
        our_statistics(&pool).await.is_empty(),
        "a dry run creates none"
    );

    let first = provision_indexes(&pool, &[&ledger()], false).await.unwrap();
    assert!(first.applied);
    assert_eq!(our_statistics(&pool).await, expected);
    let again = provision_indexes(&pool, &[&ledger()], false).await.unwrap();
    assert!(
        again
            .statistics
            .iter()
            .all(|s| s.action == StatisticsAction::Keep),
        "{again:?}"
    );
}

/// Attacker capability modelled: an operator with DDL on the claims table
/// who created statistics under Morpholog's name with another definition.
/// The run applies nothing, indexes included, and leaves theirs alone.
#[tokio::test]
async fn statistics_under_our_name_with_another_definition_apply_nothing() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    sqlx::raw_sql(
        "CREATE STATISTICS morpholog.morpholog_cs_vk1_p0 ON ((arguments ->> 0)) FROM morpholog.claims",
    )
    .execute(&pool)
    .await
    .unwrap();
    let report = provision_indexes(&pool, &[&ledger()], false).await.unwrap();
    assert!(report.has_conflict());
    assert!(!report.applied);
    assert!(
        report
            .statistics
            .iter()
            .any(|s| s.statistics_name == "morpholog_cs_vk1_p0"
                && s.action == StatisticsAction::Conflict),
        "{report:?}"
    );
    assert!(
        catalogue_names(&pool).await.is_empty(),
        "no index was built"
    );
    assert_eq!(registry_counts(&pool).await, (0, 0));
    assert_eq!(
        our_statistics(&pool).await,
        vec!["morpholog_cs_vk1_p0".to_string()],
        "only the operator's object, untouched"
    );
}

/// Statistics of the same name outside Morpholog's schema are not its own.
#[tokio::test]
async fn statistics_of_the_same_name_in_another_schema_are_not_ours() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    sqlx::raw_sql(
        "DROP STATISTICS IF EXISTS public.morpholog_cs_vk1_p0;
         CREATE STATISTICS public.morpholog_cs_vk1_p0 ON ((arguments ->> 0)) FROM morpholog.claims",
    )
    .execute(&pool)
    .await
    .unwrap();
    let report = plan_indexes(&pool, &[&ledger()], false).await.unwrap();
    sqlx::raw_sql("DROP STATISTICS public.morpholog_cs_vk1_p0")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!report.has_conflict(), "{report:?}");
    assert!(
        report
            .statistics
            .iter()
            .any(|s| s.statistics_name == "morpholog_cs_vk1_p0"
                && s.action == StatisticsAction::Create),
        "{report:?}"
    );
}

/// Attacker capability modelled: an operator with DDL on the claims table
/// who altered the statistics target of Morpholog's object. A target of
/// zero collects nothing, so an altered target is a conflict naming the
/// remedy, and restoring the default is kept again.
#[tokio::test]
async fn an_altered_statistics_target_is_a_conflict_until_restored() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    provision_indexes(&pool, &[&ledger()], false).await.unwrap();
    sqlx::raw_sql("ALTER STATISTICS morpholog.morpholog_cs_vk1_p0 SET STATISTICS 0")
        .execute(&pool)
        .await
        .unwrap();
    let report = provision_indexes(&pool, &[&ledger()], false).await.unwrap();
    assert!(!report.applied);
    let entry = report
        .statistics
        .iter()
        .find(|s| s.statistics_name == "morpholog_cs_vk1_p0")
        .unwrap();
    assert_eq!(entry.action, StatisticsAction::Conflict);
    assert!(
        entry.detail.contains("statistics target is 0")
            && entry.detail.contains("SET STATISTICS DEFAULT"),
        "{}",
        entry.detail
    );
    sqlx::raw_sql("ALTER STATISTICS morpholog.morpholog_cs_vk1_p0 SET STATISTICS DEFAULT")
        .execute(&pool)
        .await
        .unwrap();
    let restored = provision_indexes(&pool, &[&ledger()], false).await.unwrap();
    assert!(restored.applied, "{restored:?}");
}

/// A same-named object over the right expression that also covers a
/// column is not Morpholog's, and the conflict says what differs.
#[tokio::test]
async fn statistics_with_an_extra_column_conflict_and_name_it() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    let plan = plan_indexes(&pool, &[&ledger()], false).await.unwrap();
    let expression = plan
        .statistics
        .iter()
        .find(|s| s.statistics_name == "morpholog_cs_vk1_p0")
        .unwrap()
        .expression_sql
        .clone();
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE STATISTICS morpholog.morpholog_cs_vk1_p0 ON predicate_name, ({expression}) FROM morpholog.claims"
    )))
    .execute(&pool)
    .await
    .unwrap();
    let report = provision_indexes(&pool, &[&ledger()], false).await.unwrap();
    assert!(!report.applied);
    let entry = report
        .statistics
        .iter()
        .find(|s| s.statistics_name == "morpholog_cs_vk1_p0")
        .unwrap();
    assert_eq!(entry.action, StatisticsAction::Conflict);
    assert!(
        entry
            .detail
            .contains("also covers the columns predicate_name")
            && !entry.detail.contains("it covers"),
        "{}",
        entry.detail
    );
}

// ------------------------------------------------------------
// Several programmes in one call.
// ------------------------------------------------------------

/// A programme under `identity` requiring what `source` requires.
fn named(identity: &str, source: morpholog_core::Program) -> PgProgram {
    let mut p = source;
    p.name = identity.into();
    pg_program(p)
}

/// A programme under `identity` that requires nothing.
fn requiring_nothing(identity: &str) -> PgProgram {
    pg_program(program(identity).build())
}

async fn catalogue_oids(pool: &PgPool) -> Vec<(String, u32)> {
    sqlx::query(
        "SELECT c.relname, c.oid::int8 FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid
         WHERE i.indrelid = 'morpholog.claims'::regclass AND c.relname LIKE 'morpholog_ci_%'
         ORDER BY c.relname",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|r| (r.get(0), r.get::<i64, _>(1) as u32))
    .collect()
}

/// The registry's requirements as the report states them: index name to
/// the identities requiring it.
async fn recorded_requirements(pool: &PgPool) -> Vec<(String, Vec<String>)> {
    sqlx::query(
        "SELECT m.index_name, array_agg(r.program_identity ORDER BY r.program_identity)
         FROM morpholog.managed_index m
         JOIN morpholog.index_requirement r USING (spec_digest)
         GROUP BY m.index_name ORDER BY m.index_name",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|r| (r.get(0), r.get(1)))
    .collect()
}

/// What a report says of each index it reconciled, without the prose.
fn stated(report: &morpholog_postgres::ProvisionReport) -> Vec<(IndexAction, String, Vec<String>)> {
    report
        .entries
        .iter()
        .map(|e| (e.action, e.index_name.clone(), e.required_by.clone()))
        .collect()
}

// ------------------------------------------------------------
// Statistics follow the requirements' positions.
// ------------------------------------------------------------

async fn statistics_positions(pool: &PgPool) -> Vec<usize> {
    our_statistics(pool)
        .await
        .iter()
        .map(|name| {
            name.trim_start_matches("morpholog_cs_vk1_p")
                .parse()
                .unwrap()
        })
        .collect()
}

fn statistics_of(
    report: &morpholog_postgres::ProvisionReport,
) -> Vec<(usize, StatisticsAction, Vec<String>)> {
    report
        .statistics
        .iter()
        .map(|s| (s.position, s.action, s.required_by.clone()))
        .collect()
}

/// A requirement met by an operator's own index records its position
/// like any other, so its statistics survive another programme's prune.
#[tokio::test]
async fn an_operator_satisfied_requirement_keeps_its_statistics_through_anothers_prune() {
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
    let report = provision_indexes(&pool, &[&ledger()], false).await.unwrap();
    assert!(
        report
            .entries
            .iter()
            .any(|e| e.action == IndexAction::SatisfiedExternally),
        "{report:?}"
    );
    let positions = statistics_positions(&pool).await;
    assert!(!positions.is_empty());

    let pruned = provision_indexes(&pool, &[&requiring_nothing("other_book")], true)
        .await
        .unwrap();
    assert!(pruned.pruned.is_empty(), "{pruned:?}");
    assert_eq!(statistics_positions(&pool).await, positions);
    assert!(pruned.positions_unknown_for.is_empty(), "{pruned:?}");
    for entry in &pruned.statistics {
        assert_eq!(entry.action, StatisticsAction::Keep, "{entry:?}");
        assert_eq!(entry.required_by, ["double_entry_ledger"], "{entry:?}");
    }
}

/// Statistics no programme needs any more are stale: reported, dropped
/// only under prune, and only then.
#[tokio::test]
async fn statistics_nobody_requires_are_stale_and_pruned_only_on_request() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    provision_indexes(&pool, &[&ledger()], false).await.unwrap();
    let positions = statistics_positions(&pool).await;
    let retired = requiring_nothing("double_entry_ledger");

    let reported = provision_indexes(&pool, &[&retired], false).await.unwrap();
    assert_eq!(
        statistics_of(&reported),
        positions
            .iter()
            .map(|p| (*p, StatisticsAction::Stale, Vec::new()))
            .collect::<Vec<_>>(),
        "{reported:?}"
    );
    assert_eq!(
        statistics_positions(&pool).await,
        positions,
        "kept without prune"
    );

    let pruned = provision_indexes(&pool, &[&retired], true).await.unwrap();
    assert!(statistics_positions(&pool).await.is_empty(), "{pruned:?}");
    let mut dropped: Vec<String> = pruned
        .statistics
        .iter()
        .map(|s| s.statistics_name.clone())
        .collect();
    dropped.extend(pruned.entries.iter().map(|e| e.index_name.clone()));
    dropped.sort();
    let mut recorded = pruned.pruned.clone();
    recorded.sort();
    assert_eq!(recorded, dropped, "pruned names every object dropped");
}

/// Another programme's statistics object that has gone missing is
/// created again: its position is all a statistics object needs.
#[tokio::test]
async fn a_missing_statistics_object_another_programme_requires_is_created() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    provision_indexes(&pool, &[&ledger()], false).await.unwrap();
    let positions = statistics_positions(&pool).await;
    let gone = format!("morpholog_cs_vk1_p{}", positions[0]);
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP STATISTICS morpholog.{gone}"
    )))
    .execute(&pool)
    .await
    .unwrap();

    let report = provision_indexes(&pool, &[&requiring_nothing("other_book")], false)
        .await
        .unwrap();
    let entry = report
        .statistics
        .iter()
        .find(|s| s.statistics_name == gone)
        .unwrap_or_else(|| panic!("{report:?}"));
    assert_eq!(entry.action, StatisticsAction::Create);
    assert_eq!(entry.required_by, ["double_entry_ledger"]);
    assert_eq!(
        statistics_positions(&pool).await,
        positions,
        "created again"
    );
}

/// An object under Morpholog's name with another definition is a conflict
/// whoever requires it, and a prune never drops it.
#[tokio::test]
async fn an_unrequired_statistics_object_of_another_definition_is_never_pruned() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    sqlx::raw_sql(
        "CREATE STATISTICS morpholog.morpholog_cs_vk1_p9 ON predicate_name, (arguments -> 9) FROM morpholog.claims",
    )
    .execute(&pool)
    .await
    .unwrap();
    let report = provision_indexes(&pool, &[&ledger()], true).await.unwrap();
    let stray = report
        .statistics
        .iter()
        .find(|s| s.position == 9)
        .unwrap_or_else(|| panic!("{report:?}"));
    assert_eq!(stray.action, StatisticsAction::Conflict, "{stray:?}");
    assert!(stray.required_by.is_empty());
    assert!(!report.applied, "a conflict fails closed");
    assert!(
        our_statistics(&pool)
            .await
            .contains(&"morpholog_cs_vk1_p9".to_string()),
        "left exactly as found"
    );
}

/// A requirement recorded without its position, by a binary from before
/// positions were recorded, whose specification no managed index resolves:
/// unknown, so nothing is stale, and the report names the programme to
/// provision again. Provisioning it again records the position.
#[tokio::test]
async fn an_unresolved_position_keeps_every_statistics_object_and_names_the_programme() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    provision_indexes(&pool, &[&ledger()], false).await.unwrap();
    let positions = statistics_positions(&pool).await;
    // As an older binary records a requirement an operator's index met:
    // no managed index carries this digest, and no position is recorded.
    sqlx::query(
        "INSERT INTO morpholog.index_requirement (program_identity, spec_digest, program_hash)
         VALUES ('legacy_book', 'a_digest_no_managed_index_carries', 'sha256:0')",
    )
    .execute(&pool)
    .await
    .unwrap();

    let retired = requiring_nothing("double_entry_ledger");
    let held = provision_indexes(&pool, &[&retired], true).await.unwrap();
    assert_eq!(held.positions_unknown_for, ["legacy_book"], "{held:?}");
    // The retired ledger's indexes go; no statistics object does.
    assert!(
        !held.pruned.is_empty() && held.pruned.iter().all(|n| n.starts_with("morpholog_ci_")),
        "{held:?}"
    );
    assert_eq!(statistics_positions(&pool).await, positions);
    for entry in &held.statistics {
        assert_eq!(entry.action, StatisticsAction::Keep, "{entry:?}");
        assert!(
            entry.required_by.is_empty(),
            "unknown is not a requirement: {entry:?}"
        );
        assert!(entry.detail.contains("legacy_book"), "{entry:?}");
    }

    // The legacy programme, provisioned again, records what it needs.
    provision_indexes(&pool, &[&requiring_nothing("legacy_book")], false)
        .await
        .unwrap();
    let pruned = provision_indexes(&pool, &[&retired], true).await.unwrap();
    assert!(pruned.positions_unknown_for.is_empty(), "{pruned:?}");
    assert!(statistics_positions(&pool).await.is_empty(), "{pruned:?}");
}

/// A requirement recorded without its position whose specification a
/// managed index carries is not unknown: the index says the position.
#[tokio::test]
async fn a_position_left_unrecorded_is_read_from_the_managed_index() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    provision_indexes(&pool, &[&ledger()], false).await.unwrap();
    let positions = statistics_positions(&pool).await;
    // As an older binary would record the ledger's own requirements.
    sqlx::query("UPDATE morpholog.index_requirement SET position = NULL")
        .execute(&pool)
        .await
        .unwrap();

    let report = provision_indexes(&pool, &[&requiring_nothing("other_book")], true)
        .await
        .unwrap();
    assert!(report.positions_unknown_for.is_empty(), "{report:?}");
    assert!(report.pruned.is_empty(), "{report:?}");
    assert_eq!(statistics_positions(&pool).await, positions);
    for entry in &report.statistics {
        assert_eq!(entry.required_by, ["double_entry_ledger"], "{entry:?}");
    }
}

/// A position decides what a prune drops, so a registry that disagrees
/// with itself about one is refused before any change: the recorded
/// requirement against the managed index of the same specification.
#[tokio::test]
async fn a_registry_disagreeing_about_a_position_is_refused_before_any_change() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    provision_indexes(&pool, &[&ledger()], false).await.unwrap();
    let before = (
        catalogue_oids(&pool).await,
        statistics_positions(&pool).await,
    );
    sqlx::query(
        "UPDATE morpholog.index_requirement SET position = position + 1
         WHERE spec_digest = (SELECT min(spec_digest) FROM morpholog.index_requirement)",
    )
    .execute(&pool)
    .await
    .unwrap();

    for prune in [false, true] {
        let error = provision_indexes(&pool, &[&requiring_nothing("other_book")], prune)
            .await
            .unwrap_err();
        assert!(
            matches!(&error, morpholog_postgres::PgError::InvalidState(m) if m.contains("agrees with itself")),
            "{error:?}"
        );
    }
    assert_eq!(
        (
            catalogue_oids(&pool).await,
            statistics_positions(&pool).await
        ),
        before,
        "nothing changed"
    );
}

/// One programme stops requiring the indexes another takes up. Named
/// together, the prune sees both replaced and drops nothing. Named one at
/// a time with the prune on the first, the indexes go and are built again:
/// the ordering rule the union removes.
#[tokio::test]
async fn pruning_the_union_keeps_what_one_programme_drops_and_another_takes_up() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    let book = || named("book", double_entry_ledger::program());
    let other = || named("other_book", double_entry_ledger::program());

    provision_indexes(&pool, &[&book(), &requiring_nothing("other_book")], false)
        .await
        .unwrap();
    let built = catalogue_oids(&pool).await;
    assert_eq!(built.len(), LEDGER_INDEXES);

    let together = provision_indexes(&pool, &[&requiring_nothing("book"), &other()], true)
        .await
        .unwrap();
    assert!(
        together.applied && together.pruned.is_empty(),
        "{together:?}"
    );
    assert_eq!(
        actions(&together),
        vec![IndexAction::Keep; LEDGER_INDEXES],
        "{together:?}"
    );
    assert_eq!(catalogue_oids(&pool).await, built, "no index was rebuilt");

    // Back to the start, then the same change one programme at a time.
    provision_indexes(&pool, &[&book(), &requiring_nothing("other_book")], false)
        .await
        .unwrap();
    let first = provision_indexes(&pool, &[&requiring_nothing("book")], true)
        .await
        .unwrap();
    assert_eq!(
        first
            .pruned
            .iter()
            .filter(|n| n.starts_with("morpholog_ci_"))
            .count(),
        LEDGER_INDEXES,
        "{first:?}"
    );
    provision_indexes(&pool, &[&other()], false).await.unwrap();
    let rebuilt = catalogue_oids(&pool).await;
    assert_eq!(rebuilt.len(), LEDGER_INDEXES);
    assert!(
        rebuilt
            .iter()
            .zip(&built)
            .all(|(now, then)| now.1 != then.1),
        "each was dropped and built again: {built:?} then {rebuilt:?}"
    );
}

/// A conflict under one programme's index name stops the whole call: the
/// other programme's indexes are not built and no requirement is recorded.
#[tokio::test]
async fn a_conflict_in_one_programme_applies_nothing_for_any() {
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
    let approvals = pg_program(morpholog_examples::approval_controls::program());
    let alone = plan_indexes(&pool, &[&approvals], false).await.unwrap();
    assert!(
        !alone.entries.is_empty() && !alone.has_conflict(),
        "the second programme has requirements of its own and no conflict: {alone:?}"
    );

    let report = provision_indexes(&pool, &[&approvals, &ledger()], true)
        .await
        .unwrap();
    assert!(report.has_conflict() && !report.applied, "{report:?}");
    assert_eq!(
        catalogue_names(&pool).await,
        vec![(name, true)],
        "only the operator's index exists"
    );
    assert_eq!(registry_counts(&pool).await, (0, 0), "no registry write");
}

/// A programme the call does not name keeps its indexes through another's
/// prune, and the report says which programme protects each.
#[tokio::test]
async fn a_requirement_outside_the_call_protects_and_is_reported() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    provision_indexes(&pool, &[&ledger()], false).await.unwrap();
    let built = catalogue_oids(&pool).await;

    let report = provision_indexes(&pool, &[&requiring_nothing("other_book")], true)
        .await
        .unwrap();
    assert!(report.applied && report.entries.is_empty(), "{report:?}");
    assert!(report.pruned.is_empty(), "{report:?}");
    let protected: Vec<(String, Vec<String>)> = report
        .required_elsewhere
        .iter()
        .map(|e| (e.index_name.clone(), e.required_by.clone()))
        .collect();
    let expected: Vec<(String, Vec<String>)> = built
        .iter()
        .map(|(name, _)| (name.clone(), vec!["double_entry_ledger".to_string()]))
        .collect();
    assert_eq!(protected, expected);
    assert_eq!(catalogue_oids(&pool).await, built);
}

/// Requirements are kept per identity, so the second of two programmes
/// under one name would replace the first's. Refused before any change.
#[tokio::test]
async fn a_programme_named_twice_is_refused_before_anything_changes() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    let error = provision_indexes(
        &pool,
        &[&ledger(), &requiring_nothing("double_entry_ledger")],
        false,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&error, morpholog_postgres::PgError::ProgramNamedTwice(name) if name == "double_entry_ledger"),
        "{error:?}"
    );
    assert!(catalogue_names(&pool).await.is_empty());
    assert_eq!(registry_counts(&pool).await, (0, 0));
}

/// A call that names no programme is refused, prune or not: recorded
/// indexes stay, and no report names no programme.
#[tokio::test]
async fn a_call_naming_no_programme_is_refused() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    provision_indexes(&pool, &[&ledger()], false).await.unwrap();
    // Stale, so a prune that ran would drop them.
    provision_indexes(&pool, &[&requiring_nothing("double_entry_ledger")], false)
        .await
        .unwrap();
    let built = catalogue_oids(&pool).await;
    assert_eq!(built.len(), LEDGER_INDEXES);

    for planned in [true, false] {
        let error = if planned {
            plan_indexes(&pool, &[], true).await.unwrap_err()
        } else {
            provision_indexes(&pool, &[], true).await.unwrap_err()
        };
        assert!(
            matches!(error, morpholog_postgres::PgError::NoProgramNamed),
            "{error:?}"
        );
    }
    assert_eq!(catalogue_oids(&pool).await, built);
}

/// A dry run changes nothing and states what the applying run then does:
/// the same actions, the same requirements, the same stale set. After the
/// applying run the registry holds exactly the requirements both stated.
#[tokio::test]
async fn a_dry_run_states_what_the_applying_run_does() {
    let pool = test_pool().await;
    reset_db(&pool).await;
    drop_our_indexes(&pool).await;
    // Three programmes on one database: the ledger, a second book sharing
    // its indexes, and the approvals with their own.
    let approvals = || pg_program(morpholog_examples::approval_controls::program());
    provision_indexes(
        &pool,
        &[
            &ledger(),
            &named("second_book", double_entry_ledger::program()),
            &approvals(),
        ],
        false,
    )
    .await
    .unwrap();
    // The change: the second book requires nothing any more, the ledger
    // takes up the settlement programme's requirements instead of its own,
    // and the approvals are not named.
    let successor = || {
        named(
            "double_entry_ledger",
            morpholog_examples::settlement_netting::program(),
        )
    };
    let nothing = || requiring_nothing("second_book");

    let catalogue_before = catalogue_oids(&pool).await;
    let registry_before = recorded_requirements(&pool).await;
    let planned = plan_indexes(&pool, &[&successor(), &nothing()], true)
        .await
        .unwrap();
    assert!(planned.dry_run && planned.prune && !planned.applied);
    assert_eq!(catalogue_oids(&pool).await, catalogue_before);
    assert_eq!(recorded_requirements(&pool).await, registry_before);

    let applied = provision_indexes(&pool, &[&nothing(), &successor()], true)
        .await
        .unwrap();
    assert!(!applied.dry_run && applied.prune && applied.applied);
    assert_eq!(stated(&planned), stated(&applied));
    assert_eq!(statistics_of(&planned), statistics_of(&applied));
    assert_eq!(planned.required_elsewhere, applied.required_elsewhere);
    assert_eq!(planned.programs, applied.programs);

    let of = |action: IndexAction| -> Vec<String> {
        applied
            .entries
            .iter()
            .filter(|e| e.action == action)
            .map(|e| e.index_name.clone())
            .collect()
    };
    assert_eq!(of(IndexAction::Stale).len(), LEDGER_INDEXES, "{applied:?}");
    assert_eq!(of(IndexAction::Stale), applied.pruned);
    assert!(!of(IndexAction::Create).is_empty(), "{applied:?}");
    assert!(
        !applied.required_elsewhere.is_empty()
            && applied
                .required_elsewhere
                .iter()
                .all(|e| e.required_by == ["approval_controls"]),
        "{applied:?}"
    );

    // One relation: what the report stated is what the registry now holds.
    let mut reported: Vec<(String, Vec<String>)> = applied
        .entries
        .iter()
        .filter(|e| e.action != IndexAction::Stale)
        .map(|e| (e.index_name.clone(), e.required_by.clone()))
        .chain(
            applied
                .required_elsewhere
                .iter()
                .map(|e| (e.index_name.clone(), e.required_by.clone())),
        )
        .collect();
    reported.sort();
    assert_eq!(recorded_requirements(&pool).await, reported);
}
