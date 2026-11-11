//! A rule that ranges over a grid built from a claim's bounds: what the
//! checker holds it to, what the kernel decides, how admission bounds it,
//! and the three-step shape one ledger carries without a step from a
//! claim.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{claim_instance, date, dec_str, subj, ts};
use morpholog_core::{
    Bounding, ImpactPlan, Outcome, Program, RejectionReason, State, ValidationError, Widening,
};
use morpholog_surface::parse_program;
use morpholog_test_support::{must_accept, propose_with_test_actor};

const PRICED: &str = r#"
program priced

predicate Day(day: Date, starts_at: Timestamp, ends_at: Timestamp)
    unique by (day)
predicate Published(day: Date)
    append only
predicate Price(day: Date, hour: Timestamp, price: Decimal)
    unique by (day, hour)

invariant a_day_is_forward:
    Day(day, starts_at, ends_at) implies starts_at strictly_before ends_at

invariant a_published_day_is_priced:
    Published(day)
    implies Day(day, starts_at, ends_at)
    and ends_at - starts_at no_longer_than duration(PT25H)
    and (forall h in periods(starts_at, ends_at, duration(PT1H)): exists p: Price(day, h, p))

transformation declare(day, starts_at, ends_at):
    admit Day(day, starts_at, ends_at)

transformation price(day, hour, price):
    admit Price(day, hour, price)

transformation publish(day):
    admit Published(day)
"#;

fn parsed(source: &str) -> Program {
    let p = parse_program(source).expect("parses");
    p.validate().expect("validates");
    p
}

fn errors(source: &str) -> Vec<ValidationError> {
    parse_program(source)
        .expect("parses")
        .validate()
        .expect_err("refused")
}

/// Without a bound before it, the grid is refused by name; with a
/// computed step, likewise; and the binder is an instant, so comparing
/// it with a decimal is a kind error.
#[test]
fn a_grid_in_a_rule_needs_a_bound_before_it_and_a_literal_step() {
    let unbounded = errors(&PRICED.replace(
        "    and ends_at - starts_at no_longer_than duration(PT25H)\n",
        "",
    ));
    assert!(
        unbounded
            .iter()
            .any(|e| matches!(e, ValidationError::PeriodsUnbounded { .. })),
        "{unbounded:?}"
    );
    let computed = errors(&PRICED.replace(
        "periods(starts_at, ends_at, duration(PT1H))",
        "periods(starts_at, ends_at, ends_at - starts_at)",
    ));
    assert!(
        computed
            .iter()
            .any(|e| matches!(e, ValidationError::PeriodsUnbounded { .. })),
        "{computed:?}"
    );
    let decimal = errors(&PRICED.replace(
        "exists p: Price(day, h, p)",
        "exists p: Price(day, h, p) and h <= 10",
    ));
    assert!(
        !decimal.is_empty(),
        "an instant compared with a decimal is refused"
    );
}

/// Hand-built IR with another value as the source of `in` is refused by
/// name: the parser never writes one, and no walker is proven over it.
#[test]
fn the_source_of_in_is_a_variable_or_a_grid() {
    use morpholog_core::ir_builder::*;
    let p = program("shape")
        .predicates(vec![predicate("Seen").subject("x").build()])
        .invariants(vec![invariant(
            "odd",
            implies(
                claim("Seen", vec![var("x")]),
                in_grid(var("x"), add(term(dec("1")), term(dec("2")))),
            ),
        )])
        .build();
    let errors = p.validate().unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| matches!(e, ValidationError::InSourceShape { .. })),
        "{errors:?}"
    );
}

