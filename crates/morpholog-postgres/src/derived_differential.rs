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
use morpholog_test_support::{claim_instance, coll, dec, subj, test_actor};
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

/// InvenTree's roles (#874): a domain that calls a condition whose body
/// is an `or`, read with every key the call binds and with fewer.
const ROLES: &str = "program roles
predicate UserAccount(user: Subject, active: Bool, superuser: Bool)
predicate Member(user: Subject, group: Subject)
predicate RoleGrant(group: Subject, role: Subject, perm: Subject)
predicate RolePerm(role: Subject, perm: Subject)
predicate UserRole(user: Subject, role: Subject, perm: Subject)
predicate RoleHolder(user: Subject)
define has_role(user, role, perm):
    (UserAccount(user, true, true) and RolePerm(role, perm)) or (Member(user, g) and RoleGrant(g, role, perm))
derived UserRole(user, role, perm):
    over has_role(user, role, perm)
derived RoleHolder(user):
    over has_role(user, role, perm)
transformation join(user, group):
    admit Member(user, group)
";

/// InvenTree's highest serial (#878): a max guarded by a count, a min
/// left bare so an empty body raises, and a max through a join.
const EXTREMA: &str = "program extrema
predicate Part(part: Subject)
predicate VariantOf(variant: Subject, part: Subject)
predicate Serial(part: Subject, n: Decimal)
predicate Highest(part: Subject, highest: Decimal)
predicate Lowest(part: Subject, lowest: Decimal)
predicate TreeHighest(part: Subject, highest: Decimal)
predicate Lot(lot: Subject, part: Subject, n: Decimal)
predicate LotFloor(lot: Subject, floor: Decimal)
predicate LotHighest(part: Subject, highest: Decimal)
derived Highest(part):
    over Part(part: part)
    value highest = if(sum(1 | Serial(part, n)) > 0, max(n | Serial(part, n)), 0)
derived Lowest(part):
    over Part(part: part)
    value lowest = min(n | Serial(part, n))
derived TreeHighest(part):
    over Part(part: part)
    value highest = if(sum(1 | VariantOf(v, part) and Serial(v, n)) > 0, max(n | VariantOf(v, part) and Serial(v, n)), 0)
derived LotHighest(part):
    over Part(part: part)
    value highest = max(n | Lot(l, part, n) and LotFloor(l, f) and n > f)
transformation add(part):
    admit Part(part)
transformation serial(part, n):
    admit Serial(part, n)
";

/// InvenTree's order total (#886): a sum whose target is computed from
/// each line, a division that does not terminate, and a body that can
/// raise beside a target that can.
const TOTALS: &str = "program totals
predicate Order(order: Subject)
predicate SalesLine(line: Subject, order: Subject, part: Subject, quantity: Decimal)
predicate LinePrice(line: Subject, price: Decimal, currency: Subject, discount: Decimal)
predicate LineGuard(line: Subject, floor: Decimal)
predicate OrderTotal(order: Subject, total: Decimal)
predicate OrderThird(order: Subject, third: Decimal)
predicate GuardedRatio(order: Subject, ratio: Decimal)
predicate OrderNet(order: Subject, net: Decimal)
define net(unit_price, qty, discount) = (qty * unit_price * (1 - discount / 100))
derived OrderTotal(order):
    over Order(order: order)
    value total = sum(q * price * (1 - d / 100) | SalesLine(l, order, _, q) and LinePrice(l, price, _, d))
derived OrderThird(order):
    over Order(order: order)
    value third = sum(price / 3 | SalesLine(l, order, _, q) and LinePrice(l, price, _, d))
derived GuardedRatio(order):
    over Order(order: order)
    value ratio = sum(q / d | SalesLine(l, order, _, q) and LinePrice(l, _, _, d) and LineGuard(l, f) and q > f)
derived OrderNet(order):
    over Order(order: order)
    value net = sum(net(price, q, d) | SalesLine(l, order, _, q) and LinePrice(l, price, _, d))
transformation open(order):
    admit Order(order)
transformation line(line, order, part, quantity: Decimal):
    admit SalesLine(line, order, part, quantity)
transformation price(line, price: Decimal, currency, discount: Decimal):
    admit LinePrice(line, price, currency, discount)
transformation guard(line, floor: Decimal):
    admit LineGuard(line, floor)
";

