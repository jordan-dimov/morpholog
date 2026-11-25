//! A sum over a set in an order, asked once per row of the order, is
//! answered from one sorted group per act, and that changes no result.
//! A matching walk that takes from a book in price and time priority,
//! and an allocation in rank order, decide the same streams of acts with
//! the memo on and off: every outcome, trace and explanation, compared as
//! text so a decimal's scale counts. The books have equal prices, mixed
//! scales, both sides and a trader meeting their own orders; some states
//! hold a rank that is not a decimal, so the errors are compared too. Each
//! accepted act's state is the next act's pre-state. The switch is
//! process-wide, so this binary holds this one test.

#![expect(
    clippy::expect_used,
    reason = "a helper here fails fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

use morpholog_core::{ClaimInstance, EvalValue, Outcome, Program, State, Subject};
use morpholog_surface::parse_program;

const PROGRAM: &str = "
program ordered_prefix

predicate Resting(order: Subject, contract: Subject, trader: Subject, side: Subject, price: Decimal, seq: Decimal, qty: Decimal)
predicate Trade(trade: Subject, taker: Subject, maker: Subject, price: Decimal, qty: Decimal)
predicate Want(want: Subject, item: Subject, rank: Any, qty: Decimal)
predicate Stock(item: Subject, qty: Decimal)
predicate Allotted(want: Subject, qty: Decimal)

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
fn acts(seed: u64) -> Vec<(String, Vec<EvalValue>, String)> {
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
    out
}

/// Everything the kernel answers about one stream, as text, and how
/// many trades it admitted.
fn answers(program: &Program, seed: u64) -> (Vec<String>, usize) {
    let mut pre = state(seed);
    let mut out = Vec::new();
    let mut trades = 0;
    for (name, args, actor) in acts(seed) {
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
            trades += asserted_claims
                .iter()
                .filter(|c| c.predicate.as_str() == "Trade")
                .count();
            pre = candidate_state;
        }
    }
    (out, trades)
}

#[test]
fn sorted_groups_change_no_answer() {
    let program = parse_program(PROGRAM).expect("the programme parses");
    program.validated().expect("the programme validates");
    let mut compared = 0;
    let mut trades = 0;
    for seed in 0..120 {
        morpholog_core::set_memo(false);
        let (off, _) = answers(&program, seed);
        morpholog_core::set_memo(true);
        let (on, traded) = answers(&program, seed);
        assert_eq!(on.len(), off.len());
        for (i, (on, off)) in on.iter().zip(&off).enumerate() {
            assert_eq!(on, off, "seed {seed}, answer {i}");
        }
        trades += traded;
        compared += on.len();
    }
    assert!(compared >= 1900, "answers compared: {compared}");
    assert!(trades > 100, "trades admitted: {trades}");
}
