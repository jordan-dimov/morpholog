//! Proves a history view is the audit log's own record of a predicate.
//! For every gallery programme the views are applied, a generated state
//! is seeded, and every act is proposed with generated arguments. Each
//! history view is then held three ways: its rows are the effective
//! operations the audit rows record, in order, on the original tagged
//! arguments; replaying them over the seed, inserts adding and deletes
//! removing, gives the claims table exactly; and every typed field is the
//! base view's decoding of the same arguments. Built histories reach what
//! generation seldom does: an admission of a claim already held, a batch
//! of acts in one order, a row written under another declaration, and a
//! writer that commits below a row already shown. `DATABASE_URL`-gated.

use std::collections::BTreeSet;

use morpholog_core::{EvalValue, PreparedProgram, Program, Transition, ValidatedProgram};
use morpholog_test_support::differential::{sample_args, sample_state};
use morpholog_test_support::{dec, qty, subj, test_actor};
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::attestation::Proposal;
use crate::audit::{OperationKind, audit_resume_watermark, list_audit_rows};
use crate::compiled_differential::test_pool;
use crate::load_differential::seed;
use crate::program::PgProgram;
use crate::propose::propose_against_pg;
use crate::sql_quote::quote_ident;
use crate::sql_views::{DerivedSource, ViewDescription, column_sql_at, render_views};
use crate::transact::propose_all_against_pg;

const SCHEMA: &str = "history_views";

/// A `tag_history` row: item, label, note, the shape flag, the original.
type TagRow = (Option<String>, Option<String>, Option<String>, bool, String);

/// Apply `program`'s views with their history views, and return the
/// history views' descriptions; `None` where the programme's names
/// cannot be rendered at all.
async fn apply(pool: &PgPool, validated: &ValidatedProgram<'_>) -> Option<Vec<ViewDescription>> {
    let rendered = render_views(
        validated,
        SCHEMA,
        &format!("sha256:{}", "0".repeat(64)),
        DerivedSource::Cache,
        &Default::default(),
        true,
    )
    .ok()?;
    // Safe: the schema name is this test's constant.
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP SCHEMA IF EXISTS {SCHEMA} CASCADE"
    )))
    .execute(pool)
    .await
    .unwrap();
    sqlx::raw_sql(sqlx::AssertSqlSafe(rendered.sql))
        .execute(pool)
        .await
        .unwrap();
    Some(
        rendered
            .surface
            .views
            .into_iter()
            .filter(|v| v.kind == "history")
            .collect(),
    )
}

fn pg(source: &str) -> (Program, PgProgram) {
    let program = morpholog_surface::parse_program(source).expect("fragment parses");
    let pg = PgProgram::new(PreparedProgram::new(program.clone()).unwrap());
    (program, pg)
}

/// Propose one act; whether it committed.
async fn act(pool: &PgPool, pg: &PgProgram, name: &str, args: Vec<EvalValue>) -> bool {
    let transition = Transition {
        transformation_name: name.into(),
        args,
        actor: test_actor(),
        decided_at: None,
    };
    matches!(
        propose_against_pg(pool, pg, &Proposal::gateway(&transition)).await,
        Ok(crate::PgProposalOutcome::Committed { .. })
    )
}

/// A history view's rows in the log's order: the transition, the
/// operation's place in it, `del` or `ins`, the original arguments, and
/// the shape flag.
async fn view_rows(pool: &PgPool, view: &str) -> Vec<(Uuid, i64, String, serde_json::Value, bool)> {
    // Safe: the view name is quoted, the schema is this test's constant.
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT _morpholog_transition_id, _morpholog_operation, _morpholog_op,
                _morpholog_arguments, _morpholog_shape_matches
         FROM {SCHEMA}.{}
         ORDER BY _morpholog_audit_order, _morpholog_act_ordinal, _morpholog_operation",
        quote_ident(view)
    )))
    .fetch_all(pool)
    .await
    .unwrap()
}

/// The arguments the claims table holds for `predicate`, each as
/// PostgreSQL prints it.
async fn held(pool: &PgPool, predicate: &str) -> BTreeSet<String> {
    sqlx::query_scalar::<_, String>(
        "SELECT arguments::text FROM morpholog.claims WHERE predicate_name = $1",
    )
    .bind(predicate)
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .collect()
}

