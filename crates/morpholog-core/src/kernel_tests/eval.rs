//! The evaluator: literals, claim lookup, footprint walkers, arithmetic,
//! comparators, `or`/`xor`, and `pre`.

use super::*;

#[test]
fn decimal_literal_constructs() {
    let v = Value::Decimal("1250.75".to_string());
    assert_eq!(
        Term::Literal(v),
        Term::Literal(Value::Decimal("1250.75".to_string()))
    );
}

#[test]
fn subject_literal_constructs_and_resolves() {
    let v = Value::Subject("bank_debt_service".into());
    assert_eq!(
        Term::Literal(v),
        Term::Literal(Value::Subject("bank_debt_service".into()))
    );
    let resolved = resolve_term(
        &Term::Literal(Value::Subject("bank_debt_service".into())),
        &Bindings::new(),
        None,
    )
    .unwrap();
    assert_eq!(resolved, EvalValue::Subject("bank_debt_service".into()));
}

#[test]
fn subject_literal_unifies_with_matching_subject_arg() {
    let pattern = vec![Term::Literal(Value::Subject("p1".into()))];
    let value = vec![EvalValue::Subject("p1".into())];
    assert!(unify_args(&pattern, &value, &Bindings::new(), None).is_some());

    let mismatch = vec![EvalValue::Subject("p2".into())];
    assert!(unify_args(&pattern, &mismatch, &Bindings::new(), None).is_none());

    let wrong_kind = vec![EvalValue::Decimal(Decimal::new(1, 0))];
    assert!(unify_args(&pattern, &wrong_kind, &Bindings::new(), None).is_none());
}

/// `State::claims_for` returns only claims of the named predicate,
/// args intact; nothing for a predicate with no claims; and leaves
/// `State::claims` in construction order.
#[test]
fn claims_for_returns_only_matching_predicate() {
    let a1 = ClaimInstance {
        predicate: "A".into(),
        args: vec![EvalValue::Subject("a1".into())],
    };
    let b1 = ClaimInstance {
        predicate: "B".into(),
        args: vec![EvalValue::Decimal(Decimal::new(42, 0))],
    };
    let a2 = ClaimInstance {
        predicate: "A".into(),
        args: vec![EvalValue::Subject("a2".into())],
    };
    let state = State::from_claims(vec![a1.clone(), b1.clone(), a2.clone()]);

    let a_rows: Vec<&ClaimInstance> = state.claims_for("A").collect();
    assert_eq!(a_rows.len(), 2, "two A claims admitted");
    assert!(a_rows.iter().all(|c| c.predicate.as_str() == "A"));
    assert!(a_rows.contains(&&a1));
    assert!(a_rows.contains(&&a2));

    let b_rows: Vec<&ClaimInstance> = state.claims_for("B").collect();
    assert_eq!(b_rows, vec![&b1], "single B claim, args intact");

    let absent: Vec<&ClaimInstance> = state.claims_for("Nope").collect();
    assert!(
        absent.is_empty(),
        "no claims for an unknown predicate, not an error"
    );

    assert_eq!(
        state.claims().to_vec(),
        vec![a1, b1, a2],
        "claims() preserves construction order across all predicates"
    );
}