/// CTENEX's order status and InvenTree's line figures (#895): views a
/// host reads one key at a time, and a decimal key that two claims spell
/// with different scales.
const KEYED: &str = "program keyed
predicate Order(order: Subject, qty: Decimal)
predicate Fill(fill: Subject, order: Subject, qty: Decimal)
predicate Filled(order: Subject, filled: Decimal)
predicate Offer(level: Decimal, qty: Decimal)
predicate AtLevel(level: Decimal, qty: Decimal)
derived Filled(order):
    over Order(order, _)
    value filled = sum(q | Fill(_, order, q))
derived AtLevel(level):
    over Offer(level, _)
    value qty = sum(q | Offer(level, q))
transformation place(order, qty: Decimal):
    admit Order(order, qty)
transformation fill(fill, order, qty: Decimal):
    admit Fill(fill, order, qty)
";

/// Keys whose column can spell one value key two ways: a key of any kind,
/// and a collection of decimals, whose columns are raw JSON; a subject key
/// read from a position of any kind; one read through
/// a condition whose first branch binds a subject and whose second any
/// kind, with a parameter that is no key; and a subject key over a
/// position declared a subject, holding decimals admitted before
/// admission held each claim to its kinds. Their columns are not the
/// kernel's text, so rows are counted, not compared.
const ANY_KEYED: &str = "program any_keyed
predicate Tag(thing: Any)
predicate Basket(items: Collection)
predicate Baskets(items: Collection)
predicate Source(thing: Subject)
predicate FromSource(thing: Subject)
predicate Label(thing: Subject, by: Subject)
predicate Mark(thing: Any, by: Subject)
predicate Tagged(thing: Any)
predicate Named(thing: Subject)
predicate Marked(thing: Subject)
define marked(x, by):
    Label(x, by) or Mark(x, by)
derived Tagged(thing):
    over Tag(thing)
derived Named(thing):
    over Tag(thing)
derived Marked(thing):
    over marked(thing, by)
derived FromSource(thing):
    over Source(thing)
derived Baskets(items):
    over Basket(items)
";

