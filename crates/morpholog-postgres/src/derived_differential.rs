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

/// InvenTree's order progress (#802): the lines that pass a comparison,
/// the lines whose own allocations, a sum under the sum, pass one, and
/// the lines a condition with an `or` holds for, through a call the sum
/// reads as a truth test. Each raises where a stored value is not a
/// decimal.
const ORDERS: &str = "program order_progress
predicate SalesOrder(order: Subject)
predicate SalesLine(line: Subject, order: Subject, part: Subject, quantity: Decimal)
predicate VirtualPart(part: Subject)
predicate LineShipped(line: Subject, shipped: Decimal)
predicate SalesAllocation(allocation: Subject, line: Subject, quantity: Decimal)
predicate OrderProgress(order: Subject, completed_lines: Decimal, overallocated_lines: Decimal, allocated_lines: Decimal)
define line_ready(l):
    SalesLine(l, _, p, q) and (VirtualPart(p) or (LineShipped(l, s) and q <= s) or q <= sum(a | SalesAllocation(_, l, a)))
derived OrderProgress(order):
    over SalesOrder(order: order)
    value completed_lines = sum(1 | SalesLine(l, order, _, q) and LineShipped(l, s) and q <= s)
    value overallocated_lines = sum(1 | SalesLine(l, order, _, q) and q < sum(a | SalesAllocation(_, l, a)))
    value allocated_lines = sum(1 | SalesLine(l, order, _, _) and line_ready(l))
transformation open_order(order):
    admit SalesOrder(order)
transformation add_line(line, order, part, quantity):
    admit SalesLine(line, order, part, quantity)
transformation mark_virtual(part):
    admit VirtualPart(part)
transformation ship(line, shipped):
    admit LineShipped(line, shipped)
transformation allocate(allocation, line, quantity):
    admit SalesAllocation(allocation, line, quantity)
";

/// InvenTree's reads of the rows that meet a condition (#787), with no
/// computed column: one row per distinct key tuple, a negation, and a
/// head that keeps fewer variables than its domain binds.
const MEMBERS: &str = "program members
predicate Alloc(alloc: Subject, item: Subject, quantity: Decimal)
predicate Shipped(alloc: Subject)
predicate Unshipped(alloc: Subject, item: Subject)
predicate ItemsAllocated(item: Subject)
derived Unshipped(alloc, item):
    over Alloc(alloc, item, _) and not Shipped(alloc)
derived ItemsAllocated(item):
    over Alloc(_, item, _)
transformation allocate(alloc, item, quantity):
    admit Alloc(alloc, item, quantity)
transformation ship(alloc):
    admit Shipped(alloc)
";

/// InvenTree's line flags and floors (#788): a choice of three, a flag,
/// a floor at zero, and a choice whose branches read different claims,
/// so only the chosen one can raise.
const CHOICES: &str = "program choices
predicate Line(line: Subject, quantity: Decimal)
predicate Alloc(alloc: Subject, line: Subject, quantity: Decimal)
predicate Note(line: Subject, amount: Decimal)
predicate Shortfall(line: Subject, quantity: Decimal)
predicate Progress(line: Subject, state: Subject)
predicate Full(line: Subject, full: Bool)
predicate Unallocated(line: Subject, quantity: Decimal)
predicate Guarded(line: Subject, quantity: Decimal)
derived Shortfall(line):
    over Line(line, _)
    value quantity = if(sum(q | Alloc(_, line, q)) >= sum(n | Line(line, n)), 0, sum(n | Line(line, n)) - sum(q | Alloc(_, line, q)))