/// The kernel: a full day publishes, a short one is refused at the
/// missing hour, a day beyond the bound is refused by the bound.
#[test]
fn a_grid_rule_decides_per_hour_and_refuses_a_window_beyond_its_bound() {
    let p = parsed(PRICED);
    let declare = p.transformation("declare").unwrap();
    let price = p.transformation("price").unwrap();
    let publish = p.transformation("publish").unwrap();
    let day = |state: State, from: &str, until: &str| {
        must_accept(
            declare,
            vec![date("2026-06-01"), ts(from), ts(until)],
            state,
            &p,
        )
    };
    let mut state = day(
        State::default(),
        "2026-06-01T00:00:00Z",
        "2026-06-01T03:00:00Z",
    );
    for h in ["00", "01"] {
        state = must_accept(
            price,
            vec![
                date("2026-06-01"),
                ts(&format!("2026-06-01T{h}:00:00Z")),
                dec_str("5"),
            ],
            state,
            &p,
        );
    }
    let short = propose_with_test_actor(publish, vec![date("2026-06-01")], &state, &p);
    assert!(
        matches!(&short, Ok(Outcome::Rejected { reason: RejectionReason::Invariant { name, witness, .. } })
            if name.as_str() == "a_published_day_is_priced"
            && witness.iter().any(|w| w.var.as_str() == "h" && w.value == ts("2026-06-01T02:00:00Z"))),
        "{short:?}"
    );
    let state = must_accept(
        price,
        vec![date("2026-06-01"), ts("2026-06-01T02:00:00Z"), dec_str("5")],
        state,
        &p,
    );
    assert!(matches!(
        propose_with_test_actor(publish, vec![date("2026-06-01")], &state, &p),
        Ok(Outcome::Accepted { .. })
    ));
    // A window beyond the bound: the bound conjunct refuses it before
    // any grid is built.
    let long = day(
        State::default(),
        "2026-06-01T00:00:00Z",
        "2026-06-03T00:00:00Z",
    );
    let refused = propose_with_test_actor(publish, vec![date("2026-06-01")], &long, &p);
    assert!(
        matches!(&refused, Ok(Outcome::Rejected { reason: RejectionReason::Invariant { name, .. } })
            if name.as_str() == "a_published_day_is_priced"),
        "{refused:?}"
    );
}

/// Bounded is not non-empty: an empty window passes the grid rule, and
/// the day's own rule is what refuses it.
#[test]
fn an_empty_window_passes_the_grid_rule_and_fails_the_days_own_rule() {
    let p = parsed(PRICED);
    let declare = p.transformation("declare").unwrap();
    let empty = propose_with_test_actor(
        declare,
        vec![
            date("2026-06-01"),
            ts("2026-06-01T00:00:00Z"),
            ts("2026-06-01T00:00:00Z"),
        ],
        &State::default(),
        &p,
    );
    assert!(
        matches!(&empty, Ok(Outcome::Rejected { reason: RejectionReason::Invariant { name, .. } })
            if name.as_str() == "a_day_is_forward"),
        "{empty:?}"
    );
    // Without that rule, the empty day publishes with no price at all.
    let lax = parsed(&PRICED.replace(
        "invariant a_day_is_forward:\n    Day(day, starts_at, ends_at) implies starts_at strictly_before ends_at\n",
        "",
    ));
    let state = must_accept(
        lax.transformation("declare").unwrap(),
        vec![
            date("2026-06-01"),
            ts("2026-06-01T00:00:00Z"),
            ts("2026-06-01T00:00:00Z"),
        ],
        State::default(),
        &lax,
    );
    assert!(matches!(
        propose_with_test_actor(
            lax.transformation("publish").unwrap(),
            vec![date("2026-06-01")],
            &state,
            &lax
        ),
        Ok(Outcome::Accepted { .. })
    ));
}

/// A binder that takes a case variable's name is the scope's own; the
/// plan says so by checking the rule whole rather than reading the
/// body's hour as the case's instant.
#[test]
fn a_binder_that_shadows_a_case_variable_widens_the_rule() {
    let p = parsed(&PRICED.replace(
        "invariant a_published_day_is_priced:\n    Published(day)\n    implies Day(day, starts_at, ends_at)",
        "predicate Mark(day: Date, at: Timestamp)\n\ninvariant a_published_day_is_priced:\n    Published(day) and Mark(day, h)\n    implies Day(day, starts_at, ends_at)",
    ));
    let rule = p
        .invariants
        .iter()
        .find(|i| i.name.as_str() == "a_published_day_is_priced")
        .unwrap();
    let plan = ImpactPlan::with_definitions(rule, &p.definitions);
    assert!(
        matches!(plan.bounding(), Bounding::Whole(Widening::ScopeReadsBeforeBinding(v)) if v.as_str() == "h"),
        "{:?}",
        plan.bounding()
    );
}

const MARKETS: &str = r#"
program markets

predicate MarketStep(market: Subject, step: Subject)
    unique by (market)
predicate Limit(market: Subject, cap: Decimal)
    unique by (market)