fn programs() -> Vec<Program> {
    let mut out: Vec<Program> = [
        SPIKE, EDGES, ORDERS, MEMBERS, CHOICES, ROLES, EXTREMA, TOTALS, KEYED,
    ]
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
        &Default::default(),
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
    // A generated state can hold one claim twice; admitted state is a set,
    // so the kernel reads the claims the database holds.
    let state = &State::from_claims(claims);
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
    for fragment in [
        "edge_shapes",
        "members",
        "choices",
        "roles",
        "extrema",
        "totals",
    ] {
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

/// A user holds a grant through either branch of the condition, and one
/// through both: the view keeps each (user, role, perm) once, as the
/// kernel does, and an inactive superuser or a group with no grant holds
/// nothing.
#[tokio::test]
async fn a_domain_over_an_or_condition_keeps_each_key_once() {
    let pool = test_pool().await;
    let program = morpholog_surface::parse_program(ROLES).unwrap();
    let validated = program.validated().unwrap();
    let t = morpholog_test_support::bool_;
    let state = State::from_claims(vec![
        claim_instance("UserAccount", &[subj("admin"), t(true), t(true)]),
        claim_instance("UserAccount", &[subj("retired"), t(false), t(true)]),
        claim_instance("UserAccount", &[subj("both"), t(true), t(true)]),
        claim_instance("RolePerm", &[subj("part"), subj("view")]),
        claim_instance("RolePerm", &[subj("part"), subj("change")]),
        claim_instance("Member", &[subj("clerk"), subj("staff")]),
        claim_instance("Member", &[subj("both"), subj("staff")]),
        claim_instance("Member", &[subj("idle"), subj("empty")]),
        claim_instance("RoleGrant", &[subj("staff"), subj("part"), subj("view")]),
    ]);
    let rows = validated
        .enumerate_derived("UserRole", &state)
        .unwrap()
        .unwrap();
    assert_eq!(
        rows.len(),
        5,
        "admin twice, both twice, clerk once: {rows:?}"
    );
    let holders = validated
        .enumerate_derived("RoleHolder", &state)
        .unwrap()
        .unwrap();
    assert_eq!(holders.len(), 3, "{holders:?}");
    assert_eq!(agree(&pool, &validated, &state, "roles built").await, 2);
}

/// The extremum of each part's serials, as the kernel computes it: a tie
/// counted once, a join through a variant, an empty body guarded to zero.
/// Unguarded, an empty body raises in the kernel and the view names
/// `extremum_empty`; a value of another kind among the serials raises and
/// the view names `extremum_kind`.
#[tokio::test]
async fn an_extremum_is_the_kernels_or_names_why_not() {
    let pool = test_pool().await;
    let program = morpholog_surface::parse_program(EXTREMA).unwrap();
    let validated = program.validated().unwrap();
    let state = State::from_claims(vec![
        claim_instance("Part", &[subj("w")]),
        claim_instance("Part", &[subj("tree")]),
        claim_instance("Serial", &[subj("w"), dec(7)]),
        claim_instance("Serial", &[subj("w"), dec(3)]),
        claim_instance(
            "Serial",
            &[subj("w"), morpholog_test_support::dec_str("7.0")],
        ),
        claim_instance("VariantOf", &[subj("w"), subj("tree")]),
        claim_instance("Serial", &[subj("tree"), dec(2)]),
    ]);
    assert_eq!(agree(&pool, &validated, &state, "extrema built").await, 4);
    let row = |rows: &[(Vec<String>, Option<String>)], key: &str| {
        rows.iter()
            .find(|(cells, _)| cells[0] == key)
            .cloned()
            .unwrap()
    };
    let highest = live_rows(&pool, &validated, "Highest").await;
    assert_eq!(
        row(&highest, "w"),
        (vec!["w".to_string(), "7".to_string()], None)
    );
    let tree = live_rows(&pool, &validated, "TreeHighest").await;
    assert_eq!(
        row(&tree, "tree"),
        (vec!["tree".to_string(), "7".to_string()], None)
    );

    // A part with no serial: zero where guarded, `extremum_empty` where not.
    let state = State::from_claims(vec![claim_instance("Part", &[subj("bare")])]);
    assert!(validated.enumerate_derived("Lowest", &state).is_err());
    assert_eq!(agree(&pool, &validated, &state, "extrema empty").await, 4);
    let lowest = live_rows(&pool, &validated, "Lowest").await;
    assert_eq!(lowest[0].1.as_deref(), Some("extremum_empty"), "{lowest:?}");
    let highest = live_rows(&pool, &validated, "Highest").await;
    assert_eq!(
        highest[0],
        (vec!["bare".to_string(), "0".to_string()], None)
    );

    // A serial of another kind beside a decimal: the kernel cannot order
    // them, and the view says so.
    let state = State::from_claims(vec![
        claim_instance("Part", &[subj("odd")]),
        claim_instance("Serial", &[subj("odd"), dec(4)]),
        claim_instance("Serial", &[subj("odd"), subj("not_a_number")]),
    ]);
    assert!(validated.enumerate_derived("Lowest", &state).is_err());
    assert_eq!(agree(&pool, &validated, &state, "extrema kind").await, 4);
    let lowest = live_rows(&pool, &validated, "Lowest").await;
    assert_eq!(lowest[0].1.as_deref(), Some("extremum_kind"), "{lowest:?}");
}

/// Two lots of one part whose figures are equal but spelled `7` and
/// `7.0`. The kernel keeps the spelling it matches first, so the state's
/// order picks it, and PostgreSQL's `max` keeps the one it scans last.
/// Scale is representation, not value: the view promises the kernel's
/// value, written as one of the tied figures.
#[tokio::test]
async fn a_tie_between_spellings_gives_the_kernels_value() {
    let pool = test_pool().await;
    let program = morpholog_surface::parse_program(EXTREMA).unwrap();
    let validated = program.validated().unwrap();
    let ordered = vec![
        claim_instance("Part", &[subj("w")]),
        claim_instance("Lot", &[subj("l1"), subj("w"), dec(7)]),
        claim_instance(
            "Lot",
            &[
                subj("l2"),
                subj("w"),
                morpholog_test_support::dec_str("7.0"),
            ],
        ),
        claim_instance("LotFloor", &[subj("l1"), dec(0)]),
        claim_instance("LotFloor", &[subj("l2"), dec(0)]),
    ];
    let mut reversed = ordered.clone();
    reversed.swap(1, 2);
    for (label, claims) in [("ordered", ordered), ("reversed", reversed)] {
        let state = State::from_claims(claims);
        assert_eq!(agree(&pool, &validated, &state, label).await, 4);
        let kernel = validated
            .enumerate_derived("LotHighest", &state)
            .unwrap()
            .unwrap();
        let EvalValue::Decimal(kernel) = kernel[0].args[1] else {
            panic!("{label}: {kernel:?}");
        };
        // Safe: the schema name is this test's constant.
        let live: String = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT highest::text FROM {SCHEMA}.lot_highest"
        )))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(["7", "7.0"].contains(&live.as_str()), "{label}: {live}");
        assert_eq!(
            live.parse::<rust_decimal::Decimal>().unwrap(),
            kernel,
            "{label}"
        );
    }
}