/// `State::claim_candidates` yields the named predicate's claims
/// whose argument at the position equals the value; `None` (not an
/// empty bucket) when there are none; and never claims of another
/// predicate sharing that value.
#[test]
fn claim_candidates_narrow_by_predicate_position_and_value() {
    let line_for_entry_a = ClaimInstance {
        predicate: "JournalLine".into(),
        args: vec![
            EvalValue::Subject("entry_a".into()),
            EvalValue::Subject("account_cash".into()),
        ],
    };
    let line_for_entry_b = ClaimInstance {
        predicate: "JournalLine".into(),
        args: vec![
            EvalValue::Subject("entry_b".into()),
            EvalValue::Subject("account_cash".into()),
        ],
    };
    // Same value at position 0 but different predicate; must not
    // pollute the JournalLine[0=entry_a] bucket.
    let je_for_entry_a = ClaimInstance {
        predicate: "JournalEntry".into(),
        args: vec![EvalValue::Subject("entry_a".into())],
    };
    let state = State::from_claims(vec![
        line_for_entry_a.clone(),
        line_for_entry_b.clone(),
        je_for_entry_a.clone(),
    ]);

    let entry_a = EvalValue::Subject("entry_a".into());
    let claims: Vec<&ClaimInstance> = state
        .claim_candidates(&"JournalLine".into(), 0, &entry_a)
        .expect("entry_a appears at JournalLine[0]")
        .iter()
        .collect();
    assert_eq!(
        claims,
        vec![&line_for_entry_a],
        "must return only the JournalLine claim, not JournalEntry"
    );

    let unknown = EvalValue::Subject("entry_z".into());
    assert!(
        state
            .claim_candidates(&"JournalLine".into(), 0, &unknown)
            .is_none(),
        "absent value returns None, signalling empty intersection"
    );

    let cash = EvalValue::Subject("account_cash".into());
    let cash_bucket = state
        .claim_candidates(&"JournalLine".into(), 1, &cash)
        .expect("account_cash appears at JournalLine[1]");
    assert_eq!(
        cash_bucket.iter().count(),
        2,
        "both JournalLine claims share account_cash at position 1"
    );
}

/// `predicates_referenced_by_prop` finds a uniquely named predicate
/// planted in every variant that nests a `Prop` or `Claim`.
/// Comparator operands reach theirs through `Sum`/`ValueOf`.
#[test]
fn predicates_referenced_by_prop_covers_every_variant() {
    let claim = |p: &str| Prop::Claim {
        predicate: p.into(),
        args: vec![],
    };
    // A value expression that plants one predicate name (a `Sum`
    // whose body is a claim), to reach the comparator-operand path.
    let value_with = |p: &str| ValueExpr::Sum {
        value: Box::new(Term::Var("v".into()).into()),
        body: Box::new(claim(p)),
        seed: SumSeed::default(),
    };

    let prop = Prop::And(vec![
        Prop::Implies {
            left: Box::new(claim("P_implies_left")),
            right: Box::new(claim("P_implies_right")),
        },
        Prop::Exists {
            binding: "x".into(),
            body: Box::new(claim("P_exists_body")),
        },
        Prop::Not(Box::new(claim("P_not_body"))),
        Prop::Eq(
            Box::new(value_with("P_eq_left")),
            Box::new(value_with("P_eq_right")),
        ),
        le_(
            Box::new(value_with("P_le_left")),
            Box::new(value_with("P_le_right")),
        ),
        date_le_(
            Box::new(value_with("P_datele_left")),
            Box::new(value_with("P_datele_right")),
        ),
        Prop::Forall {
            binding: "y".into(),
            source: Box::new(claim("P_forall_source")),
            body: Box::new(claim("P_forall_body")),
        },
        Prop::Or(vec![claim("P_or_left"), claim("P_or_right")]),
        Prop::Pre(Box::new(claim("P_pre_inner"))),
        // Variants with no predicate references contribute nothing.
        Prop::Neq(
            Box::new(ValueExpr::Term(Term::Var("a".into()))),
            Box::new(ValueExpr::Term(Term::Var("b".into()))),
        ),
        Prop::In(Term::Var("e".into()), Term::Var("coll".into())),
    ]);

    let mut got = BTreeSet::new();
    predicates_referenced_by_prop(&prop, &[], &mut got);

    let expected: BTreeSet<PredicateName> = [
        "P_implies_left",
        "P_implies_right",
        "P_exists_body",
        "P_not_body",
        "P_eq_left",
        "P_eq_right",
        "P_le_left",
        "P_le_right",
        "P_datele_left",
        "P_datele_right",
        "P_forall_source",
        "P_forall_body",
        "P_or_left",
        "P_or_right",
        "P_pre_inner",
    ]
    .iter()
    .map(|s| PredicateName::from(*s))
    .collect();

    assert_eq!(
        got, expected,
        "every Prop variant that carries a predicate reference must contribute it"
    );
}

