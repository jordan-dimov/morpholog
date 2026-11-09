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

/// Apply `program`'s views with their history views, and return the
/// history views' descriptions; `None` where the programme's names
/// cannot be rendered at all.
async fn apply(pool: &PgPool, validated: &ValidatedProgram<'_>) -> Option<Vec<ViewDescription>> {
    let rendered = render_views(
        validated,
        SCHEMA,
        &format!("sha256:{}", "0".repeat(64)),
        DerivedSource::Cache,
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
        compared += rows.len();
    }
    (compared, deletes)
}

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

    let tags: Vec<(Option<String>, Option<String>, Option<String>, bool, String)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
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