/// A lot whose floor holds a value of another kind: the kernel cannot
/// compare it, so it raises for the part even though another lot clears
/// its floor. The view names the failure, `scope` as in a sum's body,
/// instead of giving that lot's figure as the maximum.
#[tokio::test]
async fn a_raising_body_fails_the_extremum_rather_than_skipping_a_candidate() {
    let pool = test_pool().await;
    let program = morpholog_surface::parse_program(EXTREMA).unwrap();
    let validated = program.validated().unwrap();
    let state = State::from_claims(vec![
        claim_instance("Part", &[subj("p")]),
        claim_instance("Lot", &[subj("l1"), subj("p"), dec(9)]),
        claim_instance("LotFloor", &[subj("l1"), dec(1)]),
        claim_instance("Lot", &[subj("l2"), subj("p"), dec(5)]),
        claim_instance("LotFloor", &[subj("l2"), subj("not_a_number")]),
    ]);
    assert!(validated.enumerate_derived("LotHighest", &state).is_err());
    assert_eq!(
        agree(&pool, &validated, &state, "extrema raising body").await,
        4
    );
    let rows = live_rows(&pool, &validated, "LotHighest").await;
    assert_eq!(
        rows,
        vec![(
            vec!["p".to_string(), "NULL".to_string()],
            Some("scope".to_string())
        )]
    );
}

/// One order's lines, priced, with a guard each.
fn order_lines(
    order: &str,
    lines: &[(&str, EvalValue, EvalValue, EvalValue, EvalValue)],
) -> Vec<ClaimInstance> {
    let mut claims = vec![claim_instance("Order", &[subj(order)])];
    for (line, q, price, d, floor) in lines {
        claims.push(claim_instance(
            "SalesLine",
            &[subj(line), subj(order), subj("part"), q.clone()],
        ));
        claims.push(claim_instance(
            "LinePrice",
            &[subj(line), price.clone(), subj("usd"), d.clone()],
        ));
        claims.push(claim_instance("LineGuard", &[subj(line), floor.clone()]));
    }
    claims
}

/// The live view's own text for one column of one key.
async fn raw(
    pool: &PgPool,
    view: &str,
    column: &str,
    order: &str,
) -> (Option<String>, Option<String>) {
    // Safe: identifiers are this test's constants.
    let row = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {column}::text, _morpholog_error FROM {SCHEMA}.{view} WHERE \"order\" = $1"
    )))
    .bind(order)
    .fetch_one(pool)
    .await
    .unwrap();
    (row.try_get(0).unwrap(), row.try_get(1).unwrap())
}

/// An order total computed per line is the kernel's to the last digit,
/// a non-terminating division rounded per row as the kernel rounds it.
/// The kernel normalises a sum's total, so the view's text is compared
/// with its trailing zeros trimmed: that drops no significant digit.
#[tokio::test]
async fn a_computed_target_is_the_kernels_figure() {
    let pool = test_pool().await;
    let program = morpholog_surface::parse_program(TOTALS).unwrap();
    let validated = program.validated().unwrap();
    let state = State::from_claims(order_lines(
        "o1",
        &[
            (
                "l1",
                dec(3),
                dec(10),
                morpholog_test_support::dec_str("12.5"),
                dec(0),
            ),
            ("l2", dec(2), dec(7), dec(5), dec(0)),
        ],
    ));
    assert_eq!(agree(&pool, &validated, &state, "totals built").await, 4);
    for (view, column, predicate, expected) in [
        ("order_total", "total", "OrderTotal", "39.55"),
        (
            "order_third",
            "third",
            "OrderThird",
            "5.6666666666666666666666666666",
        ),
        ("guarded_ratio", "ratio", "GuardedRatio", "0.64"),
        ("order_net", "net", "OrderNet", "39.55"),
    ] {
        let rows = validated
            .enumerate_derived(predicate, &state)
            .unwrap()
            .unwrap();
        let EvalValue::Decimal(kernel) = &rows[0].args[1] else {
            panic!("{predicate}: {rows:?}");
        };
        assert_eq!(kernel.to_string(), expected, "{predicate}");
        let (text, error) = raw(&pool, view, column, "o1").await;
        assert_eq!(error, None, "{predicate}");
        let live: rust_decimal::Decimal = text.unwrap().parse().unwrap();
        assert_eq!(live.normalize().to_string(), expected, "{predicate}");
    }
}

