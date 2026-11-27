//! A sum over a set in an order, asked once per row of the order, is
//! answered from one sorted group per act, and so is an `exists` asking
//! whether anything comes first; that changes no result. Three workloads
//! decide the same streams of acts with the memo on and off:
//! - a matching walk that takes from a book in price and time priority,
//!   an allocation in rank order, and the same two orders asked as
//!   "nothing of mine ahead of this" and "nothing ranked before this";
//! - the embedder guide's shipment walk, which splits and empties items
//!   in the shipment's recorded order;
//! - a payment waterfall, which pays each tranche what the tranches
//!   senior to it leave, and the most junior one what is left over.
//!
//! Every outcome, trace and explanation is compared as text, so a
//! decimal's scale counts. The books have equal prices, mixed scales,
//! both sides and a trader meeting their own orders; some states hold a
//! rank that is not a decimal, so the errors are compared too. Each
//! accepted act's state is the next act's pre-state. The switch is
//! process-wide, so this binary holds this one test.

#![expect(
    clippy::expect_used,
    reason = "a helper here fails fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

use std::collections::BTreeMap;

use morpholog_core::{ClaimInstance, EvalValue, Outcome, Program, State, Subject};
use morpholog_surface::parse_program;

const PROGRAM: &str = "
program ordered_prefix

predicate Resting(order: Subject, contract: Subject, trader: Subject, side: Subject, price: Decimal, seq: Decimal, qty: Decimal)
predicate Trade(trade: Subject, taker: Subject, maker: Subject, price: Decimal, qty: Decimal)
predicate Want(want: Subject, item: Subject, rank: Any, qty: Decimal)
predicate Stock(item: Subject, qty: Decimal)
predicate Allotted(want: Subject, qty: Decimal)
predicate Clear(order: Subject)
predicate First(want: Subject)