/// Hold every history view of `validated` to the audit rows and to the
/// claims table, starting from what the table held before the acts.
/// Returns the history rows compared and the deletes among them.
async fn agree(
    pool: &PgPool,
    validated: &ValidatedProgram<'_>,
    views: &[ViewDescription],
    before: &dyn Fn(&str) -> BTreeSet<String>,
    label: &str,
) -> (usize, usize) {
    let audit = list_audit_rows(pool).await.unwrap();
    let (mut compared, mut deletes) = (0, 0);
    for view in views {
        let predicate = view.predicate.as_str();
        let rows = view_rows(pool, &view.view).await;

        let logged: Vec<(Uuid, i64, String, serde_json::Value)> = audit
            .iter()
            .flat_map(|row| {
                row.operations
                    .iter()
                    .flatten()
                    .enumerate()
                    .filter(|(_, op)| {
                        op.predicate.as_str() == predicate && op.op != OperationKind::Noop
                    })
                    .map(|(n, op)| {
                        let kind = if op.op == OperationKind::Del {
                            "del"
                        } else {
                            "ins"
                        };
                        (
                            row.transition_id,
                            i64::try_from(n).unwrap() + 1,
                            kind.to_string(),
                            serde_json::to_value(&op.args).unwrap(),
                        )
                    })
            })
            .collect();
        let shown: Vec<(Uuid, i64, String, serde_json::Value)> = rows
            .iter()
            .map(|(t, n, op, args, _)| (*t, *n, op.clone(), args.clone()))
            .collect();
        assert_eq!(
            shown, logged,
            "{label}: {predicate}: the view and the log disagree"
        );
        assert!(
            rows.iter().all(|(.., shape)| *shape),
            "{label}: {predicate}: a row this programme wrote has another shape"
        );

        // Replay over what the table held: a delete finds its claim, an
        // insert does not, and the end is the table.
        let texts: Vec<(String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT _morpholog_op, _morpholog_arguments::text FROM {SCHEMA}.{}
             ORDER BY _morpholog_audit_order, _morpholog_act_ordinal, _morpholog_operation",
            quote_ident(&view.view)
        )))
        .fetch_all(pool)
        .await
        .unwrap();
        let mut folded = before(predicate);
        for (op, arguments) in texts {
            if op == "del" {
                deletes += 1;
                assert!(
                    folded.remove(&arguments),
                    "{label}: {predicate}: a delete of a claim not held: {arguments}"
                );
            } else {
                assert!(
                    folded.insert(arguments.clone()),
                    "{label}: {predicate}: an insert of a claim already held: {arguments}"
                );
            }
        }
        assert_eq!(
            folded,
            held(pool, predicate).await,
            "{label}: {predicate}: replaying the history does not give the table"
        );

        // Each typed field is the base view's decoding of the arguments.
        let decl = validated
            .as_program()
            .predicates
            .iter()
            .find(|p| p.name.as_str() == predicate)
            .unwrap();
        let differing: Vec<String> = decl
            .args
            .iter()
            .enumerate()
            .map(|(i, arg)| {
                format!(
                    "h.{} IS DISTINCT FROM ({})",
                    quote_ident(&arg.name),
                    column_sql_at(&arg.kind, "h._morpholog_", i).expr
                )
            })
            .collect();
        if !differing.is_empty() {
            let (wrong,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM {SCHEMA}.{} h WHERE {}",
                quote_ident(&view.view),
                differing.join(" OR ")
            )))
            .fetch_one(pool)
            .await
            .unwrap();
            assert_eq!(wrong, 0, "{label}: {predicate}: a typed field differs");
        }

        // Each lookup returns the view's rows for its key, in the view's
        // order, for every key the view holds.
        for lookup in &view.lookups {
            let typed: Vec<(String, String)> = lookup
                .key
                .iter()
                .map(|field| {
                    let column = view.columns.iter().find(|c| &c.name == field).unwrap();
                    (quote_ident(field), column.sql_type.clone())
                })
                .collect();
            let keys: Vec<Vec<String>> =
                sqlx::query_scalar::<_, Vec<String>>(sqlx::AssertSqlSafe(format!(
                    "SELECT DISTINCT ARRAY[{}] FROM {SCHEMA}.{} WHERE {}",
                    typed
                        .iter()
                        .map(|(c, _)| format!("{c}::text"))
                        .collect::<Vec<_>>()
                        .join(", "),
                    quote_ident(&view.view),
                    typed
                        .iter()
                        .map(|(c, _)| format!("{c} IS NOT NULL"))
                        .collect::<Vec<_>>()
                        .join(" AND ")
                )))
                .fetch_all(pool)
                .await
                .unwrap();
            for key in keys {
                let values: Vec<String> = key
                    .iter()
                    .zip(&typed)
                    .map(|(v, (_, t))| format!("{}::{t}", crate::sql_quote::quote_literal(v)))
                    .collect();
                let order = "ORDER BY h._morpholog_audit_order, h._morpholog_act_ordinal, h._morpholog_operation";
                let through: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT h::text FROM {SCHEMA}.{}({}) h",
                    quote_ident(&lookup.function),
                    values.join(", ")
                )))
                .fetch_all(pool)
                .await
                .unwrap();
                let filtered: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT h::text FROM {SCHEMA}.{} h WHERE {} {order}",
                    quote_ident(&view.view),
                    typed
                        .iter()
                        .zip(&values)
                        .map(|((c, _), v)| format!("h.{c} = {v}"))
                        .collect::<Vec<_>>()
                        .join(" AND ")
                )))
                .fetch_all(pool)
                .await
                .unwrap();
                assert_eq!(
                    through, filtered,
                    "{label}: {}: the lookup and the view disagree for {key:?}",
                    lookup.function
                );
                LOOKUPS_CHECKED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        compared += rows.len();
    }
    (compared, deletes)
}