/// Where the kernel cannot compute a row's target, the view names
/// `value` for that order alone: a zero divisor, a product no decimal
/// holds, an operand of another kind. A total past the range from rows
/// that are each in range is `sum_range`. A body that raises on one line
/// outranks a target that raises on another, as the kernel finds the
/// body's matches before it computes a target: `scope`.
#[tokio::test]
async fn a_computed_target_that_raises_names_why() {
    let pool = test_pool().await;
    let program = morpholog_surface::parse_program(TOTALS).unwrap();
    let validated = program.validated().unwrap();
    let huge = morpholog_test_support::dec_str("50000000000000000000000000000");
    let max = morpholog_test_support::dec_str("79228162514264337593543950335");
    let cases: Vec<(&str, Vec<ClaimInstance>, &str, &str, &str)> = vec![
        (
            "a zero divisor",
            order_lines("o", &[("l1", dec(3), dec(10), dec(0), dec(0))]),
            "guarded_ratio",
            "ratio",
            "value",
        ),
        (
            "a product no decimal holds",
            order_lines("o", &[("l1", max.clone(), dec(2), dec(0), dec(0))]),
            "order_total",
            "total",
            "value",
        ),
        (
            "an operand of another kind",
            order_lines("o", &[("l1", dec(3), subj("ten"), dec(0), dec(0))]),
            "order_total",
            "total",
            "value",
        ),
        (
            "a product no decimal holds, through a definition",
            order_lines("o", &[("l1", max.clone(), dec(2), dec(0), dec(0))]),
            "order_net",
            "net",
            "value",
        ),
        (
            "an operand of another kind, through a definition",
            order_lines("o", &[("l1", dec(3), subj("ten"), dec(0), dec(0))]),
            "order_net",
            "net",
            "value",
        ),
        (
            "a total past the range",
            order_lines(
                "o",
                &[
                    ("l1", huge.clone(), dec(1), dec(0), dec(0)),
                    ("l2", huge, dec(1), dec(0), dec(0)),
                ],
            ),
            "order_total",
            "total",
            "sum_range",
        ),
        (
            "a raising body beside a raising target",
            order_lines(
                "o",
                &[
                    ("l1", dec(3), dec(10), dec(5), subj("no_floor")),
                    ("l2", dec(3), dec(10), dec(0), dec(0)),
                ],
            ),
            "guarded_ratio",
            "ratio",
            "scope",
        ),
    ];
    for (label, claims, view, column, class) in cases {
        let state = State::from_claims(claims);
        let predicate = match view {
            "order_total" => "OrderTotal",
            "order_net" => "OrderNet",
            _ => "GuardedRatio",
        };
        assert!(
            validated.enumerate_derived(predicate, &state).is_err(),
            "{label}: the kernel raises"
        );
        assert_eq!(agree(&pool, &validated, &state, label).await, 4);
        let (text, error) = raw(&pool, view, column, "o").await;
        assert_eq!((text, error.as_deref()), (None, Some(class)), "{label}");
    }
}

/// A target that brings in an aggregate of its own is refused by name,
/// not rendered, written out or inside a called definition: the per-row
/// scope has no place for it.
#[test]
fn a_computed_target_with_its_own_aggregate_is_refused() {
    for target in ["q * sum(1 | Order(o))", "weight(q)"] {
        let program = morpholog_surface::parse_program(&format!(
            "program nested
predicate Order(order: Subject)
predicate SalesLine(line: Subject, order: Subject, quantity: Decimal)
predicate Weighted(order: Subject, total: Decimal)
define weight(x) = (x * sum(1 | Order(o)))
derived Weighted(order):
    over Order(order: order)
    value total = sum({target} | SalesLine(l, order, q))
"
        ))
        .unwrap();
        let validated = program.validated().unwrap();
        let Err(refused) = render_views(
            &validated,
            SCHEMA,
            &format!("sha256:{}", "0".repeat(64)),
            DerivedSource::Live,
            &Default::default(),
            false,
        ) else {
            panic!("{target}: a nested aggregate in a target renders");
        };
        let text = format!("{refused:?}");
        assert!(
            text.contains("a computed target with an aggregate of its own"),
            "{target}: {text}"
        );
    }
}