-- (p, s) comes before (p2, s2) in the walk of the side `side` takes from.
define before(side, p, s, p2, s2):
    (side = #buy and p < p2) or (side = #sell and p > p2) or (p = p2 and s < s2)

define meets(contract, side, price, r, rt, p, s, q):
    (side = #buy and Resting(r, contract, rt, #sell, p, s, q) and p <= price)
    or (side = #sell and Resting(r, contract, rt, #buy, p, s, q) and p >= price)

-- What the walk takes before (p, s): another trader's orders it meets.
define ahead_of(contract, side, price, trader, p, s) = (sum(q2 | meets(contract, side, price,
    r2, rt2, p2, s2, q2) and rt2 != trader and before(side, p2, s2, p, s)))

define ranked_before(item, r) = (sum(q2 | Want(w2, item, r2, q2) and r2 < r))

-- Whether the trader has an order of their own the walk meets first.
define own_ahead(contract, side, price, trader, p, s):
    exists o: meets(contract, side, price, o, trader, p2, s2, _) and before(side, p2, s2, p, s)

invariant allotments_follow_rank:
    Allotted(w, a) and Want(w, item, r, q) and Stock(item, st) implies a + ranked_before(item, r) <= st

transformation submit(order, contract, side, price: Decimal, quantity: Decimal):
    if side = #buy:
        for Resting(r, contract, rt, #sell, p, s, q):
            if rt != actor and p <= price:
                let ahead = ahead_of(contract, side, price, actor, p, s)
                if ahead < quantity:
                    let take = min(q, quantity - ahead)
                    admit Trade(new Subject(), order, r, p, take)
                    retract Resting(r, contract, rt, #sell, p, s, q)
                    if take < q:
                        let rest = q - take
                        admit Resting(r, contract, rt, #sell, p, s, rest)
    if side = #sell:
        for Resting(r, contract, rt, #buy, p, s, q):
            if rt != actor and p >= price:
                let ahead = ahead_of(contract, side, price, actor, p, s)
                if ahead < quantity:
                    let take = min(q, quantity - ahead)
                    admit Trade(new Subject(), order, r, p, take)
                    retract Resting(r, contract, rt, #buy, p, s, q)
                    if take < q:
                        let rest = q - take
                        admit Resting(r, contract, rt, #buy, p, s, rest)

transformation clear(contract, side, price: Decimal):
    if side = #buy:
        for Resting(r, contract, rt, #sell, p, s, q):
            if not own_ahead(contract, side, price, actor, p, s):
                admit Clear(r)
    if side = #sell:
        for Resting(r, contract, rt, #buy, p, s, q):
            if not own_ahead(contract, side, price, actor, p, s):
                admit Clear(r)

transformation first(item):
    for Want(w, item, r, q):
        if not (exists w2: Want(w2, item, r2, _) and r2 < r):
            admit First(w)

transformation allot(item):
    bind Stock(item, stock)
    for Want(w, item, r, q):
        let before_me = ranked_before(item, r)
        if before_me < stock:
            let give = min(q, stock - before_me)
            admit Allotted(w, give)
";

const DECIDED_AT: &str = "2026-01-01T00:00:00Z";

/// A small deterministic generator, so a failure names its seed.
struct Draw(u64);

impl Draw {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
}

fn subject(s: &str) -> EvalValue {
    EvalValue::Subject(Subject::from(s))
}

fn decimal(s: &str) -> EvalValue {
    EvalValue::Decimal(s.parse().expect("a decimal literal"))
}

fn claim(predicate: &str, args: Vec<EvalValue>) -> ClaimInstance {
    ClaimInstance {
        predicate: predicate.into(),
        args,
    }
}

/// Prices from a few levels, written at two scales, so equal prices tie.
fn price(d: &mut Draw) -> EvalValue {
    let level = 98 + d.next(5);
    decimal(&if d.next(2) == 0 {
        format!("{level}")
    } else {
        format!("{level}.00")
    })
}

fn state(seed: u64) -> State {
    let mut d = Draw(seed);
    let mut claims = Vec::new();
    for i in 0..(4 + d.next(24)) {
        let side = if d.next(2) == 0 { "buy" } else { "sell" };
        let contract = if d.next(4) == 0 { "c2" } else { "c1" };
        let trader = format!("t{}", d.next(3));
        let seq = decimal(&format!("{}", d.next(12)));
        let qty = decimal(&format!("{}.{}", 1 + d.next(9), d.next(10)));
        claims.push(claim(
            "Resting",
            vec![
                subject(&format!("o{i}")),
                subject(contract),
                subject(&trader),
                subject(side),
                price(&mut d),
                seq,
                qty,
            ],
        ));
    }
    for item in ["i1", "i2"] {
        claims.push(claim(
            "Stock",
            vec![subject(item), decimal(&format!("{}", 3 + d.next(20)))],
        ));
        for w in 0..(1 + d.next(8)) {
            let rank = if seed % 7 == 3 && w == 0 {
                subject("first")
            } else {
                decimal(&format!("{}", d.next(5)))
            };
            claims.push(claim(
                "Want",
                vec![
                    subject(&format!("{item}_w{w}")),
                    subject(item),
                    rank,
                    decimal(&format!("{}", 1 + d.next(6))),
                ],
            ));
        }
    }
    State::from_claims(claims)
}

/// The acts of one stream: orders on both sides, then the allocations.
fn acts(seed: u64) -> Acts {
    let mut d = Draw(seed ^ 0x9e37);
    let mut out = Vec::new();
    for n in 0..6 {
        let side = if d.next(2) == 0 { "buy" } else { "sell" };
        let args = vec![
            subject(&format!("new{n}")),
            subject(if d.next(5) == 0 { "c2" } else { "c1" }),
            subject(side),
            price(&mut d),
            decimal(&format!("{}", 1 + d.next(15))),
        ];
        out.push(("submit".to_string(), args, format!("t{}", d.next(3))));
    }
    for item in ["i1", "i2"] {
        out.push(("allot".to_string(), vec![subject(item)], "t0".to_string()));
    }
    // Drawn apart, so the streams above stay as they were.
    let mut d = Draw(seed ^ 0x51ed);
    for _ in 0..3 {
        let side = if d.next(2) == 0 { "buy" } else { "sell" };
        let args = vec![
            subject(if d.next(5) == 0 { "c2" } else { "c1" }),
            subject(side),
            price(&mut d),
        ];
        out.push(("clear".to_string(), args, format!("t{}", d.next(3))));
    }
    for item in ["i1", "i2"] {
        out.push(("first".to_string(), vec![subject(item)], "t0".to_string()));
    }
    out
}

/// The embedder guide's walk in a recorded order, as the guide prints it.
fn shipping() -> Program {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/embedder-integration.md");
    let guide = std::fs::read_to_string(&path).expect("docs/embedder-integration.md");
    let block = guide
        .split_once("### A walk in a recorded order")
        .expect("the guide has the section")
        .1
        .split_once("```morph\n")
        .expect("the section has a programme")
        .1
        .split_once("```")
        .expect("the block closes")
        .0;
    parse_program(block).expect("the guide's programme parses")
}

/// Shipments whose allocations hold distinct places, over a few items,
/// some drawn more than once in one shipment.
fn shipping_state(seed: u64) -> State {
    let mut d = Draw(seed ^ 0x5a17);
    let mut claims = Vec::new();
    for i in 0..4 {
        claims.push(claim(
            "Item",
            vec![
                subject(&format!("i{i}")),
                decimal(&format!("{}", 1 + d.next(20))),
            ],
        ));
    }
    for s in ["s1", "s2"] {
        let n = d.next(12);
        claims.push(claim(
            "Shipment",
            vec![subject(s), decimal(&format!("{n}"))],
        ));
        for seq in 0..n {
            claims.push(claim(
                "Alloc",
                vec![
                    subject(&format!("{s}_a{seq}")),
                    subject(s),
                    subject(&format!("i{}", d.next(4))),
                    decimal(&format!("{}", 1 + d.next(8))),
                    decimal(&format!("{seq}")),
                ],
            ));
        }
    }
    State::from_claims(claims)
}

/// More allocations, a merge now and then, then both shipments shipped.
fn shipping_acts(seed: u64) -> Vec<(String, Vec<EvalValue>, String)> {
    let mut d = Draw(seed ^ 0xc0de);
    let mut out = Vec::new();
    for n in 0..3 {
        let args = vec![
            subject(&format!("new{n}")),
            subject(if d.next(2) == 0 { "s1" } else { "s2" }),
            subject(&format!("i{}", d.next(4))),
            decimal(&format!("{}", 1 + d.next(8))),
        ];
        out.push(("allocate".to_string(), args, "t0".to_string()));
    }
    if d.next(3) == 0 {
        out.push((
            "merge_stock".to_string(),
            vec![subject("i1"), subject("i0")],
            "t0".to_string(),
        ));
    }
    for s in ["s1", "s2"] {
        out.push(("ship".to_string(), vec![subject(s)], "t0".to_string()));
    }
    out
}

const WATERFALL: &str = "
program waterfall

predicate Tranche(tranche: Subject, deal: Subject, rank: Any, due: Decimal)
predicate Cash(deal: Subject, amount: Decimal)
predicate Paid(tranche: Subject, amount: Decimal)
predicate Residual(tranche: Subject, amount: Decimal)

-- What the tranches senior to this rank are owed.
define owed_ahead(deal, rank) = (sum(d2 | Tranche(_, deal, r2, d2) and r2 < rank))

-- Each tranche is paid what the senior tranches leave of the cash, up to
-- its due; the most junior one also takes whatever is left after it.
transformation pay(deal):
    bind Cash(deal, cash)
    for Tranche(t, deal, rank, due):
        let ahead = owed_ahead(deal, rank)
        if ahead < cash:
            let paid = min(due, cash - ahead)
            admit Paid(t, paid)
        if not (exists t2: Tranche(t2, deal, r2, _) and r2 > rank):
            let left = cash - ahead - due
            if left > 0:
                admit Residual(t, left)
";

/// Deals with tranches in rank order, ranks sometimes shared and written
/// at two scales; some states hold a rank that is not a decimal.
fn waterfall_state(seed: u64) -> State {
    let mut d = Draw(seed ^ 0x7a7e);
    let mut claims = Vec::new();
    for deal in ["d1", "d2"] {
        claims.push(claim(
            "Cash",
            vec![subject(deal), decimal(&format!("{}", d.next(60)))],
        ));
        for t in 0..(1 + d.next(6)) {
            let rank = if seed % 9 == 4 && t == 0 {
                subject("senior")
            } else if d.next(2) == 0 {
                decimal(&format!("{}", d.next(5)))
            } else {
                decimal(&format!("{}.0", d.next(5)))
            };
            claims.push(claim(
                "Tranche",
                vec![
                    subject(&format!("{deal}_t{t}")),
                    subject(deal),
                    rank,
                    decimal(&format!("{}", 1 + d.next(20))),
                ],
            ));
        }
    }
    State::from_claims(claims)
}

fn waterfall_acts(_seed: u64) -> Vec<(String, Vec<EvalValue>, String)> {
    ["d1", "d2"]
        .into_iter()
        .map(|deal| ("pay".to_string(), vec![subject(deal)], "t0".to_string()))
        .collect()
}

type Acts = Vec<(String, Vec<EvalValue>, String)>;

/// Everything the kernel answers about one stream, as text, and how many
/// claims of each predicate its accepted acts admitted.
fn answers(
    program: &Program,
    mut pre: State,
    acts: Acts,
    seed: u64,
) -> (Vec<String>, BTreeMap<String, usize>) {
    let mut out = Vec::new();
    let mut admitted = BTreeMap::new();
    for (name, args, actor) in acts {
        let t = program
            .transformations
            .iter()
            .find(|t| t.name.as_str() == name)
            .expect("the act names a transformation");
        let transition =
            morpholog_test_support::transition_at(t, args, Subject::from(actor), DECIDED_AT);
        let drawn = || (0..).map(|i| Subject::from(format!("drawn_{seed}_{i}")));
        let explanation = morpholog_test_support::explain(program, &transition, &pre, &mut drawn());
        out.push(serde_json::to_string(&explanation).expect("an explanation serialises"));
        let traced =
            morpholog_test_support::propose_with_trace(program, &transition, &pre, &mut drawn());
        out.push(format!("{traced:?}"));
        if let Ok(Outcome::Accepted {
            asserted_claims,
            candidate_state,
            ..
        }) = morpholog_test_support::propose(program, &transition, &pre, &mut drawn())
        {
            for c in &asserted_claims {
                *admitted.entry(c.predicate.to_string()).or_insert(0) += 1;
            }
            pre = candidate_state;
        }
    }
    (out, admitted)
}

/// One workload's streams with the memo off and on: every answer the
/// same, and how many claims of each predicate were admitted and how
/// many answers were errors, over all its streams.
fn compare(
    name: &str,
    program: &Program,
    state: fn(u64) -> State,
    acts: fn(u64) -> Acts,
) -> (usize, BTreeMap<String, usize>, usize) {
    program.validated().expect("the programme validates");
    assert!(
        !morpholog_core::ordered_sums(program).is_empty(),
        "{name}: the programme has shapes the sorted groups answer"
    );
    let mut compared = 0;
    let mut admitted: BTreeMap<String, usize> = BTreeMap::new();
    let mut errors = 0;
    for seed in 0..120 {
        morpholog_core::set_memo(false);
        let (off, _) = answers(program, state(seed), acts(seed), seed);
        morpholog_core::set_memo(true);
        let (on, these) = answers(program, state(seed), acts(seed), seed);
        assert_eq!(on.len(), off.len());
        for (i, (on, off)) in on.iter().zip(&off).enumerate() {
            assert_eq!(on, off, "{name}: seed {seed}, answer {i}");
        }
        for (p, n) in these {
            *admitted.entry(p).or_insert(0) += n;
        }
        errors += on
            .iter()
            .filter(|a| a.contains("TypeMismatch") || a.contains("type_mismatch"))
            .count();
        compared += on.len();
    }
    (compared, admitted, errors)
}

#[test]
fn sorted_groups_change_no_answer() {
    let count = |admitted: &BTreeMap<String, usize>, p: &str| admitted.get(p).copied().unwrap_or(0);

    let book = parse_program(PROGRAM).expect("the programme parses");
    let (compared, admitted, errors) = compare("book", &book, state, acts);
    assert!(compared >= 3000, "book: answers compared: {compared}");
    assert!(count(&admitted, "Trade") > 100, "book: {admitted:?}");
    assert!(
        count(&admitted, "Clear") + count(&admitted, "First") > 100,
        "book: {admitted:?}"
    );
    assert!(errors > 0, "book: errors compared: {errors}");

    let (compared, admitted, _) = compare("shipping", &shipping(), shipping_state, shipping_acts);
    assert!(compared >= 1000, "shipping: answers compared: {compared}");
    assert!(count(&admitted, "Shipped") > 300, "shipping: {admitted:?}");

    let waterfall = parse_program(WATERFALL).expect("the programme parses");
    let (compared, admitted, errors) =
        compare("waterfall", &waterfall, waterfall_state, waterfall_acts);
    assert!(compared >= 400, "waterfall: answers compared: {compared}");
    assert!(count(&admitted, "Paid") > 100, "waterfall: {admitted:?}");
    assert!(count(&admitted, "Residual") > 10, "waterfall: {admitted:?}");
    assert!(errors > 0, "waterfall: errors compared: {errors}");
}
