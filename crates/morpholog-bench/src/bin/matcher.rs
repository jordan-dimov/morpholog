//! The matcher's binding pressure, measured apart from the suite (#352).
//!
//! Each shape is one rule, or one act, over an in-memory state built
//! outside every timer, scaled on two axes where the shape allows: the
//! witnesses it matches and the variables live in each binding frame.
//! For each point it reports the median wall time of an evaluation, and
//! for one further evaluation the allocations and bytes it made and the
//! binding maps the evaluator copied, with the entries those held. The
//! counts are exact and the same every run; the times are this
//! machine's. Nothing here is asserted beyond the evaluation's verdict.

use std::alloc::System;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use morpholog_core::{ClaimInstance, EvalValue, Outcome, PreparedProgram, State, Transition};
use rust_decimal::Decimal;
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

fn subject(name: &str) -> EvalValue {
    EvalValue::Subject(name.into())
}

fn claim(predicate: &str, args: Vec<EvalValue>) -> ClaimInstance {
    ClaimInstance {
        predicate: predicate.into(),
        args,
    }
}

/// One measured point: a programme, a state, and what to evaluate.
struct Point {
    shape: &'static str,
    witnesses: usize,
    live_vars: usize,
    prepared: PreparedProgram,
    state: State,
    run: Run,
}