/// Lookup calls compared with the view, over every test in this module.
static LOOKUPS_CHECKED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[tokio::test]
async fn history_views_replay_to_the_claims_table_over_the_gallery() {
    let pool = test_pool().await;
    let (mut programmes, mut commits, mut compared, mut deletes) = (0usize, 0usize, 0usize, 0usize);
    for program in morpholog_examples::all_programs() {
        let validated = program.validated().unwrap();
        let Some(views) = apply(&pool, &validated).await else {
            continue;
        };
        programmes += 1;
        let pg = PgProgram::new(PreparedProgram::new(program.clone()).unwrap());
        for salt in 0..2u64 {
            crate::testing::reset_database(&pool).await.unwrap();
            let mut claims = sample_state(&program, 2, salt).claims().to_vec();
            claims.sort_by_key(|c| format!("{c:?}"));
            claims.dedup();
            seed(&pool, &claims).await;
            let mut before = std::collections::BTreeMap::new();
            for view in &views {
                before.insert(view.predicate.clone(), held(&pool, &view.predicate).await);
            }
            for t in &program.transformations {
                for arg_salt in 0..3u64 {
                    let Some(args) = sample_args(&program, t, salt + arg_salt) else {
                        continue;
                    };
                    commits += usize::from(act(&pool, &pg, t.name.as_str(), args).await);
                }
            }
            let (rows, dels) = agree(
                &pool,
                &validated,
                &views,
                &|predicate| before[predicate].clone(),
                &format!("{} salt {salt}", program.name),
            )
            .await;
            compared += rows;
            deletes += dels;
        }
    }
    eprintln!(
        "history differential: {programmes} programmes, {commits} commits, {compared} rows, {deletes} deletes"
    );
    assert!(
        programmes >= 10,
        "programmes with history views: {programmes}"
    );
    assert!(commits >= 30, "committed acts: {commits}");
    assert!(compared >= 60, "history rows compared: {compared}");
    assert!(deletes >= 1, "no history held a delete");
    let lookups = LOOKUPS_CHECKED.load(std::sync::atomic::Ordering::Relaxed);
    assert!(lookups >= 20, "lookup calls compared: {lookups}");
}

const STOCK: &str = "program stock
predicate StockItem(item: Subject, part: Subject, quantity: Decimal)
transformation receive(item, part, quantity):
    admit StockItem(item, part, quantity)
transformation recount(item, quantity):
    bind row: StockItem(item, part, old)
    retract StockItem(item, part, old)
    admit StockItem(item, part, quantity)
transformation scrap(item):
    retract StockItem(item: item, ..)
";