/// A quantity stored where a decimal is declared is the attack: no
/// admission puts it there, a direct write can. The kernel's arithmetic
/// keeps its unit, so the row's target is a quantity, which a decimal
/// column cannot hold: the view names `value` for that order rather than
/// summing the bare amount. A quantity over one of the same unit is a
/// bare decimal, so that row computes as the kernel's figure. A value
/// definition's call is the same arithmetic under its arguments.
#[tokio::test]
async fn a_computed_target_that_carries_a_unit_is_not_summed_as_a_decimal() {
    let pool = test_pool().await;
    let program = morpholog_surface::parse_program(
        "program quantities
predicate Order(order: Subject)
predicate Line(line: Subject, order: Subject, quantity: Decimal, price: Decimal)
predicate Weighted(order: Subject, total: Decimal)
predicate Size(order: Subject, total: Decimal)
predicate Ratio(order: Subject, total: Decimal)
predicate Scaled(order: Subject, total: Decimal)
define scaled(x, y) = (x * y)
derived Weighted(order):
    over Order(order: order)
    value total = sum(q * p | Line(l, order, q, p))
derived Size(order):
    over Order(order: order)
    value total = sum(abs(q) | Line(l, order, q, p))
derived Ratio(order):
    over Order(order: order)
    value total = sum(q / p | Line(l, order, q, p))
derived Scaled(order):
    over Order(order: order)
    value total = sum(scaled(q, p) | Line(l, order, q, p))
",
    )
    .unwrap();
    let validated = program.validated().unwrap();
    let kg = |amount: &str| morpholog_test_support::qty(amount, "kg");
    let lines = |q: EvalValue, p: EvalValue| {
        State::from_claims(vec![
            claim_instance("Order", &[subj("o")]),
            claim_instance("Line", &[subj("l1"), subj("o"), q, p]),
        ])
    };
    let reseed = async |state: &State| {
        apply(&pool, &validated).await.unwrap();
        crate::testing::reset_database(&pool).await.unwrap();
        seed(&pool, &state.claims().to_vec()).await;
    };

    let state = lines(kg("-3"), dec(2));
    reseed(&state).await;
    for (predicate, view, kernel) in [
        ("Weighted", "weighted", "-6"),
        ("Size", "size", "3"),
        ("Scaled", "scaled", "-6"),
    ] {
        let rows = validated
            .enumerate_derived(predicate, &state)
            .unwrap()
            .unwrap();
        assert_eq!(
            rows[0].args[1],
            kg(kernel),
            "{predicate}: the kernel keeps the unit"
        );
        let (text, error) = raw(&pool, view, "total", "o").await;
        assert_eq!(
            (text, error.as_deref()),
            (None, Some("value")),
            "{predicate}"
        );
    }

    let state = lines(kg("6"), kg("2"));
    reseed(&state).await;
    let rows = validated
        .enumerate_derived("Ratio", &state)
        .unwrap()
        .unwrap();
    assert_eq!(rows[0].args[1], dec(3), "kg over kg is a bare decimal");
    let (text, error) = raw(&pool, "ratio", "total", "o").await;
    assert_eq!(error, None);
    let live: rust_decimal::Decimal = text.unwrap().parse().unwrap();
    assert_eq!(live.normalize().to_string(), "3");
}