enum Run {
    /// Evaluate the named invariant over the whole state; it must hold.
    Invariant(&'static str),
    /// Propose the transition against the state; it must be accepted.
    Propose(Transition),
}

fn prepare(source: &str) -> Result<PreparedProgram> {
    let program = morpholog_surface::parse_program(source)
        .map_err(|d| anyhow!("the shape does not parse: {d:?}"))?;
    PreparedProgram::new(program).map_err(|e| anyhow!("the shape does not validate: {e:?}"))
}

/// A long `and` chain: each conjunct binds the variable the next one
/// reads, `live_vars` of them in the frame by the end.
fn chain(witnesses: usize, live_vars: usize) -> Result<Point> {
    let links: Vec<String> = (1..live_vars)
        .map(|i| format!("Link(x{}, x{i})", i - 1))
        .collect();
    let source = format!(
        "program chain\n\
         predicate Root(x: Subject)\n\
         predicate Link(from: Subject, to: Subject)\n\
         predicate Ok(x: Subject)\n\
         invariant chain_closes:\n    Root(x0) and {} implies Ok(x0)\n",
        links.join(" and ")
    );
    let mut claims = Vec::new();
    for w in 0..witnesses {
        let root = format!("r{w}");
        claims.push(claim("Root", vec![subject(&root)]));
        claims.push(claim("Ok", vec![subject(&root)]));
        let mut from = root;
        for i in 1..live_vars {
            let to = format!("n{w}_{i}");
            claims.push(claim("Link", vec![subject(&from), subject(&to)]));
            from = to;
        }
    }
    Ok(Point {
        shape: "and chain",
        witnesses,
        live_vars,
        prepared: prepare(&source)?,
        state: State::from_claims(claims),
        run: Run::Invariant("chain_closes"),
    })
}

/// An `or` whose two branches export the same bindings to the conjunct
/// after it, half the witnesses through each.
fn branches(witnesses: usize) -> Result<Point> {
    let source = "program branches\n\
         predicate A(x: Subject, y: Subject)\n\
         predicate B(x: Subject, y: Subject)\n\
         predicate C(y: Subject)\n\
         predicate Ok(x: Subject)\n\
         invariant either_branch:\n    (A(x, y) or B(x, y)) and C(y) implies Ok(x)\n";
    let mut claims = Vec::new();
    for w in 0..witnesses {
        let (x, y) = (format!("x{w}"), format!("y{w}"));
        let branch = if w % 2 == 0 { "A" } else { "B" };
        claims.push(claim(branch, vec![subject(&x), subject(&y)]));
        claims.push(claim("C", vec![subject(&y)]));
        claims.push(claim("Ok", vec![subject(&x)]));
    }
    Ok(Point {
        shape: "or branches",
        witnesses,
        live_vars: 2,
        prepared: prepare(source)?,
        state: State::from_claims(claims),
        run: Run::Invariant("either_branch"),
    })
}

/// `forall` over a claim pattern with an `exists` inside it: each group
/// has four members, one of them active.
fn nested(witnesses: usize) -> Result<Point> {
    let source = "program nested\n\
         predicate Group(g: Subject)\n\
         predicate Member(g: Subject, m: Subject)\n\
         predicate Active(m: Subject)\n\
         invariant every_group_has_an_active_member:\n    \
         forall g in Group(g): exists m: Member(g, m) and Active(m)\n";
    let mut claims = Vec::new();
    for w in 0..witnesses {
        let g = format!("g{w}");
        claims.push(claim("Group", vec![subject(&g)]));
        for k in 0..4 {
            let m = format!("m{w}_{k}");
            claims.push(claim("Member", vec![subject(&g), subject(&m)]));
            if k == 3 {
                claims.push(claim("Active", vec![subject(&m)]));
            }
        }
    }
    Ok(Point {
        shape: "forall, exists",
        witnesses,
        live_vars: 2,
        prepared: prepare(source)?,
        state: State::from_claims(claims),
        run: Run::Invariant("every_group_has_an_active_member"),
    })
}

/// A defined call whose body binds a variable the caller never sees:
/// three two-hop paths from each root, projected to the call's arguments.
fn defined(witnesses: usize) -> Result<Point> {
    let source = "program defined\n\
         predicate Root(x: Subject)\n\
         predicate Link(from: Subject, to: Subject)\n\
         define reaches(a, b):\n    Link(a, m) and Link(m, b)\n\
         invariant every_root_reaches:\n    Root(x) implies reaches(x, _)\n";
    let mut claims = Vec::new();
    for w in 0..witnesses {
        let root = format!("r{w}");
        claims.push(claim("Root", vec![subject(&root)]));
        for k in 0..3 {
            let (mid, end) = (format!("m{w}_{k}"), format!("e{w}_{k}"));
            claims.push(claim("Link", vec![subject(&root), subject(&mid)]));
            claims.push(claim("Link", vec![subject(&mid), subject(&end)]));
        }
    }
    Ok(Point {
        shape: "defined call",
        witnesses,
        live_vars: 2,
        prepared: prepare(source)?,
        state: State::from_claims(claims),
        run: Run::Invariant("every_root_reaches"),
    })
}

/// One defined call with many distinct projections: a single group
/// whose `witnesses` members each carry a tag the call hides.
fn fan_out(witnesses: usize) -> Result<Point> {
    let source = "program fan_out\n\
         predicate Group(g: Subject)\n\
         predicate Member(g: Subject, m: Subject, t: Subject)\n\
         predicate Active(m: Subject)\n\
         define member_of(g, m):\n    Member(g, m, t)\n\
         invariant every_member_is_active:\n    Group(g) and member_of(g, m) implies Active(m)\n";
    let mut claims = vec![claim("Group", vec![subject("g")])];
    for w in 0..witnesses {
        let (m, t) = (format!("m{w}"), format!("t{w}"));
        claims.push(claim(
            "Member",
            vec![subject("g"), subject(&m), subject(&t)],
        ));
        claims.push(claim("Active", vec![subject(&m)]));
    }
    Ok(Point {
        shape: "defined fan-out",
        witnesses,
        live_vars: 2,
        prepared: prepare(source)?,
        state: State::from_claims(claims),
        run: Run::Invariant("every_member_is_active"),
    })
}

/// A sum over a body with many witnesses: ten entries, `witnesses`
/// lines each.
fn sum(witnesses: usize) -> Result<Point> {
    let source = "program sums\n\
         predicate Entry(e: Subject)\n\
         predicate Line(e: Subject, l: Subject, a: Decimal)\n\
         invariant entries_are_not_negative:\n    Entry(e) implies sum(a | Line(e, l, a)) >= 0\n";
    let mut claims = Vec::new();
    for e in 0..10 {
        let entry = format!("e{e}");
        claims.push(claim("Entry", vec![subject(&entry)]));
        for l in 0..witnesses {
            claims.push(claim(
                "Line",
                vec![
                    subject(&entry),
                    subject(&format!("l{e}_{l}")),
                    EvalValue::Decimal(Decimal::from(l)),
                ],
            ));
        }
    }
    Ok(Point {
        shape: "sum",
        witnesses,
        live_vars: 3,
        prepared: prepare(source)?,
        state: State::from_claims(claims),
        run: Run::Invariant("entries_are_not_negative"),
    })
}

/// An act's `for` over a collection, binding a claim per item and
/// admitting one: the statement bindings rather than a rule's.
fn for_bind(witnesses: usize) -> Result<Point> {
    let source = "program for_bind\n\
         predicate Node(x: Subject, v: Decimal)\n\
         predicate Seen(x: Subject, v: Decimal)\n\
         transformation mark(xs):\n    for x in xs:\n        bind Node(x, v)\n        admit Seen(x, v)\n";
    let mut claims = Vec::new();
    let mut xs = Vec::new();
    for w in 0..witnesses {
        let x = format!("x{w}");
        claims.push(claim(
            "Node",
            vec![subject(&x), EvalValue::Decimal(Decimal::from(w))],
        ));
        xs.push(subject(&x));
    }
    Ok(Point {
        shape: "for, bind",
        witnesses,
        live_vars: 2,
        prepared: prepare(source)?,
        state: State::from_claims(claims),
        run: Run::Propose(Transition {
            transformation_name: "mark".into(),
            args: vec![EvalValue::Collection(xs)],
            actor: "bench".into(),
        }),
    })
}

fn evaluate(point: &Point) -> Result<()> {
    match &point.run {
        Run::Invariant(name) => {
            let holds = point
                .prepared
                .validated()
                .eval_invariant(name, &point.state, None)
                .context("evaluating")?;
            if holds != Some(true) {
                return Err(anyhow!(
                    "{}: the rule does not hold: {holds:?}",
                    point.shape
                ));
            }
        }
        Run::Propose(transition) => {
            let outcome = point
                .prepared
                .propose(transition, &point.state, &mut std::iter::empty())
                .context("proposing")?;
            if !matches!(outcome, Some(Outcome::Accepted { .. })) {
                return Err(anyhow!("{}: the act is not accepted", point.shape));
            }
        }
    }
    Ok(())
}

/// One point's measures.
struct Measured {
    median: Duration,
    allocations: usize,
    bytes: usize,
    copies: u64,
    entries_copied: u64,
}

fn measure(point: &Point, repeat: usize) -> Result<Measured> {
    for _ in 0..3 {
        evaluate(point)?;
    }
    let mut times = Vec::with_capacity(repeat);
    for _ in 0..repeat {
        let t = Instant::now();
        evaluate(point)?;
        times.push(t.elapsed());
    }
    times.sort();
    let [copies_before, entries_before] = morpholog_core::tally::bindings();
    let region = Region::new(GLOBAL);
    evaluate(point)?;
    let stats = region.change();
    let [copies_after, entries_after] = morpholog_core::tally::bindings();
    Ok(Measured {
        median: times[times.len() / 2],
        allocations: stats.allocations + stats.reallocations,
        bytes: stats.bytes_allocated + stats.bytes_reallocated.max(0).unsigned_abs(),
        copies: copies_after - copies_before,
        entries_copied: entries_after - entries_before,
    })
}

/// One act admitting a claim per witness, each a case of its own of the
/// rule over them.
fn wide_delta(witnesses: usize) -> Result<Point> {
    let source = "program wide_delta\n\
         predicate Node(x: Subject, v: Decimal)\n\
         predicate Seen(x: Subject, v: Decimal)\n\
         transformation mark(xs):\n    for x in xs:\n        bind Node(x, v)\n        admit Seen(x, v)\n\
         invariant seen_is_a_node:\n    Seen(x, v) implies Node(x, v)\n";
    let mut point = for_bind(witnesses)?;
    point.shape = "wide delta";
    point.prepared = prepare(source)?;
    Ok(point)
}

/// One act admitting a claim per witness, the claims spread over a few
/// cases of the rule over them, named with how many.
const REPEATED: [(usize, &str); 5] = [
    (1, "repeated delta, 1 case"),
    (8, "repeated delta, 8 cases"),
    (16, "repeated delta, 16 cases"),
    (32, "repeated delta, 32 cases"),
    (64, "repeated delta, 64 cases"),
];

fn repeated_delta(witnesses: usize, (cases, shape): (usize, &'static str)) -> Result<Point> {
    let source = "program repeated_delta\n\
         predicate Owner(t: Subject, x: Subject)\n\
         predicate Seen(x: Subject, t: Subject)\n\
         predicate Live(x: Subject)\n\
         transformation mark(ts):\n    for t in ts:\n        bind Owner(t, x)\n        admit Seen(x, t)\n\
         invariant seen_is_live:\n    Seen(x, _) implies Live(x)\n";
    let mut claims: Vec<ClaimInstance> = (0..cases)
        .map(|c| claim("Live", vec![subject(&format!("x{c}"))]))
        .collect();
    let mut ts = Vec::new();
    for w in 0..witnesses {
        let t = format!("t{w}");
        claims.push(claim(
            "Owner",
            vec![subject(&t), subject(&format!("x{}", w % cases))],
        ));
        ts.push(subject(&t));
    }
    Ok(Point {
        shape,
        witnesses,
        live_vars: 2,
        prepared: prepare(source)?,
        state: State::from_claims(claims),
        run: Run::Propose(Transition {
            transformation_name: "mark".into(),
            args: vec![EvalValue::Collection(ts)],
            actor: "bench".into(),
        }),
    })
}

fn points(quick: bool) -> Result<Vec<Point>> {
    let witnesses: &[usize] = if quick { &[10] } else { &[100, 1_000, 10_000] };
    let live_vars: &[usize] = if quick { &[2] } else { &[2, 4, 8] };
    let mut points = Vec::new();
    for &w in witnesses {
        for &v in live_vars {
            points.push(chain(w, v)?);
        }
    }
    for &w in witnesses {
        points.push(branches(w)?);
        points.push(nested(w)?);
        points.push(defined(w)?);
        points.push(fan_out(w)?);
        points.push(sum(w)?);
        points.push(for_bind(w)?);
        points.push(wide_delta(w)?);
    }
    let repeated_over = if quick { 10 } else { 10_000 };
    for shape in REPEATED {
        points.push(repeated_delta(repeated_over, shape)?);
    }
    Ok(points)
}

fn per(n: f64, witnesses: usize) -> f64 {
    n / witnesses as f64
}

fn main() -> Result<()> {
    let quick = std::env::args().any(|a| a == "--quick");
    let repeat = if quick { 3 } else { 31 };
    println!(
        "| shape | witnesses | live vars | median | allocations | bytes | binding copies | entries copied | per witness: us, allocations, copies, entries |"
    );
    println!("|---|--:|--:|--:|--:|--:|--:|--:|---|");
    for point in points(quick)? {
        let m = measure(&point, repeat)?;
        let us = m.median.as_secs_f64() * 1e6;
        println!(
            "| {} | {} | {} | {:.1} us | {} | {} | {} | {} | {:.2}, {:.1}, {:.1}, {:.1} |",
            point.shape,
            point.witnesses,
            point.live_vars,
            us,
            m.allocations,
            m.bytes,
            m.copies,
            m.entries_copied,
            per(us, point.witnesses),
            per(m.allocations as f64, point.witnesses),
            per(m.copies as f64, point.witnesses),
            per(m.entries_copied as f64, point.witnesses),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every shape parses, holds or is accepted, and moves both counters:
    /// a shape that allocated nothing or copied no binding would be
    /// measuring something other than the matcher.
    #[test]
    fn every_shape_runs_and_is_counted() {
        for point in points(true).unwrap() {
            let m = measure(&point, 1).unwrap();
            assert!(m.allocations > 0, "{}: no allocation counted", point.shape);
            assert!(m.copies > 0, "{}: no binding copy counted", point.shape);
        }
    }
}