/// One item's life, act by act: an admission, the same admission again,
/// which changes nothing and is left out, a revision as a delete and an
/// insert in one act, two acts of one batch in one order, and a
/// retraction.
#[tokio::test]
async fn a_history_view_shows_each_change_and_no_admission_that_changed_nothing() {
    let pool = test_pool().await;
    crate::testing::reset_database(&pool).await.unwrap();
    let (program, pg) = pg(STOCK);
    let validated = program.validated().unwrap();
    let views = apply(&pool, &validated).await.unwrap();

    assert!(act(&pool, &pg, "receive", vec![subj("i1"), subj("p"), dec(5)]).await);
    assert!(act(&pool, &pg, "receive", vec![subj("i1"), subj("p"), dec(5)]).await);
    assert!(act(&pool, &pg, "recount", vec![subj("i1"), dec(7)]).await);
    let batch: Vec<Transition> = [
        ("receive", vec![subj("i2"), subj("p"), dec(1)]),
        ("recount", vec![subj("i2"), dec(2)]),
    ]
    .into_iter()
    .map(|(name, args)| Transition {
        transformation_name: name.into(),
        args,
        actor: test_actor(),
        decided_at: None,
    })
    .collect();
    let proposals: Vec<Proposal> = batch.iter().map(Proposal::gateway).collect();
    assert!(matches!(
        propose_all_against_pg(&pool, &pg, &proposals)
            .await
            .unwrap(),
        crate::PgAtomicOutcome::Committed { .. }
    ));
    assert!(act(&pool, &pg, "scrap", vec![subj("i1")]).await);

    let audit = list_audit_rows(&pool).await.unwrap();
    assert!(
        audit[1]
            .operations
            .iter()
            .flatten()
            .all(|op| op.op == OperationKind::Noop),
        "the second receive recorded only a no-op: {:?}",
        audit[1].operations
    );

    // Safe: constant text.
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT _morpholog_transformation, _morpholog_act_ordinal, _morpholog_op, item,
                quantity::text, _morpholog_actor, _morpholog_asserted_in IS NULL,
                _morpholog_audit_order, _morpholog_ordered_at
         FROM {SCHEMA}.stock_item_history
         ORDER BY _morpholog_audit_order, _morpholog_act_ordinal, _morpholog_operation"
    )))
    .fetch_all(&pool)
    .await
    .unwrap();
    let shown: Vec<(String, i32, String, String, String)> = rows
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4)))
        .collect();
    let expect = |act: &str, ordinal: i32, op: &str, item: &str, quantity: &str| {
        (
            act.to_string(),
            ordinal,
            op.to_string(),
            item.to_string(),
            quantity.to_string(),
        )
    };
    assert_eq!(
        shown,
        vec![
            expect("receive", 0, "ins", "i1", "5"),
            expect("recount", 0, "del", "i1", "5"),
            expect("recount", 0, "ins", "i1", "7"),
            expect("receive", 0, "ins", "i2", "1"),
            expect("recount", 1, "del", "i2", "1"),
            expect("recount", 1, "ins", "i2", "2"),
            expect("scrap", 0, "del", "i1", "7"),
        ]
    );
    let orders: Vec<i64> = rows.iter().map(|r| r.get(7)).collect();
    assert_eq!(orders[3], orders[5], "a batch's acts share one order");
    assert!(orders.windows(2).all(|w| w[0] <= w[1]));
    let instants: Vec<jiff_sqlx::Timestamp> = rows.iter().map(|r| r.get(8)).collect();
    assert!(
        instants
            .windows(2)
            .all(|w| w[0].to_jiff() <= w[1].to_jiff()),
        "ordered_at follows the order"
    );
    for r in &rows {
        assert_eq!(r.get::<String, _>(5), "test_actor");
        assert!(r.get::<bool, _>(6), "only a baseline row names an admitter");
    }

    agree(&pool, &validated, &views, &|_| BTreeSet::new(), "stock").await;
}