derived Progress(line):
    over Line(line, _)
    value state = if(sum(q | Alloc(_, line, q)) >= sum(n | Line(line, n)), #full, if(sum(q | Alloc(_, line, q)) > 0, #partial, #none))
derived Full(line):
    over Line(line, _)
    value full = if(sum(q | Alloc(_, line, q)) >= sum(n | Line(line, n)), true, false)
derived Unallocated(line):
    over Line(line, _)
    value quantity = max(sum(n | Line(line, n)) - sum(q | Alloc(_, line, q)), 0)
derived Guarded(line):
    over Line(line, _)
    value quantity = if(sum(n | Line(line, n)) > 0, sum(n | Line(line, n)), sum(a | Note(line, a)))
transformation add(line, quantity):
    admit Line(line, quantity)
transformation allocate(alloc, line, quantity):
    admit Alloc(alloc, line, quantity)
transformation note(line, amount):
    admit Note(line, amount)
";

fn programs() -> Vec<Program> {
    let mut out: Vec<Program> = [SPIKE, EDGES, ORDERS, MEMBERS, CHOICES]
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
        false,
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
            let cells: Vec<String> = head
                .args
                .iter()
                .enumerate()
                .map(|(i, arg)| normalise(&arg.kind, row.try_get::<Option<String>, _>(i).unwrap()))
                .collect();
            (
                cells,
                row.try_get::<Option<String>, _>(head.args.len()).unwrap(),
            )
        })
        .collect()
}

