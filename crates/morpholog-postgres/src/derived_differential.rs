//! Proves a live derived view computes the kernel's rows. For every
//! programme whose derived claims the renderer can all express, the view
//! is applied to a database holding a generated state, and its rows are
//! held to `enumerate_derived` over the same state: the same key tuples,
//! the same values, no error named; and where the kernel's enumeration
//! errors, a row names the class of failure. Built states reach what
//! generation seldom does: a key matched by several domain rows, an
//! empty domain, an empty sum, a sum over a value of another kind, a sum
//! past the decimal range. A governed history, reached through accepted
//! proposals, is compared after every commit, which is also the freshness
//! the cache cannot give. `DATABASE_URL`-gated.

use morpholog_core::{ClaimInstance, EvalValue, Program, State, Transition, ValidatedProgram};
use morpholog_test_support::differential::sample_state;
use morpholog_test_support::{claim_instance, dec, subj, test_actor};
use sqlx::{PgPool, Row};

use crate::attestation::Proposal;
use crate::compiled_differential::test_pool;
use crate::load_differential::seed;
use crate::program::PgProgram;
use crate::propose::propose_against_pg;
use crate::sql_views::{DerivedSource, render_views};

const SCHEMA: &str = "live_views";

/// The spike's three shapes and an act that admits and one that retracts.
const SPIKE: &str = "program spike_shapes
predicate Part(part: Subject)
predicate StockItem(item: Subject, part: Subject, quantity: Decimal)
predicate SalesLine(line: Subject, order: Subject, part: Subject, quantity: Decimal)
predicate OrderStatus(order: Subject, status: Subject)
predicate ShipmentShipped(shipment: Subject, shipped_at: Timestamp)
predicate SalesAllocation(allocation: Subject, line: Subject, shipment: Subject, item: Subject, quantity: Decimal)
predicate BuildAllocation(allocation: Subject, item: Subject, quantity: Decimal)
predicate TransferAllocation(allocation: Subject, order: Subject, item: Subject, quantity: Decimal)
predicate TransferOrderOpen(order: Subject)
predicate LineAllocated(line: Subject, quantity: Decimal)
predicate PartSalesAllocated(part: Subject, quantity: Decimal)
predicate Allocated(item: Subject, quantity: Decimal)
define order_open(order):
    OrderStatus(order, s) and s in (#pending, #on_hold, #in_progress, #shipped)
define line_counts(line, shipment):
    SalesLine(line, order, _, _) and order_open(order) and not ShipmentShipped(shipment, _)
define counted_allocation(allocation, item, quantity):
    (SalesAllocation(allocation, l, sh, item, quantity) and line_counts(l, sh)) or BuildAllocation(allocation, item, quantity) or (TransferAllocation(allocation, o, item, quantity) and TransferOrderOpen(o))
derived Allocated(item):
    over StockItem(item: item, ..)
    value quantity = sum(q | counted_allocation(a, item, q))
derived LineAllocated(line):
    over SalesLine(line: line, ..)
    value quantity = sum(q | SalesAllocation(_, line, _, _, q))
derived PartSalesAllocated(part):
    over Part(part: part)
    value quantity = sum(q | SalesAllocation(_, l, sh, i, q) and StockItem(i, part, _) and line_counts(l, sh))
transformation allocate(allocation, line, shipment, item, quantity):
    admit SalesAllocation(allocation, line, shipment, item, quantity)
transformation deallocate(allocation):
    retract SalesAllocation(allocation: allocation, ..)
transformation ship(shipment):
    admit ShipmentShipped(shipment, decided_at)
transformation build_allocate(allocation, item, quantity):
    admit BuildAllocation(allocation, item, quantity)
";

/// Edge shapes: a key the domain matches several times, two values, a
/// value computed by arithmetic over two sums.
const EDGES: &str = "program edge_shapes
predicate Entry(account: Subject, side: Subject, amount: Decimal)
predicate Balance(account: Subject, debits: Decimal, net: Decimal)
derived Balance(account):
    over Entry(account: account, ..)
    value debits = sum(a | Entry(account, #debit, a))
    value net = sum(a | Entry(account, #debit, a)) - sum(a | Entry(account, #credit, a))
transformation post(account, side, amount):
    admit Entry(account, side, amount)
";

fn programs() -> Vec<Program> {
    let mut out: Vec<Program> = [SPIKE, EDGES]
        .iter()
        .map(|s| morpholog_surface::parse_program(s).expect("fragment parses"))
        .collect();
    out.extend(morpholog_examples::all_programs());
    out
}

async fn apply(pool: &PgPool, validated: &ValidatedProgram<'_>) -> Option<Vec<String>> {
    let rendered = render_views(
        validated,
        SCHEMA,
        &format!("sha256:{}", "0".repeat(64)),
        DerivedSource::Live,
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
            .iter()
            .filter(|v| v.kind == "derived")
            .map(|v| v.predicate.clone())
            .collect(),
    )
}

/// A row of a live view as the kernel would print it: each key and value
/// as text, decimals normalised, plus the error class.
async fn live_rows(
    pool: &PgPool,
    validated: &ValidatedProgram<'_>,
    predicate: &str,
) -> Vec<(Vec<String>, Option<String>)> {
    let head = validated
        .as_program()
        .predicates
        .iter()
        .find(|p| p.name.as_str() == predicate)
        .unwrap();
    let view = crate::sql_views::view_name(predicate);
    let columns: Vec<String> = head
        .args
        .iter()
        .map(|a| format!("{}::text", crate::sql_quote::quote_ident(&a.name)))
        .collect();
    // Safe: identifiers are quoted, the schema is this test's constant.
    let sql = format!(
        "SELECT {}, _morpholog_error FROM {SCHEMA}.{}",
        columns.join(", "),
        crate::sql_quote::quote_ident(&view)
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
        .unwrap();
    rows.iter()
        .map(|row| {
            let cells: Vec<String> = (0..head.args.len())
                .map(|i| normalise(row.try_get::<Option<String>, _>(i).unwrap()))
                .collect();
            (
                cells,
                row.try_get::<Option<String>, _>(head.args.len()).unwrap(),
            )
        })
        .collect()
}

fn normalise(text: Option<String>) -> String {
    match text {
        None => "NULL".to_string(),
        Some(t) => match t.parse::<rust_decimal::Decimal>() {
            Ok(d) => d.normalize().to_string(),
            Err(_) => t,
        },
    }
}

fn kernel_rows(rows: &[ClaimInstance]) -> Vec<(Vec<String>, Option<String>)> {
    rows.iter()
        .map(|c| {
            (
                c.args
                    .iter()
                    .map(|v| match v {
                        EvalValue::Subject(s) => s.to_string(),
                        EvalValue::Decimal(d) => d.normalize().to_string(),
                        other => format!("{other:?}"),
                    })
                    .collect(),
                None,
            )
        })
        .collect()
}

/// Hold every live view of `validated` to the kernel over `state`.
async fn agree(
    pool: &PgPool,
    validated: &ValidatedProgram<'_>,
    state: &State,
    label: &str,
) -> usize {
    let Some(derived) = apply(pool, validated).await else {
        return 0;
    };
    crate::testing::reset_database(pool).await.unwrap();
    let mut claims = state.claims().to_vec();
    claims.sort_by_key(|c| format!("{c:?}"));
    claims.dedup();
    seed(pool, &claims).await;
    for predicate in &derived {
        let mut live = live_rows(pool, validated, predicate).await;
        live.sort();
        match validated.enumerate_derived(predicate, state) {
            Ok(None) => panic!("{label}: nothing derives {predicate}"),
            Ok(Some(rows)) => {
                let mut kernel = kernel_rows(&rows);
                kernel.sort();
                assert_eq!(
                    live, kernel,
                    "{label}: {predicate}: the live view and the kernel disagree"
                );
            }
            Err(e) => {
                assert!(
                    live.iter().any(|(_, error)| error.is_some()),
                    "{label}: {predicate}: the kernel errors ({e}) and no live row names a failure: {live:?}"
                );
            }
        }
    }
    derived.len()
}

#[tokio::test]
async fn live_views_compute_the_kernels_rows_over_generated_states() {
    let pool = test_pool().await;
    let mut compared = 0usize;
    let mut live_programs = 0usize;
    for program in programs() {
        let validated = program.validated().unwrap();
        if validated.as_program().derived_claims.is_empty() {
            continue;
        }
        let mut any = false;
        for salt in 0..3u64 {
            let state = sample_state(&program, 2, salt);
            let n = agree(
                &pool,
                &validated,
                &state,
                &format!("{} salt {salt}", program.name),
            )
            .await;
            any |= n > 0;
            compared += n;
        }
        live_programs += usize::from(any);
    }
    assert!(
        live_programs >= 2,
        "the two fragments at least render live: {live_programs}"
    );
    assert!(compared >= 12, "comparisons: {compared}");
}

/// Built states the generator seldom reaches, each compared on both
/// routes: a key matched by several domain rows and an empty sum, the
/// edge programme's two values and arithmetic over two sums, a sum over a
/// value of another kind, and a sum past the decimal range, which the
/// kernel refuses to compute and the view names.
#[tokio::test]
async fn live_views_agree_on_built_edge_states() {
    let pool = test_pool().await;
    let spike = morpholog_surface::parse_program(SPIKE).unwrap();
    let spike_v = spike.validated().unwrap();
    // Two allocations on one line, one on a line of a cancelled order, a
    // part with no stock: a key matched by several domain rows and an
    // empty sum.
    let state = State::from_claims(vec![
        claim_instance("Part", &[subj("w")]),
        claim_instance("Part", &[subj("bare")]),
        claim_instance("StockItem", &[subj("i1"), subj("w"), dec(10)]),
        claim_instance("SalesLine", &[subj("l1"), subj("o1"), subj("w"), dec(5)]),
        claim_instance("SalesLine", &[subj("l2"), subj("o2"), subj("w"), dec(5)]),
        claim_instance("OrderStatus", &[subj("o1"), subj("pending")]),
        claim_instance("OrderStatus", &[subj("o2"), subj("cancelled")]),
        claim_instance(
            "SalesAllocation",
            &[subj("a1"), subj("l1"), subj("s1"), subj("i1"), dec(2)],
        ),
        claim_instance(
            "SalesAllocation",
            &[
                subj("a2"),
                subj("l1"),
                subj("s1"),
                subj("i1"),
                morpholog_test_support::dec_str("1.5"),
            ],
        ),
        claim_instance(
            "SalesAllocation",
            &[subj("a3"), subj("l2"), subj("s2"), subj("i1"), dec(7)],
        ),
        // One allocation id in two families with one quantity: one
        // projection, counted once by the kernel and by the view; a
        // transfer allocation on a closed order, not counted.
        claim_instance("BuildAllocation", &[subj("a1"), subj("i1"), dec(2)]),
        claim_instance("BuildAllocation", &[subj("b1"), subj("i1"), dec(4)]),
        claim_instance(
            "TransferAllocation",
            &[subj("t1"), subj("to1"), subj("i1"), dec(8)],
        ),
        claim_instance(
            "TransferAllocation",
            &[subj("t2"), subj("to2"), subj("i1"), dec(16)],
        ),
        claim_instance("TransferOrderOpen", &[subj("to1")]),
    ]);
    assert_eq!(agree(&pool, &spike_v, &state, "spike built").await, 3);
    let allocated = live_rows(&pool, &spike_v, "Allocated").await;
    assert_eq!(
        allocated,
        vec![(vec!["i1".to_string(), "15.5".to_string()], None)],
        "2 + 1.5 counted once with the build row sharing a1, + 4 + 8"
    );

    let edges = morpholog_surface::parse_program(EDGES).unwrap();
    let edges_v = edges.validated().unwrap();
    let state = State::from_claims(vec![
        claim_instance("Entry", &[subj("cash"), subj("debit"), dec(100)]),
        claim_instance("Entry", &[subj("cash"), subj("debit"), dec(50)]),
        claim_instance("Entry", &[subj("cash"), subj("credit"), dec(30)]),
        claim_instance("Entry", &[subj("rent"), subj("credit"), dec(30)]),
    ]);
    assert_eq!(agree(&pool, &edges_v, &state, "edges built").await, 1);

    // A value of another kind where a decimal is summed: the kernel's
    // error, and the view's `sum_kind`.
    let state = State::from_claims(vec![claim_instance(
        "Entry",
        &[subj("cash"), subj("debit"), subj("not_a_number")],
    )]);
    assert!(edges_v.enumerate_derived("Balance", &state).is_err());
    assert_eq!(agree(&pool, &edges_v, &state, "edges kind").await, 1);
    let rows = live_rows(&pool, &edges_v, "Balance").await;
    assert_eq!(rows[0].1.as_deref(), Some("sum_kind"), "{rows:?}");

    // A sum past the decimal range: the kernel's error, and the view's
    // `sum_range`.
    let huge = morpholog_test_support::dec_str("79228162514264337593543950335");
    let state = State::from_claims(vec![
        claim_instance("Entry", &[subj("cash"), subj("debit"), huge.clone()]),
        claim_instance("Entry", &[subj("cash"), subj("debit"), dec(1)]),
    ]);
    assert!(edges_v.enumerate_derived("Balance", &state).is_err());
    assert_eq!(agree(&pool, &edges_v, &state, "edges range").await, 1);
    let rows = live_rows(&pool, &edges_v, "Balance").await;
    assert_eq!(rows[0].1.as_deref(), Some("sum_range"), "{rows:?}");
}

/// A governed history: the spike fragment's acts committed one by one
/// through the adapter, the live views compared with the kernel after
/// every commit, with no refresh between. The figures move with each
/// allocation, retraction and shipment.
#[tokio::test]
async fn live_views_follow_a_governed_history_commit_by_commit() {
    let pool = test_pool().await;
    let program = morpholog_surface::parse_program(SPIKE).unwrap();
    let validated = program.validated().unwrap();
    let derived = apply(&pool, &validated)
        .await
        .expect("the spike shapes render live");
    assert_eq!(
        derived.len(),
        3,
        "all three of the spike's derived claims render live"
    );
    crate::testing::reset_database(&pool).await.unwrap();
    seed(
        &pool,
        &[
            claim_instance("Part", &[subj("w")]),
            claim_instance("StockItem", &[subj("i1"), subj("w"), dec(10)]),
            claim_instance("SalesLine", &[subj("l1"), subj("o1"), subj("w"), dec(5)]),
            claim_instance("OrderStatus", &[subj("o1"), subj("pending")]),
        ],
    )
    .await;
    let pg = PgProgram::new(morpholog_core::PreparedProgram::new(program.clone()).unwrap());
    let acts: Vec<(&str, Vec<EvalValue>)> = vec![
        (
            "allocate",
            vec![subj("a1"), subj("l1"), subj("s1"), subj("i1"), dec(2)],
        ),
        (
            "allocate",
            vec![subj("a2"), subj("l1"), subj("s1"), subj("i1"), dec(3)],
        ),
        ("build_allocate", vec![subj("b1"), subj("i1"), dec(4)]),
        ("deallocate", vec![subj("a1")]),
        ("ship", vec![subj("s1")]),
    ];
    let mut seen_values: Vec<String> = Vec::new();
    for (name, args) in acts {
        let transition = Transition {
            transformation_name: name.into(),
            args,
            actor: test_actor(),
            decided_at: None,
        };
        let outcome = propose_against_pg(&pool, &pg, &Proposal::gateway(&transition))
            .await
            .unwrap();
        assert!(
            matches!(outcome, crate::PgProposalOutcome::Committed { .. }),
            "{name}: {outcome:?}"
        );
        let state = State::from_claims(
            crate::claims::list_claims_for_predicates(
                &pool,
                &validated
                    .as_program()
                    .predicates
                    .iter()
                    .map(|p| p.name.to_string())
                    .collect::<Vec<_>>(),
            )
            .await
            .unwrap(),
        );
        for predicate in &derived {
            let mut live = live_rows(&pool, &validated, predicate).await;
            live.sort();
            let mut kernel = kernel_rows(
                &validated
                    .enumerate_derived(predicate, &state)
                    .unwrap()
                    .unwrap(),
            );
            kernel.sort();
            assert_eq!(live, kernel, "after {name}: {predicate}");
        }
        let part = live_rows(&pool, &validated, "PartSalesAllocated").await;
        seen_values.push(part[0].0[1].clone());
    }
    assert_eq!(
        seen_values,
        ["2", "5", "5", "3", "0"],
        "the figure moved with every commit, with no refresh"
    );
}