/// The log outlives a declaration. Rows written under one programme are
/// read through views generated from another: a field whose stored kind
/// is not the declared one is NULL instead of failing the read, a
/// quantity in another unit is not decoded, the flag says the structure
/// differs, and the original arguments stay. A field renamed with its
/// kind kept decodes under the new name with the flag true: the flag is
/// about structure, and says nothing of what the field meant.
#[tokio::test]
async fn a_row_written_under_another_declaration_reads_without_breaking_the_view() {
    let pool = test_pool().await;
    crate::testing::reset_database(&pool).await.unwrap();
    let (_, written) = pg("program before
predicate Price(product: Subject, wholesale_price: Decimal)
predicate Lot(lot: Subject, size: Decimal[kg])
predicate Tag(item: Subject, label: Subject)
transformation set_price(product, price):
    admit Price(product, price)
transformation add_lot(lot, size):
    admit Lot(lot, size)
transformation tag(item, label):
    admit Tag(item, label)
");
    assert!(act(&pool, &written, "set_price", vec![subj("w"), dec(3)]).await);
    assert!(act(&pool, &written, "add_lot", vec![subj("l"), qty("40", "kg")]).await);
    assert!(act(&pool, &written, "tag", vec![subj("i"), subj("red")]).await);

    let (program, read) = pg("program after
predicate Price(product: Subject, retail_price: Decimal)
predicate Lot(lot: Subject, size: Decimal[lb])
predicate Tag(item: Subject, label: Decimal, note: Subject)
transformation tag(item, label, note):
    admit Tag(item, label, note)
");
    let validated = program.validated().unwrap();
    apply(&pool, &validated).await.unwrap();
    assert!(act(&pool, &read, "tag", vec![subj("j"), dec(9), subj("n")]).await);

    // Safe: constant text.
    let price: (Option<String>, bool) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT retail_price::text, _morpholog_shape_matches FROM {SCHEMA}.price_history"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        price,
        (Some("3".to_string()), true),
        "the same structure decodes under today's name, whatever it meant then"
    );

    let lot: (Option<String>, Option<String>, bool, String) =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT lot, size::text, _morpholog_shape_matches,
                    _morpholog_arguments -> 1 -> 'value' ->> 'unit'
             FROM {SCHEMA}.lot_history"
        )))
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        lot,
        (Some("l".to_string()), None, false, "kg".to_string()),
        "an amount in another unit is not decoded, and the original stays"
    );

    let tags: Vec<TagRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT item, label::text, note, _morpholog_shape_matches,
                    _morpholog_arguments::text
             FROM {SCHEMA}.tag_history ORDER BY _morpholog_audit_order"
    )))
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(tags.len(), 2);
    assert_eq!(
        (&tags[0].0, &tags[0].1, &tags[0].2, tags[0].3),
        (&Some("i".to_string()), &None, &None, false),
        "the old row: its subject label is no decimal, and it has no note"
    );
    assert!(tags[0].4.contains("\"red\""), "{}", tags[0].4);
    // What the guard is for: the base view's cast, unguarded, fails the
    // whole read on that one row.
    let unguarded = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT (_morpholog_arguments -> 1 ->> 'value')::numeric FROM {SCHEMA}.tag_history"
    )))
    .fetch_all(&pool)
    .await
    .expect_err("a subject is no numeric");
    assert!(
        unguarded.to_string().contains("invalid input syntax"),
        "{unguarded}"
    );
    assert_eq!(
        (&tags[1].0, &tags[1].1, &tags[1].2, tags[1].3),
        (
            &Some("j".to_string()),
            &Some("9".to_string()),
            &Some("n".to_string()),
            true
        ),
        "the row today's programme wrote"
    );
}

