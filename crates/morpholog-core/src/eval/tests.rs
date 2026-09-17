use super::*;
use crate::ir_builder::{
    dec, div, max, min, modulo, mul, subj, term, value_of, value_of_with_default, wildcard,
};
use crate::state::{ClaimInstance, State};

/// The extremum picks from a set, so match order must not decide it,
/// and the answer must be the member itself rather than a position.
///
/// Table-driven over both ends and both orderings of the same claims:
/// a max that depended on iteration would pass one row and fail its
/// mirror.
#[test]
fn an_extremum_picks_the_same_member_whatever_the_match_order() {
    use crate::ir::ExtremumOp;
    let rows = [("2025-01-01", 10), ("2026-01-01", 12), ("2024-06-01", 9)];
    let build = |order: &[usize]| {
        State::from_claims(
            order
                .iter()
                .map(|&i| ClaimInstance {
                    predicate: "Rate".into(),
                    args: vec![
                        EvalValue::Date(rows[i].0.parse().expect("date")),
                        EvalValue::Decimal(rust_decimal::Decimal::from(rows[i].1)),
                    ],
                })
                .collect(),
        )
    };
    let aggregate = |op| ValueExpr::Extremum {
        op,
        value: Term::Var(Var::from("d")),
        body: Box::new(crate::ir_builder::claim(
            "Rate",
            vec![Term::Var(Var::from("d")), Term::Wildcard],
        )),
    };
    for order in [[0usize, 1, 2], [2, 1, 0], [1, 0, 2]] {
        let state = build(&order);
        let bindings = Bindings::new();
        let ctx = EvalContext::new(
            &state,
            None,
            &bindings,
            None,
            crate::definitions::DefinitionTable::new(&[]),
        );
        assert_eq!(
            eval_value(&aggregate(ExtremumOp::Max), &ctx).expect("max"),
            EvalValue::Date("2026-01-01".parse().expect("date")),
            "max over {order:?}"
        );
        assert_eq!(
            eval_value(&aggregate(ExtremumOp::Min), &ctx).expect("min"),
            EvalValue::Date("2024-06-01".parse().expect("date")),
            "min over {order:?}"
        );
    }
}

/// Validity is a question of kind, never of how many claims happen
/// to match.
///
/// The first cut checked only comparisons, so one unordered match
/// succeeded and two raised a type error - the same programme going
/// from working to broken because a second claim was admitted. Every
/// candidate is checked now, singletons included.
#[test]
fn an_unordered_candidate_is_refused_however_many_there_are() {
    use crate::ir::ExtremumOp;
    let unordered = [
        ("subject", EvalValue::Subject(Subject::from("a"))),
        ("bool", EvalValue::Bool(true)),
        (
            "collection",
            EvalValue::Collection(vec![EvalValue::Subject(Subject::from("x"))]),
        ),
    ];
    for (label, first) in unordered {
        for count in [1usize, 2] {
            let state = State::from_claims(
                (0..count)
                    .map(|i| ClaimInstance {
                        predicate: "Thing".into(),
                        args: vec![if i == 0 {
                            first.clone()
                        } else {
                            EvalValue::Subject(Subject::from("b"))
                        }],
                    })
                    .collect(),
            );
            let bindings = Bindings::new();
            let ctx = EvalContext::new(
                &state,
                None,
                &bindings,
                None,
                crate::definitions::DefinitionTable::new(&[]),
            );
            let expr = ValueExpr::Extremum {
                op: ExtremumOp::Max,
                value: Term::Var(Var::from("v")),
                body: Box::new(crate::ir_builder::claim(
                    "Thing",
                    vec![Term::Var(Var::from("v"))],
                )),
            };
            let err = eval_value(&expr, &ctx).expect_err(&format!("{label} x{count} has no order"));
            assert!(
                err.to_string().contains("ordered kind"),
                "{label} x{count}: {err}"
            );
        }
    }
}