predicate Trade(market: Subject, trade: Subject, mw: Decimal, starts_at: Timestamp, ends_at: Timestamp)
    unique by (trade)

invariant every_limited_market_has_a_step:
    Limit(market, _) implies MarketStep(market, _)

invariant quarter_hourly_limit:
    Trade(market, trade, _, starts_at, ends_at) and MarketStep(market, #quarter) and Limit(market, cap)
    implies ends_at - starts_at no_longer_than duration(PT744H)
    and (forall q in periods(starts_at, ends_at, duration(PT15M)): sum(m | Trade(market, _, m, s, e) and s at_or_before q and q strictly_before e) <= cap)

invariant half_hourly_limit:
    Trade(market, trade, _, starts_at, ends_at) and MarketStep(market, #half) and Limit(market, cap)
    implies ends_at - starts_at no_longer_than duration(PT744H)
    and (forall q in periods(starts_at, ends_at, duration(PT30M)): sum(m | Trade(market, _, m, s, e) and s at_or_before q and q strictly_before e) <= cap)

invariant hourly_limit:
    Trade(market, trade, _, starts_at, ends_at) and MarketStep(market, #hour) and Limit(market, cap)
    implies ends_at - starts_at no_longer_than duration(PT744H)
    and (forall q in periods(starts_at, ends_at, duration(PT1H)): sum(m | Trade(market, _, m, s, e) and s at_or_before q and q strictly_before e) <= cap)

transformation open_market(market, step, cap):
    admit MarketStep(market, step)
    admit Limit(market, cap)

transformation set_limit(market, cap):
    admit Limit(market, cap)

transformation capture(market, trade, mw, starts_at, ends_at):
    admit Trade(market, trade, mw, starts_at, ends_at)
"#;

/// Three markets, three steps, one ledger: each rule checks its own
/// grid under its market's declared step, and a market with a limit
/// and no step is refused, so no rule can be vacuous by omission.
#[test]
fn three_steps_coexist_each_selected_by_its_markets_declared_step() {
    let p = parsed(MARKETS);
    let open = p.transformation("open_market").unwrap();
    let capture = p.transformation("capture").unwrap();
    let mut state = State::default();
    for (market, step) in [("de", "quarter"), ("gb", "half"), ("hourly", "hour")] {
        state = must_accept(
            open,
            vec![subj(market), subj(step), dec_str("10")],
            state,
            &p,
        );
    }
    // Two trades of six in the same hour: fourteen in every period they
    // share, over the cap, in each market by its own rule.
    for (market, rule) in [
        ("de", "quarter_hourly_limit"),
        ("gb", "half_hourly_limit"),
        ("hourly", "hourly_limit"),
    ] {
        let first = must_accept(
            capture,
            vec![
                subj(market),
                subj(&format!("{market}_1")),
                dec_str("6"),
                ts("2026-06-01T10:00:00Z"),
                ts("2026-06-01T12:00:00Z"),
            ],
            state.clone(),
            &p,
        );
        let second = propose_with_test_actor(
            capture,
            vec![
                subj(market),
                subj(&format!("{market}_2")),
                dec_str("6"),
                ts("2026-06-01T11:00:00Z"),
                ts("2026-06-01T13:00:00Z"),
            ],
            &first,
            &p,
        );
        assert!(
            matches!(&second, Ok(Outcome::Rejected { reason: RejectionReason::Invariant { name, .. } }) if name.as_str() == rule),
            "{market}: {second:?}"
        );
        state = first;
    }
    for name in ["quarter_hourly_limit", "half_hourly_limit", "hourly_limit"] {
        let rule = p
            .invariants
            .iter()
            .find(|i| i.name.as_str() == name)
            .unwrap();
        assert!(
            matches!(
                ImpactPlan::with_definitions(rule, &p.definitions).bounding(),
                Bounding::Bound
            ),
            "{name} is bounded to its trade"
        );
    }
    let stepless = propose_with_test_actor(
        p.transformation("set_limit").unwrap(),
        vec![subj("fr"), dec_str("10")],
        &state,
        &p,
    );
    assert!(
        matches!(&stepless, Ok(Outcome::Rejected { reason: RejectionReason::Invariant { name, .. } }) if name.as_str() == "every_limited_market_has_a_step"),
        "{stepless:?}"
    );
    let _ = claim_instance;
}