/// `predicates_referenced_by_value` finds a predicate planted in each
/// `ValueExpr` variant that can nest one (`ValueOf`, a `Sum` body, an
/// arithmetic operand). `Term` carries none.
#[test]
fn predicates_referenced_by_value_covers_every_variant() {
    let claim = |p: &str| Prop::Claim {
        predicate: p.into(),
        args: vec![],
    };
    let value_of = |p: &str, default: Option<ValueExpr>| ValueExpr::ValueOf {
        predicate: p.into(),
        args: vec![Term::Wildcard],
        extract: 0,
        default: default.map(Box::new),
    };

    let expr = ValueExpr::Arith {
        op: ArithOp::Add,
        left: Box::new(ValueExpr::Arith {
            op: ArithOp::Sub,
            left: Box::new(ValueExpr::Sum {
                value: Box::new(Term::Var("v".into()).into()),
                body: Box::new(claim("P_sum_body")),
                seed: SumSeed::default(),
            }),
            right: Box::new(value_of(
                "P_valueof_self",
                Some(value_of("P_valueof_default", None)),
            )),
        }),
        // A bare term carries no predicate reference.
        right: Box::new(ValueExpr::Term(Term::Var("z".into()))),
    };

    let mut got = BTreeSet::new();
    analysis::predicates_referenced_by_value(&expr, &[], &mut got);

    let expected: BTreeSet<PredicateName> = ["P_sum_body", "P_valueof_self", "P_valueof_default"]
        .iter()
        .map(|s| PredicateName::from(*s))
        .collect();

    assert_eq!(
        got, expected,
        "every ValueExpr variant that carries a predicate reference must contribute it"
    );
}

/// `predicates_read_by_stmt` includes every predicate the
/// statement reads from pre-state (Require, BindOne, Let value,
/// For collection + body, Retract pattern) and excludes
/// `Stmt::Assert`'s output predicate.
#[test]
fn predicates_read_by_stmt_excludes_assert_includes_retract_and_reads() {
    use ir_builder::*;
    let body = vec![
        require(claim("P_require", vec![var("x")])),
        bind_one(claim("P_bind", vec![var("y"), var("z")])),
        let_("v", value_of("P_let", vec![var("y"), wildcard()])),
        // Writes only: P_assert MUST NOT appear in the read set.
        assert_("P_assert", vec![var("y")]),
        retract("P_retract", vec![wildcard()]),
        for_(
            "i",
            term(var("xs")),
            vec![require(claim("P_for_inner", vec![var("i")]))],
        ),
        emit("Notified", vec![var("y")]),
    ];
    let mut got = BTreeSet::new();
    for stmt in &body {
        predicates_read_by_stmt(stmt, &[], &mut got);
    }
    let expected: BTreeSet<PredicateName> =
        ["P_require", "P_bind", "P_let", "P_retract", "P_for_inner"]
            .iter()
            .map(|s| PredicateName::from(*s))
            .collect();
    assert_eq!(
        got, expected,
        "read-set must include every pre-state read and exclude Stmt::Assert's output"
    );
}

/// `ArithOp::Add` returns the decimal sum of its operands when both
/// evaluate to decimals.
#[test]
fn add_sums_two_decimals() {
    let expr = ValueExpr::Arith {
        op: ArithOp::Add,
        left: Box::new(ValueExpr::Term(Term::Literal(Value::Decimal(
            "10".to_string(),
        )))),
        right: Box::new(ValueExpr::Term(Term::Literal(Value::Decimal(
            "32.5".to_string(),
        )))),
    };
    let v = eval_value(&expr, &ctx(&State::from_claims(vec![]), &Bindings::new())).unwrap();
    assert_eq!(v, EvalValue::Decimal(Decimal::new(425, 1)));
}

/// Non-decimal operands surface as `TypeMismatch` rather than
/// falling through silently. Same contract as `Sub`.
#[test]
fn add_with_non_decimal_operand_is_type_mismatch() {
    let expr = ValueExpr::Arith {
        op: ArithOp::Add,
        left: Box::new(ValueExpr::Term(Term::Literal(Value::Decimal(
            "10".to_string(),
        )))),
        right: Box::new(ValueExpr::Term(Term::Literal(Value::Subject(
            "oops".into(),
        )))),
    };
    let err = eval_value(&expr, &ctx(&State::from_claims(vec![]), &Bindings::new()))
        .expect_err("expected TypeMismatch");
    match err {
        EvalError::TypeMismatch(msg) => assert!(msg.contains("Add")),
        other => panic!("expected TypeMismatch, got {other:?}"),
    }
}

fn date_lit(s: &str) -> ValueExpr {
    ValueExpr::Term(Term::Literal(Value::Date(s.to_string())))
}