/// Every kind the checker admits must actually evaluate, or the
/// allow-list and the runtime disagree about what is filterable.
#[test]
fn every_ordered_kind_yields_its_largest_member() {
    use crate::ir::ExtremumOp;
    let cases: Vec<(&str, Vec<EvalValue>, EvalValue)> = vec![
        (
            "decimal",
            vec![
                EvalValue::Decimal(rust_decimal::Decimal::from(1)),
                EvalValue::Decimal(rust_decimal::Decimal::from(9)),
            ],
            EvalValue::Decimal(rust_decimal::Decimal::from(9)),
        ),
        (
            "date",
            vec![
                EvalValue::Date("2025-01-01".parse().expect("date")),
                EvalValue::Date("2026-01-01".parse().expect("date")),
            ],
            EvalValue::Date("2026-01-01".parse().expect("date")),
        ),
        (
            "timestamp",
            vec![
                EvalValue::Timestamp("2025-01-01T00:00:00Z".parse().expect("ts")),
                EvalValue::Timestamp("2026-01-01T00:00:00Z".parse().expect("ts")),
            ],
            EvalValue::Timestamp("2026-01-01T00:00:00Z".parse().expect("ts")),
        ),
        (
            "duration",
            vec![
                EvalValue::Duration("PT1H".parse().expect("dur")),
                EvalValue::Duration("PT6H".parse().expect("dur")),
            ],
            EvalValue::Duration("PT6H".parse().expect("dur")),
        ),
        (
            "quantity",
            vec![
                EvalValue::Quantity {
                    amount: rust_decimal::Decimal::from(5),
                    unit: "USD".into(),
                },
                EvalValue::Quantity {
                    amount: rust_decimal::Decimal::from(7),
                    unit: "USD".into(),
                },
            ],
            EvalValue::Quantity {
                amount: rust_decimal::Decimal::from(7),
                unit: "USD".into(),
            },
        ),
    ];
    for (label, values, expected) in cases {
        let state = State::from_claims(
            values
                .into_iter()
                .map(|v| ClaimInstance {
                    predicate: "Thing".into(),
                    args: vec![v],
                })
                .collect(),
        );
        let bindings = Bindings::new();
        let ctx = EvalContext::new(
            &state,
            None,
            &bindings,
            None,
            crate::definitions::DefinitionTable::new(&[]),
        );
        let expr = ValueExpr::Extremum {
            op: ExtremumOp::Max,
            value: Term::Var(Var::from("v")),
            body: Box::new(crate::ir_builder::claim(
                "Thing",
                vec![Term::Var(Var::from("v"))],
            )),
        };
        assert_eq!(
            eval_value(&expr, &ctx).unwrap_or_else(|e| panic!("{label}: {e}")),
            expected,
            "{label}"
        );
    }
}

/// An empty sum has a typed zero to fall back on; an empty extremum
/// has no answer, and inventing one would let a rule price against a
/// version that does not exist. It names the body so the author can
/// see which selection came up empty.
#[test]
fn an_extremum_over_nothing_refuses_by_name() {
    use crate::ir::ExtremumOp;
    let state = State::from_claims(vec![]);
    let bindings = Bindings::new();
    let ctx = EvalContext::new(
        &state,
        None,
        &bindings,
        None,
        crate::definitions::DefinitionTable::new(&[]),
    );
    let expr = ValueExpr::Extremum {
        op: ExtremumOp::Max,
        value: Term::Var(Var::from("d")),
        body: Box::new(crate::ir_builder::claim(
            "Rate",
            vec![Term::Var(Var::from("d"))],
        )),
    };
    let err = eval_value(&expr, &ctx).expect_err("an empty max has no value");
    let text = err.to_string();
    assert!(text.contains("matched nothing"), "got: {text}");
    assert!(text.contains("Rate"), "must name the body: {text}");
    assert!(text.contains("require"), "must name the remedy: {text}");
}

/// `claim_matches` and `unify_args` share `match_args`, so they must
/// agree on every verdict; `unify_args` must additionally extend the
/// base with exactly the new bindings. Pins that the boolean path
/// cannot drift from the binding-producing one.
#[test]
fn claim_matches_agrees_with_unify_args_and_extends_base() {
    let s = |x: &str| EvalValue::Subject(Subject::from(x));
    let var = |x: &str| Term::Var(Var::from(x));
    let lit = |x: &str| Term::Literal(Value::Subject(x.into()));
    let actor = Subject::from("alice");
    let mut base = Bindings::new();
    base.insert(Var::from("known"), s("k"));

    let cases: Vec<(Vec<Term>, Vec<EvalValue>, bool)> = vec![
        (vec![var("x"), var("y")], vec![s("a"), s("b")], true), // fresh vars
        (vec![var("x"), var("x")], vec![s("a"), s("a")], true), // repeated var, consistent
        (vec![var("x"), var("x")], vec![s("a"), s("b")], false), // repeated var, conflict
        (vec![lit("a")], vec![s("a")], true),                   // literal match
        (vec![lit("a")], vec![s("b")], false),                  // literal mismatch
        (vec![Term::Wildcard], vec![s("z")], true),             // wildcard
        (vec![Term::Actor], vec![s("alice")], true),            // actor match
        (vec![Term::Actor], vec![s("bob")], false),             // actor mismatch
        (vec![var("known")], vec![s("k")], true),               // agrees with base
        (vec![var("known")], vec![s("other")], false),          // conflicts with base
        (vec![var("x"), var("y")], vec![s("a")], false),        // arity mismatch never matches
    ];
    for (pats, vals, expect) in cases {
        let m = claim_matches(&pats, &vals, &base, Some(&actor));
        let u = unify_args(&pats, &vals, &base, Some(&actor));
        assert_eq!(m, u.is_some(), "verdicts disagree on {pats:?}");
        assert_eq!(m, expect, "wrong verdict on {pats:?}");
    }

    // A match extends the base with the new binding and keeps base entries.
    let u = unify_args(&[var("x")], &[s("a")], &base, Some(&actor)).unwrap();
    assert_eq!(u.get(&Var::from("x")), Some(&s("a")));
    assert_eq!(u.get(&Var::from("known")), Some(&s("k")));
}