/// A column's text as the kernel's row would print it: a decimal
/// normalised, by the declared kind, so a subject such as `001` is never
/// read as a number.
fn normalise(kind: &morpholog_core::PredicateArgKind, text: Option<String>) -> String {
    match (kind, text) {
        (_, None) => "NULL".to_string(),
        (morpholog_core::PredicateArgKind::Decimal, Some(t)) => t
            .parse::<rust_decimal::Decimal>()
            .map(|d| d.normalize().to_string())
            .unwrap_or(t),
        (_, Some(t)) => t,
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
                        EvalValue::Bool(b) => b.to_string(),
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
    let mut live: Vec<String> = Vec::new();
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
        if any {
            live.push(program.name.to_string());
        }
    }
    for fragment in ["edge_shapes", "members", "choices"] {
        assert!(
            live.iter().any(|n| n == fragment),
            "{fragment} renders live and is compared: {live:?}"
        );
    }
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
        claim_instance("Part", &[subj("001")]),
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

/// One order whose lines raise and one whose lines do not (#802). The
/// kernel's enumeration fails whole; the live view answers the clean
/// order exactly and names the failure on the other alone, with its
/// values NULL where they raise. A line two branches hold for counts
/// once, as the kernel counts a call.
#[tokio::test]
async fn a_raising_sum_body_fails_its_own_key_alone() {
    let pool = test_pool().await;
    let orders = morpholog_surface::parse_program(ORDERS).unwrap();
    let orders_v = orders.validated().unwrap();
    let clean = vec![
        claim_instance("SalesOrder", &[subj("a")]),
        // a1: shipped in full, and its part is virtual: two branches.
        claim_instance("SalesLine", &[subj("a1"), subj("a"), subj("v"), dec(5)]),
        // a2: shipped short, allocated short: no branch.
        claim_instance("SalesLine", &[subj("a2"), subj("a"), subj("p"), dec(3)]),
        // a3: allocated 5 against 4: the sum branch.
        claim_instance("SalesLine", &[subj("a3"), subj("a"), subj("p"), dec(4)]),
        claim_instance("VirtualPart", &[subj("v")]),
        claim_instance("LineShipped", &[subj("a1"), dec(5)]),
        claim_instance("LineShipped", &[subj("a2"), dec(1)]),
        claim_instance("SalesAllocation", &[subj("x1"), subj("a3"), dec(3)]),
        claim_instance("SalesAllocation", &[subj("x2"), subj("a3"), dec(2)]),
        claim_instance("SalesOrder", &[subj("b")]),
        claim_instance("SalesLine", &[subj("b1"), subj("b"), subj("p"), dec(2)]),
    ];
    assert_eq!(
        agree(
            &pool,
            &orders_v,
            &State::from_claims(clean.clone()),
            "orders clean"
        )
        .await,
        1
    );
    let rows = live_rows(&pool, &orders_v, "OrderProgress").await;
    let row = |order: &str| {
        rows.iter()
            .find(|(cells, _)| cells[0] == order)
            .cloned()
            .unwrap_or_else(|| panic!("a row for {order}: {rows:?}"))
    };
    assert_eq!(
        row("a"),
        (
            vec![
                "a".to_string(),
                "1".to_string(),
                "1".to_string(),
                "2".to_string()
            ],
            None
        ),
        "a1 shipped in full; a3 allocated 5 against 4; a1 and a3 ready, a1 once"
    );

    // b's shipment holds a subject where a quantity is compared.
    let mut dirty = clean;
    dirty.push(claim_instance("LineShipped", &[subj("b1"), subj("lots")]));
    let state = State::from_claims(dirty);
    assert!(orders_v.enumerate_derived("OrderProgress", &state).is_err());
    assert_eq!(agree(&pool, &orders_v, &state, "orders dirty").await, 1);
    let rows = live_rows(&pool, &orders_v, "OrderProgress").await;
    let row = |order: &str| {
        rows.iter()
            .find(|(cells, _)| cells[0] == order)
            .cloned()
            .unwrap_or_else(|| panic!("a row for {order}: {rows:?}"))
    };
    assert_eq!(
        row("a"),
        (
            vec![
                "a".to_string(),
                "1".to_string(),
                "1".to_string(),
                "2".to_string()
            ],
            None
        ),
        "the clean order keeps its values"
    );
    assert_eq!(
        row("b"),
        (
            vec![
                "b".to_string(),
                "NULL".to_string(),
                "0".to_string(),
                "NULL".to_string()
            ],
            Some("scope".to_string())
        ),
        "the values that compare b1's shipment are NULL, the other is computed, and the row names the failure"
    );
}

/// A choice evaluates its test, then the chosen branch alone: a branch
/// not chosen never raises, the chosen one does, and so does the test.
#[tokio::test]
async fn a_choice_raises_only_in_its_test_or_its_chosen_branch() {
    let pool = test_pool().await;
    let choices = morpholog_surface::parse_program(CHOICES).unwrap();
    let v = choices.validated().unwrap();

    // Allocated in full, in part and not at all; a note of another kind
    // the chosen branch never reads.
    let state = State::from_claims(vec![
        claim_instance("Line", &[subj("full"), dec(5)]),
        claim_instance("Line", &[subj("part"), dec(5)]),
        claim_instance("Line", &[subj("none"), dec(5)]),
        claim_instance("Alloc", &[subj("a1"), subj("full"), dec(5)]),
        claim_instance("Alloc", &[subj("a2"), subj("part"), dec(2)]),
        claim_instance("Note", &[subj("full"), subj("not_a_number")]),
    ]);
    assert_eq!(agree(&pool, &v, &state, "choices built").await, 5);
    let mut progress = live_rows(&pool, &v, "Progress").await;
    progress.sort();
    assert_eq!(
        progress,
        vec![
            (vec!["full".to_string(), "full".to_string()], None),
            (vec!["none".to_string(), "none".to_string()], None),
            (vec!["part".to_string(), "partial".to_string()], None),
        ]
    );
    let guarded = live_rows(&pool, &v, "Guarded").await;
    assert!(
        guarded.contains(&(vec!["full".to_string(), "5".to_string()], None)),
        "the branch not chosen reads the note and does not raise: {guarded:?}"
    );

    // The chosen branch reads the note of another kind.
    let state = State::from_claims(vec![
        claim_instance("Line", &[subj("empty"), dec(0)]),
        claim_instance("Note", &[subj("empty"), subj("not_a_number")]),
    ]);
    assert!(v.enumerate_derived("Guarded", &state).is_err());
    assert_eq!(agree(&pool, &v, &state, "choices chosen raises").await, 5);
    let rows = live_rows(&pool, &v, "Guarded").await;
    assert_eq!(rows[0].1.as_deref(), Some("sum_kind"), "{rows:?}");

    // The test reads a quantity of another kind.
    let state = State::from_claims(vec![claim_instance(
        "Line",
        &[subj("odd"), subj("not_a_number")],
    )]);
    assert!(v.enumerate_derived("Guarded", &state).is_err());
    assert_eq!(agree(&pool, &v, &state, "choices test raises").await, 5);
    let rows = live_rows(&pool, &v, "Guarded").await;
    assert_eq!(rows[0].1.as_deref(), Some("sum_kind"), "{rows:?}");
}