/// `DateLe(a, b)` admits `a <= b` with bindings unchanged, like
/// decimal `Le`; `a > b` is a plain no-match, not `TypeMismatch`.
/// Equal dates admit: validity windows are inclusive.
#[test]
fn date_le_pins_direction_and_inclusivity() {
    let cases = [
        ("2026-03-11", "2026-03-12", 1), // earlier admits
        ("2026-03-12", "2026-03-12", 1), // equal admits: inclusive window
        ("2026-03-13", "2026-03-12", 0), // later is a lawful no-match
    ];
    for (lhs, rhs, expected) in cases {
        let expr = date_le_(Box::new(date_lit(lhs)), Box::new(date_lit(rhs)));
        let matches =
            find_matches(&expr, &ctx(&State::from_claims(vec![]), &Bindings::new())).unwrap();
        assert_eq!(matches.len(), expected, "DateLe({lhs}, {rhs})");
    }
}

/// Mixed operand kinds on either side raise `TypeMismatch`, not a
/// silent no-match. A malformed `Value::Date` string fails the same
/// way at evaluation time, like an invalid `Value::Decimal`.
#[test]
fn date_le_operand_type_guards_raise_type_mismatch() {
    let dec_lit = ValueExpr::Term(Term::Literal(Value::Decimal("1".to_string())));
    let subj_lit = ValueExpr::Term(Term::Literal(Value::Subject("oops".into())));
    let cases = [
        (dec_lit, date_lit("2026-03-12"), "civil-date"),
        (date_lit("2026-03-12"), subj_lit, "civil-date"),
        (
            date_lit("not-a-date"),
            date_lit("2026-03-12"),
            "invalid civil date",
        ),
    ];
    for (lhs, rhs, fragment) in cases {
        let expr = date_le_(Box::new(lhs), Box::new(rhs));
        let err = find_matches(&expr, &ctx(&State::from_claims(vec![]), &Bindings::new()))
            .expect_err("mixed or malformed operands must be a TypeMismatch");
        match err {
            EvalError::TypeMismatch(msg) => {
                assert!(msg.contains(fragment), "msg was: {msg}")
            }
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }
}

/// The decimal strict/inclusive comparators admit in the right
/// direction: `Gt`/`Lt` are strict, `Ge` includes equality.
#[test]
fn decimal_strict_comparators_pin_direction() {
    let d = |s: &str| {
        Box::new(ValueExpr::Term(Term::Literal(Value::Decimal(
            s.to_string(),
        ))))
    };
    let admits = |e: Prop| {
        !find_matches(&e, &ctx(&State::from_claims(vec![]), &Bindings::new()))
            .unwrap()
            .is_empty()
    };
    assert!(admits(gt_(d("5"), d("3"))));
    assert!(!admits(gt_(d("3"), d("5"))));
    assert!(!admits(gt_(d("3"), d("3"))));
    assert!(admits(lt_(d("3"), d("5"))));
    assert!(!admits(lt_(d("3"), d("3"))));
    assert!(admits(ge_(d("3"), d("3"))));
    assert!(!admits(ge_(d("3"), d("5"))));
}

/// The civil-date comparators mirror the decimal ones: `before`
/// (`DateLt`) and `after` (`DateGt`) are strict, `on_or_after`
/// (`DateGe`) includes equality.
#[test]
fn date_strict_comparators_pin_direction() {
    let admits = |e: Prop| {
        !find_matches(&e, &ctx(&State::from_claims(vec![]), &Bindings::new()))
            .unwrap()
            .is_empty()
    };
    assert!(admits(date_lt_(
        Box::new(date_lit("2026-01-01")),
        Box::new(date_lit("2026-06-01")),
    )));
    assert!(!admits(date_lt_(
        Box::new(date_lit("2026-06-01")),
        Box::new(date_lit("2026-06-01")),
    )));
    assert!(admits(date_gt_(
        Box::new(date_lit("2026-06-01")),
        Box::new(date_lit("2026-01-01")),
    )));
    assert!(admits(date_ge_(
        Box::new(date_lit("2026-06-01")),
        Box::new(date_lit("2026-06-01")),
    )));
}

/// A `Value::Date` literal in a `claim` argument matches a claim
/// admitted with the same date in that position.
#[test]
fn date_literal_unifies_with_matching_date_arg() {
    let claim = ClaimInstance {
        predicate: "OnDate".into(),
        args: vec![EvalValue::Date(
            "2026-03-12".parse::<Date>().expect("hand-built ISO date"),
        )],
    };
    let state = State::from_claims(vec![claim]);
    let expr = Prop::Claim {
        predicate: "OnDate".into(),
        args: vec![Term::Literal(Value::Date("2026-03-12".to_string()))],
    };
    let matches = find_matches(&expr, &ctx(&state, &Bindings::new())).unwrap();
    assert_eq!(matches.len(), 1, "literal date arg must unify");

    let other = Prop::Claim {
        predicate: "OnDate".into(),
        args: vec![Term::Literal(Value::Date("2026-03-13".to_string()))],
    };
    let none = find_matches(&other, &ctx(&state, &Bindings::new())).unwrap();
    assert!(
        none.is_empty(),
        "literal date arg must not unify with a different date"
    );
}

/// The cumulative cap: an addition inside a `<=` comparison
/// (`running + proposed <= cap`).
#[test]
fn add_nests_under_le_for_cumulative_cap() {
    let running = ValueExpr::Term(Term::Literal(Value::Decimal("60".to_string())));
    let proposed = ValueExpr::Term(Term::Literal(Value::Decimal("40".to_string())));
    let cap = ValueExpr::Term(Term::Literal(Value::Decimal("100".to_string())));

    // 60 + 40 <= 100 admits (binding pass-through).
    let under_cap = le_(
        Box::new(ValueExpr::Arith {
            op: ArithOp::Add,
            left: Box::new(running.clone()),
            right: Box::new(proposed),
        }),
        Box::new(cap.clone()),
    );
    let matches = find_matches(
        &under_cap,
        &ctx(&State::from_claims(vec![]), &Bindings::new()),
    )
    .unwrap();
    assert_eq!(matches.len(), 1, "60 + 40 <= 100 should admit");

    // 60 + 50 <= 100 fails (empty match set).
    let over_cap = le_(
        Box::new(ValueExpr::Arith {
            op: ArithOp::Add,
            left: Box::new(running),
            right: Box::new(ValueExpr::Term(Term::Literal(Value::Decimal(
                "50".to_string(),
            )))),
        }),
        Box::new(cap),
    );
    let matches = find_matches(
        &over_cap,
        &ctx(&State::from_claims(vec![]), &Bindings::new()),
    )
    .unwrap();
    assert!(matches.is_empty(), "60 + 50 <= 100 should reject");
}

/// `Prop::Or` concatenates each branch's binding sets without
/// deduplication, like `find_conjunction`. Covers one branch, both
/// branches, neither, and a branch adding a fresh binding.
#[test]
fn or_returns_union_of_branch_binding_sets() {
    // State holds two A claims and one B claim. Different keys per
    // predicate so a branch's extensions are distinguishable.
    let state = State::from_claims(vec![
        ClaimInstance {
            predicate: "A".into(),
            args: vec![EvalValue::Subject("a1".into())],
        },
        ClaimInstance {
            predicate: "A".into(),
            args: vec![EvalValue::Subject("a2".into())],
        },
        ClaimInstance {
            predicate: "B".into(),
            args: vec![EvalValue::Subject("b1".into())],
        },
    ]);

    let a_x = Prop::Claim {
        predicate: "A".into(),
        args: vec![Term::Var("x".into())],
    };
    let b_x = Prop::Claim {
        predicate: "B".into(),
        args: vec![Term::Var("x".into())],
    };
    let c_x = Prop::Claim {
        predicate: "C".into(),
        args: vec![Term::Var("x".into())],
    };

    // Both branches match: two A extensions + one B extension = 3.
    let both = Prop::Or(vec![a_x.clone(), b_x.clone()]);
    let matches = find_matches(&both, &ctx(&state, &Bindings::new())).unwrap();
    assert_eq!(
        matches.len(),
        3,
        "Or must concatenate every branch's binding extensions"
    );
    let bound_x: Vec<_> = matches
        .iter()
        .map(
            |b| match b.get(&Var::from("x")).expect("x bound in every extension") {
                EvalValue::Subject(s) => s.as_str().to_string(),
                _ => panic!("x must be a subject"),
            },
        )
        .collect();
    assert!(bound_x.contains(&"a1".to_string()));
    assert!(bound_x.contains(&"a2".to_string()));
    assert!(bound_x.contains(&"b1".to_string()));

    // One branch matches, one doesn't: only the matching branch's
    // extensions are returned.
    let one_matches = Prop::Or(vec![a_x.clone(), c_x.clone()]);
    let matches = find_matches(&one_matches, &ctx(&state, &Bindings::new())).unwrap();
    assert_eq!(
        matches.len(),
        2,
        "Or with one empty branch returns the other branch's matches"
    );

    // Neither branch matches: empty.
    let none = Prop::Or(vec![c_x.clone(), c_x]);
    let matches = find_matches(&none, &ctx(&state, &Bindings::new())).unwrap();
    assert!(
        matches.is_empty(),
        "Or with every branch empty produces an empty result"
    );

    // No deduplication: two branches admitting the same extension
    // appear twice, matching find_conjunction's convention.
    let dup = Prop::Or(vec![a_x.clone(), a_x]);
    let matches = find_matches(&dup, &ctx(&state, &Bindings::new())).unwrap();
    assert_eq!(
        matches.len(),
        4,
        "Or preserves multiplicity; identical branches double-count"
    );
}

/// `Prop::Xor` holds exactly when one operand matches and the other
/// does not. Covers all four cases with ground operands.
#[test]
fn xor_holds_for_exactly_one_operand() {
    let l = Prop::Claim {
        predicate: "L".into(),
        args: vec![],
    };
    let r = Prop::Claim {
        predicate: "R".into(),
        args: vec![],
    };
    let xor = Prop::Xor(Box::new(l), Box::new(r));

    let l_claim = ClaimInstance {
        predicate: "L".into(),
        args: vec![],
    };
    let r_claim = ClaimInstance {
        predicate: "R".into(),
        args: vec![],
    };

    let holds = |claims: Vec<ClaimInstance>| {
        !find_matches(&xor, &ctx(&State::from_claims(claims), &Bindings::new()))
            .unwrap()
            .is_empty()
    };

    assert!(holds(vec![l_claim.clone()]), "left only: xor holds");
    assert!(holds(vec![r_claim.clone()]), "right only: xor holds");
    assert!(
        !holds(vec![l_claim.clone(), r_claim]),
        "both: xor fails (not exclusive)"
    );
    assert!(!holds(vec![]), "neither: xor fails");
}

// ============================================================
// Prop::Pre - pre-state opt-in
// ============================================================

/// `pre(inner)` flips state lookup: the inner expression sees the
/// pre-state, the outer sees the candidate.
#[test]
fn pre_flips_predicate_lookup_to_pre_state() {
    // pre_state has Counter(1); post (candidate) has Counter(2).
    let pre = State::from_claims(vec![ClaimInstance {
        predicate: "Counter".into(),
        args: vec![EvalValue::Decimal(rust_decimal::Decimal::from(1))],
    }]);
    let post = State::from_claims(vec![ClaimInstance {
        predicate: "Counter".into(),
        args: vec![EvalValue::Decimal(rust_decimal::Decimal::from(2))],
    }]);

    // Counter(n) and pre(Counter(m)) implies n = m + 1
    let body = Prop::Implies {
        left: Box::new(Prop::And(vec![
            Prop::Claim {
                predicate: "Counter".into(),
                args: vec![Term::Var("n".into())],
            },
            Prop::Pre(Box::new(Prop::Claim {
                predicate: "Counter".into(),
                args: vec![Term::Var("m".into())],
            })),
        ])),
        right: Box::new(Prop::Eq(
            Box::new(ValueExpr::Term(Term::Var("n".into()))),
            Box::new(ValueExpr::Arith {
                op: ArithOp::Add,
                left: Box::new(ValueExpr::Term(Term::Var("m".into()))),
                right: Box::new(ValueExpr::Term(Term::Literal(Value::Decimal(
                    "1".to_string(),
                )))),
            }),
        )),
    };

    let matches = find_matches(&body, &ctx_with_pre(&post, &pre, &Bindings::new())).unwrap();
    assert_eq!(
        matches.len(),
        1,
        "Counter(2) and pre(Counter(1)) implies 2 = 1 + 1 should hold"
    );

    // Now post has Counter(5): with pre still Counter(1), the
    // rule `n = m + 1` reduces to `5 = 2`, which must reject.
    let bad_post = State::from_claims(vec![ClaimInstance {
        predicate: "Counter".into(),
        args: vec![EvalValue::Decimal(rust_decimal::Decimal::from(5))],
    }]);
    let matches = find_matches(&body, &ctx_with_pre(&bad_post, &pre, &Bindings::new())).unwrap();
    assert!(
        matches.is_empty(),
        "Counter(5) and pre(Counter(1)) implies 5 = 1 + 1 should reject"
    );
}

/// `Prop::Pre` with no pre-state in scope errors
/// `PreStateUnavailable`, so derived claims, `require` bodies, and
/// standalone evaluator callers cannot use `pre()`.
#[test]
fn pre_without_pre_state_errors_pre_state_unavailable() {
    let post = State::from_claims(vec![]);
    let body = Prop::Pre(Box::new(Prop::Claim {
        predicate: "Anything".into(),
        args: vec![],
    }));
    let err = find_matches(&body, &ctx(&post, &Bindings::new())).expect_err("must error");
    assert!(matches!(err, EvalError::PreStateUnavailable), "got {err:?}");
}

/// Nested `pre(pre(x))` is also unavailable - the inner subtree
/// inherits a cleared pre slot, so a second `Pre` finds nothing
/// to swap into.
#[test]
fn nested_pre_errors_pre_state_unavailable() {
    let pre = State::from_claims(vec![]);
    let post = State::from_claims(vec![]);
    let body = Prop::Pre(Box::new(Prop::Pre(Box::new(Prop::Claim {
        predicate: "Anything".into(),
        args: vec![],
    }))));
    let err = find_matches(&body, &ctx_with_pre(&post, &pre, &Bindings::new()))
        .expect_err("nested pre must error");
    assert!(matches!(err, EvalError::PreStateUnavailable), "got {err:?}");
}

/// `pre(forall x in S: body)` and `forall x in S: pre(body)` differ
/// when `S` changes. With `S` only in post, the first ranges over
/// nothing (vacuously true), the second over the post-state members.
#[test]
fn pre_outside_forall_vs_inside_distinguish_iteration_domain() {
    let pre = State::from_claims(vec![]);
    let post = State::from_claims(vec![
        ClaimInstance {
            predicate: "Account".into(),
            args: vec![EvalValue::Subject("a1".into())],
        },
        ClaimInstance {
            predicate: "Balance".into(),
            args: vec![EvalValue::Subject("a1".into())],
        },
    ]);

    // `pre(forall a in Account: Balance(a))`: in pre-state there
    // are no Account claims, so the source is empty and the
    // body is vacuously satisfied.
    let outside = Prop::Pre(Box::new(Prop::Forall {
        binding: "a".into(),
        source: Box::new(Prop::Claim {
            predicate: "Account".into(),
            args: vec![Term::Var("a".into())],
        }),
        body: Box::new(Prop::Claim {
            predicate: "Balance".into(),
            args: vec![Term::Var("a".into())],
        }),
    }));
    let matches = find_matches(&outside, &ctx_with_pre(&post, &pre, &Bindings::new())).unwrap();
    assert!(
        !matches.is_empty(),
        "pre(forall over empty pre-state Account) is vacuously true"
    );

    // `forall a in Account: pre(Balance(a))`: iterates the
    // single post-state Account, and asks whether Balance(a)
    // held in pre. Pre has no Balance, so the body fails for
    // the iterated a.
    let inside = Prop::Forall {
        binding: "a".into(),
        source: Box::new(Prop::Claim {
            predicate: "Account".into(),
            args: vec![Term::Var("a".into())],
        }),
        body: Box::new(Prop::Pre(Box::new(Prop::Claim {
            predicate: "Balance".into(),
            args: vec![Term::Var("a".into())],
        }))),
    };
    let matches = find_matches(&inside, &ctx_with_pre(&post, &pre, &Bindings::new())).unwrap();
    assert!(
        matches.is_empty(),
        "forall over post Account where body asks pre(Balance) must fail when pre has no Balance"
    );
}