// Evaluate a literal-only value expression against empty state/bindings.
fn eval_lit(e: &ValueExpr) -> Result<EvalValue, EvalError> {
    let state = State::default();
    let bindings = Bindings::new();
    let ctx = EvalContext::new(
        &state,
        None,
        &bindings,
        None,
        crate::definitions::DefinitionTable::new(&[]),
    );
    eval_value(e, &ctx)
}

#[test]
fn mul_multiplies_decimal_operands_exactly() {
    assert_eq!(
        eval_lit(&mul(term(dec("3")), term(dec("4")))).unwrap(),
        eval_lit(&term(dec("12"))).unwrap(),
    );
}

#[test]
fn div_divides_decimal_operands() {
    assert_eq!(
        eval_lit(&div(term(dec("12")), term(dec("4")))).unwrap(),
        eval_lit(&term(dec("3"))).unwrap(),
    );
}

#[test]
fn div_by_zero_surfaces_division_by_zero() {
    assert!(matches!(
        eval_lit(&div(term(dec("10")), term(dec("0")))),
        Err(EvalError::DivisionByZero)
    ));
}

#[test]
fn modulo_takes_the_decimal_remainder() {
    // 7 % 2 = 1 - the parity case the chess example relies on.
    assert_eq!(
        eval_lit(&modulo(term(dec("7")), term(dec("2")))).unwrap(),
        eval_lit(&term(dec("1"))).unwrap(),
    );
}

#[test]
fn modulo_by_zero_surfaces_division_by_zero() {
    assert!(matches!(
        eval_lit(&modulo(term(dec("10")), term(dec("0")))),
        Err(EvalError::DivisionByZero)
    ));
}

#[test]
fn modulo_rejects_non_decimal_operands() {
    assert!(matches!(
        eval_lit(&modulo(term(subj("x")), term(dec("2")))),
        Err(EvalError::TypeMismatch(_))
    ));
}

#[test]
fn mul_rejects_non_decimal_operands() {
    assert!(matches!(
        eval_lit(&mul(term(subj("x")), term(dec("2")))),
        Err(EvalError::TypeMismatch(_))
    ));
}

#[test]
fn min_takes_the_lesser_operand() {
    assert_eq!(
        eval_lit(&min(term(dec("3")), term(dec("4")))).unwrap(),
        eval_lit(&term(dec("3"))).unwrap(),
    );
}

#[test]
fn max_takes_the_greater_operand() {
    assert_eq!(
        eval_lit(&max(term(dec("3")), term(dec("4")))).unwrap(),
        eval_lit(&term(dec("4"))).unwrap(),
    );
}

#[test]
fn min_rejects_non_decimal_operands() {
    assert!(matches!(
        eval_lit(&min(term(subj("x")), term(dec("2")))),
        Err(EvalError::TypeMismatch(_))
    ));
}

// ValueOf: pins the single-indexed-pass behaviour that the
// `select_candidates` extraction shares with `find_claim_matches`.
// The double-entry example the bench uses has no ValueOf, so these
// are where the changed path's semantics are nailed down.

/// `Price(trade, amount)` claims for the given (trade, amount) rows.
fn price_state(rows: &[(&str, i64)]) -> State {
    State::from_claims(
        rows.iter()
            .map(|(t, amt)| ClaimInstance {
                predicate: "Price".into(),
                args: vec![
                    EvalValue::Subject(Subject::from(*t)),
                    EvalValue::Decimal(Decimal::new(*amt, 0)),
                ],
            })
            .collect(),
    )
}

fn eval_in(e: &ValueExpr, state: &State, actor: Option<&Subject>) -> Result<EvalValue, EvalError> {
    let bindings = Bindings::new();
    let ctx = EvalContext::new(
        state,
        None,
        &bindings,
        actor,
        crate::definitions::DefinitionTable::new(&[]),
    );
    eval_value(e, &ctx)
}