/// Why an incremental reader needs the watermark. A writer takes its
/// place in the order and stays open; a later writer commits; the view
/// shows the later row alone. When the first commits, its row reads
/// BEFORE the one already shown. The watermark, read first, was at the
/// open order all along, so a reader paging below it had shown neither.
/// A read is consistent within its snapshot and promises nothing more,
/// for one entity as for the whole log.
#[tokio::test]
async fn a_writer_still_open_can_commit_below_a_row_already_shown() {
    let pool = test_pool().await;
    crate::testing::reset_database(&pool).await.unwrap();
    let (program, pg) = pg(STOCK);
    apply(&pool, &program.validated().unwrap()).await.unwrap();
    assert!(act(&pool, &pg, "receive", vec![subj("i1"), subj("p"), dec(1)]).await);

    let mut open = pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO morpholog.audit (
            transition_id, transformation_name, arguments, actor,
            invariant_epoch, invariants_checked,
            asserted_claims, retracted_claims, emitted_intents,
            attestation, parameters, model_hash, semantics_version, drawn_subjects,
            operations
         ) VALUES ($1, 'receive', '[]'::jsonb,
                   '{\"type\":\"subject\",\"value\":\"in_flight\"}'::jsonb,
                   1, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb, '[]'::jsonb,
                   '{\"mode\":\"gateway\",\"authenticated_by\":\"test\"}'::jsonb,
                   '[]'::jsonb, 'sha256:' || repeat('0', 64), 1, '[]'::jsonb,
                   '[{\"op\":\"ins\",\"predicate\":\"StockItem\",\"args\":[
                       {\"type\":\"subject\",\"value\":\"i1\"},
                       {\"type\":\"subject\",\"value\":\"q\"},
                       {\"type\":\"decimal\",\"value\":\"2\"}]}]'::jsonb)",
    )
    .bind(Uuid::now_v7())
    .execute(&mut *open)
    .await
    .unwrap();
    assert!(act(&pool, &pg, "receive", vec![subj("i1"), subj("r"), dec(3)]).await);

    let parts = async |pool: &PgPool| -> Vec<(String, i64)> {
        // Safe: constant text.
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT part, _morpholog_audit_order FROM {SCHEMA}.stock_item_history
             WHERE item = 'i1'
             ORDER BY _morpholog_audit_order, _morpholog_act_ordinal, _morpholog_operation"
        )))
        .fetch_all(pool)
        .await
        .unwrap()
    };
    let watermark = audit_resume_watermark(&pool).await.unwrap();
    let shown = parts(&pool).await;
    assert_eq!(
        shown.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
        ["p", "r"],
        "the open writer's row is not visible"
    );
    assert!(
        i64::try_from(watermark).unwrap() < shown[1].1,
        "the later row is above the watermark: {watermark} vs {shown:?}"
    );

    open.commit().await.unwrap();
    let after = parts(&pool).await;
    assert_eq!(
        after.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
        ["p", "q", "r"],
        "the first writer's row now reads before the one already shown"
    );
    assert!(after[1].1 < after[2].1);
}

const LINKS: &str = "program links
predicate Link(item: Subject, other: Subject)
    unique by (item)
transformation link(item, other):
    admit Link(item, other)
";

/// A lookup's index match finds every audit row whose operation holds the
/// subject in any field; the key is then decided by position. So `a`'s
/// history is the link keyed by `a`, not the one that merely points at it.
#[tokio::test]
async fn a_lookup_returns_the_key_s_rows_not_a_subject_s_in_another_field() {
    let pool = test_pool().await;
    crate::testing::reset_database(&pool).await.unwrap();
    let (program, pg) = pg(LINKS);
    let validated = program.validated().unwrap();
    let views = apply(&pool, &validated).await.unwrap();
    assert_eq!(views[0].lookups[0].function, "link_history_by_item");
    assert!(act(&pool, &pg, "link", vec![subj("a"), subj("b")]).await);
    assert!(act(&pool, &pg, "link", vec![subj("b"), subj("a")]).await);
    let items: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT item FROM {SCHEMA}.link_history_by_item('a')"
    )))
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(items, ["a"]);
}

const SPREAD: &str = "program spread
predicate Item(item: Subject, qty: Decimal)
    unique by (item)
predicate Note(item: Subject, text: Subject)
transformation stock(item, qty: Decimal):
    admit Item(item, qty)
transformation note(item, text):
    admit Note(item, text)
";

/// Every audit-row scan in a plan, with the rows it returned and whether
/// it went through the history index.
fn audit_scans(node: &serde_json::Value, out: &mut Vec<(String, u64)>) {
    if node["Relation Name"] == "audit" || node["Index Name"] == crate::HISTORY_INDEX {
        out.push((
            node["Node Type"].as_str().unwrap_or_default().to_string(),
            node["Actual Rows"].as_f64().unwrap_or_default() as u64,
        ));
    }
    if let Some(children) = node["Plans"].as_array() {
        for child in children {
            audit_scans(child, out);
        }
    }
}