/// A key spelt two ways is one key: a decimal at two scales is one level,
/// in the kernel and in the view, though its column leads the view's
/// `DISTINCT ON`. A key holding a value of another kind than it declares
/// shows NULL, as a history view's field does, so its two spellings are
/// one row too; a key of any kind keeps the value key alone.
#[tokio::test]
async fn a_key_spelt_two_ways_is_one_row() {
    let pool = test_pool().await;
    let one = morpholog_test_support::dec_str("1.0");
    let one_again = morpholog_test_support::dec_str("1.00");
    let keyed = morpholog_surface::parse_program(KEYED).unwrap();
    let keyed_v = keyed.validated().unwrap();
    let state = State::from_claims(vec![
        claim_instance("Offer", &[one.clone(), dec(2)]),
        claim_instance("Offer", &[one_again.clone(), dec(3)]),
        claim_instance("Offer", &[dec(2), dec(1)]),
    ]);
    assert_eq!(agree(&pool, &keyed_v, &state, "keyed built").await, 2);
    let levels = live_rows(&pool, &keyed_v, "AtLevel").await;
    assert_eq!(levels.len(), 2, "{levels:?}");

    let any = morpholog_surface::parse_program(ANY_KEYED).unwrap();
    let any_v = any.validated().unwrap();
    let state = State::from_claims(vec![
        claim_instance("Tag", std::slice::from_ref(&one)),
        claim_instance("Tag", std::slice::from_ref(&one_again)),
        claim_instance("Tag", &[subj("t")]),
        claim_instance("Mark", &[one.clone(), subj("a")]),
        claim_instance("Mark", &[one_again.clone(), subj("b")]),
        claim_instance("Mark", &[subj("t"), subj("a")]),
        // Built, not admitted: as a deployment holds them from before
        // admission held each claim to its declared kinds.
        claim_instance("Source", std::slice::from_ref(&one)),
        claim_instance("Source", std::slice::from_ref(&one_again)),
        claim_instance("Source", &[subj("t")]),
        claim_instance("Basket", &[coll(vec![one.clone()])]),
        claim_instance("Basket", &[coll(vec![one_again.clone()])]),
        claim_instance("Basket", &[coll(vec![subj("t")])]),
    ]);
    apply(&pool, &any_v).await.expect("the view renders");
    crate::testing::reset_database(&pool).await.unwrap();
    seed(&pool, &state.claims().to_vec()).await;
    for predicate in ["FromSource", "Tagged", "Baskets", "Named", "Marked"] {
        let kernel = any_v.enumerate_derived(predicate, &state).unwrap().unwrap();
        let mut live = live_rows(&pool, &any_v, predicate).await;
        assert_eq!((live.len(), kernel.len()), (2, 2), "{predicate}: {live:?}");
        if !["Tagged", "Baskets"].contains(&predicate) {
            live.sort();
            assert_eq!(live[0].0, ["NULL"], "{predicate}: {live:?}");
        }
    }
}

/// A read of one key computes that key alone (#895): the filter on the
/// key column reaches the scan of the domain, below the step that keeps
/// one row per key, rather than waiting for every key to be computed.
#[tokio::test]
async fn a_read_of_one_key_filters_the_domains_scan() {
    let pool = test_pool().await;
    let keyed = morpholog_surface::parse_program(KEYED).unwrap();
    let keyed_v = keyed.validated().unwrap();
    let mut claims = Vec::new();
    for i in 0..50 {
        claims.push(claim_instance("Order", &[subj(&format!("o{i}")), dec(10)]));
        claims.push(claim_instance(
            "Fill",
            &[subj(&format!("f{i}")), subj(&format!("o{i}")), dec(1)],
        ));
    }
    let state = State::from_claims(claims);
    assert_eq!(agree(&pool, &keyed_v, &state, "keyed seek").await, 2);
    let plan: serde_json::Value = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "EXPLAIN (FORMAT JSON) SELECT * FROM {SCHEMA}.filled WHERE \"order\" = 'o7'"
    )))
    .fetch_one(&pool)
    .await
    .unwrap();
    let mut scans = Vec::new();
    domain_scans(&plan[0]["Plan"], &mut scans);
    assert_eq!(scans.len(), 1, "one scan of the domain: {plan}");
    assert!(
        scans[0].contains("'o7'"),
        "the key filter reaches the domain's scan: {plan}"
    );
}

/// The filter and index condition of every plan node that scans the
/// domain's claims, `t0`.
fn domain_scans(node: &serde_json::Value, out: &mut Vec<String>) {
    if node["Alias"] == "t0" {
        out.push(format!("{} {}", node["Filter"], node["Index Cond"]));
    }
    if let Some(children) = node["Plans"].as_array() {
        for child in children {
            domain_scans(child, out);
        }
    }
}