#[test]
fn value_of_single_match_returns_wildcard_value() {
    // Grounded arg 0 (`t1`) narrows via the argument-position index
    // (the `Indexed` candidate branch); the wildcard at arg 1 is the
    // value read back.
    let state = price_state(&[("t1", 100), ("t2", 200)]);
    assert_eq!(
        eval_in(
            &value_of("Price", vec![subj("t1"), wildcard()]),
            &state,
            None
        ),
        Ok(EvalValue::Decimal(Decimal::new(100, 0))),
    );
}

#[test]
fn value_of_full_scan_branch_single_match() {
    // No grounded arg, so `select_candidates` takes the `All` branch
    // (full predicate scan). A single claim resolves uniquely.
    let state = State::from_claims(vec![ClaimInstance {
        predicate: "Singleton".into(),
        args: vec![EvalValue::Decimal(Decimal::new(7, 0))],
    }]);
    assert_eq!(
        eval_in(&value_of("Singleton", vec![wildcard()]), &state, None),
        Ok(EvalValue::Decimal(Decimal::new(7, 0))),
    );
}

#[test]
fn value_of_zero_matches_uses_default() {
    let state = price_state(&[("t1", 100)]);
    assert_eq!(
        eval_in(
            &value_of_with_default("Price", vec![subj("absent"), wildcard()], term(dec("42")),),
            &state,
            None,
        ),
        Ok(EvalValue::Decimal(Decimal::new(42, 0))),
    );
}

#[test]
fn value_of_zero_matches_without_default_errors() {
    let state = price_state(&[("t1", 100)]);
    assert_eq!(
        eval_in(
            &value_of("Price", vec![subj("absent"), wildcard()]),
            &state,
            None
        ),
        Err(EvalError::ValueOfZeroMatches("Price".to_string())),
    );
}

#[test]
fn value_of_multiple_matches_errors() {
    // Two Price claims share arg 0 `t1`, so the wildcard at arg 1
    // matches both - the functional-lookup contract is violated.
    let state = price_state(&[("t1", 100), ("t1", 200)]);
    assert_eq!(
        eval_in(
            &value_of("Price", vec![subj("t1"), wildcard()]),
            &state,
            None
        ),
        Err(EvalError::ValueOfMultipleMatches("Price".to_string())),
    );
}

#[test]
fn value_of_unbound_actor_errors_position_independently() {
    // A selective ground arg before `actor` would short-circuit to
    // "no matches" first; the up-front actor check in
    // `select_candidates` must still surface `UnboundActor` when no
    // actor is in scope.
    let state = price_state(&[("t1", 100)]);
    let e = value_of("Triple", vec![subj("absent"), Term::Actor, wildcard()]);
    assert_eq!(eval_in(&e, &state, None), Err(EvalError::UnboundActor));
}

// matching_claims: the retract path's claim lookup. Same indexed
// narrowing as find_claim_matches, returning the matched claims.

fn matched_for(state: &State, args: Vec<Term>) -> Vec<ClaimInstance> {
    let bindings = Bindings::new();
    let ctx = EvalContext::new(
        state,
        None,
        &bindings,
        None,
        crate::definitions::DefinitionTable::new(&[]),
    );
    matching_claims(&"Price".into(), &args, &ctx).expect("matching_claims")
}

#[test]
fn matching_claims_narrows_by_ground_arg() {
    // Ground arg 0 selects only that subject's claims (the Indexed
    // branch); the wildcard at arg 1 does not constrain.
    let state = price_state(&[("t1", 100), ("t1", 150), ("t2", 200)]);
    let matched = matched_for(&state, vec![subj("t1"), wildcard()]);
    assert_eq!(matched.len(), 2);
    assert!(
        matched
            .iter()
            .all(|c| c.args[0] == EvalValue::Subject(Subject::from("t1")))
    );
}

#[test]
fn matching_claims_full_scan_all_wildcards() {
    // No ground arg: the All branch returns every claim of the
    // predicate.
    let state = price_state(&[("t1", 100), ("t2", 200)]);
    assert_eq!(matched_for(&state, vec![wildcard(), wildcard()]).len(), 2);
}

#[test]
fn matching_claims_no_match_is_empty() {
    let state = price_state(&[("t1", 100)]);
    assert!(matched_for(&state, vec![subj("absent"), wildcard()]).is_empty());
}

#[test]
fn matching_claims_unbound_actor_errors() {
    // `select_candidates`' up-front actor check applies on the
    // retract path too: a `Term::Actor` arg with no actor in scope
    // is an error, not a silent no-match.
    let state = price_state(&[("t1", 100)]);
    let bindings = Bindings::new();
    let ctx = EvalContext::new(
        &state,
        None,
        &bindings,
        None,
        crate::definitions::DefinitionTable::new(&[]),
    );
    assert_eq!(
        matching_claims(&"Price".into(), &[Term::Actor, wildcard()], &ctx),
        Err(EvalError::UnboundActor),
    );
}