/// With the history index, a view of a predicate few audit rows touch, and
/// a lookup of one item, read only the audit rows that mention them: the
/// index condition sits on the audit row, before its operations are
/// expanded. Sequential scans are turned off, as a large log would make
/// them lose; on a small one the planner rightly prefers them.
#[tokio::test]
async fn the_history_index_reaches_a_rare_predicate_and_one_entity() {
    let pool = test_pool().await;
    crate::testing::reset_database(&pool).await.unwrap();
    let (program, pg) = pg(SPREAD);
    let validated = program.validated().unwrap();
    apply(&pool, &validated).await.unwrap();
    for i in 0..40 {
        assert!(act(&pool, &pg, "stock", vec![subj(&format!("i{i}")), dec(i)]).await);
    }
    assert!(act(&pool, &pg, "note", vec![subj("i3"), subj("checked")]).await);
    crate::provision_history_index(&pool).await.unwrap();
    sqlx::raw_sql("ANALYZE morpholog.audit")
        .execute(&pool)
        .await
        .unwrap();

    // A lookup runs with its owner's rights, so PostgreSQL does not inline
    // it and a plan of the call shows a function scan only; its body, with
    // the key filled in, is what it runs.
    let body: String =
        sqlx::query_scalar("SELECT prosrc FROM pg_proc WHERE proname = 'item_history_by_item'")
            .fetch_one(&pool)
            .await
            .unwrap();
    for (what, query, candidates) in [
        (
            "the rare predicate's view",
            format!("SELECT * FROM {SCHEMA}.note_history"),
            1,
        ),
        ("one item's lookup", body.replace("$1", "'i3'::text"), 1),
    ] {
        let mut tx = pool.begin().await.unwrap();
        sqlx::raw_sql("SET LOCAL enable_seqscan = off")
            .execute(&mut *tx)
            .await
            .unwrap();
        let plan: serde_json::Value = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "EXPLAIN (ANALYZE, FORMAT JSON) {query}"
        )))
        .fetch_one(&mut *tx)
        .await
        .unwrap();
        tx.rollback().await.unwrap();
        let mut scans = Vec::new();
        audit_scans(&plan[0]["Plan"], &mut scans);
        assert!(
            scans.iter().any(|(node, _)| node == "Bitmap Index Scan"),
            "{what}: the index serves it: {plan}"
        );
        assert!(
            scans.iter().all(|(_, rows)| *rows <= candidates),
            "{what}: only the audit rows that mention it are read: {scans:?}"
        );
    }
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP INDEX morpholog.{}",
        crate::HISTORY_INDEX
    )))
    .execute(&pool)
    .await
    .unwrap();
}

/// A history view is read with its owner's rights, so a reader granted
/// only the view surface reads the log through it. Its lookup is the same:
/// a role with no access to Morpholog's own schema calls it once granted
/// EXECUTE, and is refused until then, as it is a view it was not granted.
#[tokio::test]
async fn a_reader_of_the_view_surface_reads_through_a_lookup_it_was_granted() {
    let pool = test_pool().await;
    let (is_super,): (bool,) =
        sqlx::query_as("SELECT rolsuper FROM pg_roles WHERE rolname = session_user")
            .fetch_one(&pool)
            .await
            .unwrap();
    if !is_super {
        eprintln!("skipped: creating a role needs a superuser session");
        return;
    }
    crate::testing::reset_database(&pool).await.unwrap();
    let (program, pg) = pg(LINKS);
    let validated = program.validated().unwrap();
    apply(&pool, &validated).await.unwrap();
    assert!(act(&pool, &pg, "link", vec![subj("a"), subj("b")]).await);
    // Roles belong to the cluster, and the gate runs this suite in several
    // databases at once, so each run makes a role of its own.
    let reader = format!("history_reader_{}", Uuid::now_v7().simple());
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE ROLE {reader} NOLOGIN;
         GRANT USAGE ON SCHEMA {SCHEMA} TO {reader};
         GRANT SELECT ON {SCHEMA}.link_history TO {reader};"
    )))
    .execute(&pool)
    .await
    .unwrap();
    let as_reader = async |sql: &str| -> Result<Vec<String>, sqlx::Error> {
        let mut tx = pool.begin().await.unwrap();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET LOCAL ROLE {reader}")))
            .execute(&mut *tx)
            .await
            .unwrap();
        let rows = sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
            .fetch_all(&mut *tx)
            .await;
        tx.rollback().await.unwrap();
        rows
    };
    let call = format!("SELECT item FROM {SCHEMA}.link_history_by_item('a')");
    assert_eq!(
        as_reader(&format!("SELECT item FROM {SCHEMA}.link_history"))
            .await
            .unwrap(),
        ["a"],
        "the view reads the log with its owner's rights"
    );
    let refused = as_reader(&call).await.unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("permission denied for function"),
        "not granted, the lookup is refused: {refused}"
    );
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "GRANT EXECUTE ON FUNCTION {SCHEMA}.link_history_by_item(text) TO {reader}"
    )))
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(as_reader(&call).await.unwrap(), ["a"]);
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP OWNED BY {reader}; DROP ROLE {reader}"
    )))
    .execute(&pool)
    .await
    .unwrap();
}
