//! Canonical encoding 1, the bytes `sha256:` names, frozen over one carrier
//! programme built directly as IR. Built after parsing, so a parser change
//! cannot move it and an example edit cannot excuse it: if this fails, the
//! encoder changed what an existing programme hashes to. That is never
//! regenerated under the same prefix; a new construct may only add
//! rendering for itself.

use crate::format::{canonical_hash, canonical_preimage};
use crate::ir::{
    ArgDecl, ArithOp, CompareOp, DefinitionOrigin, DerivedClaim, DerivedValue, Discipline,
    ExtremumOp, IntentDecl, OrderedDomain, PredicateArgKind, Program, Prop, SumSeed, Unit,
    ValueExpr,
};
use crate::ir_builder::*;

const PREIMAGE: &str = include_str!("../../tests/golden/canonical_encoding_v1.txt");
const HASH: &str = "sha256:ee3ed284180fc4021c60c350268ec262dc4bd78f5d50ac4f3076b1e29e76f2f6";

fn compare(op: CompareOp, domain: OrderedDomain, l: ValueExpr, r: ValueExpr) -> Prop {
    Prop::Compare {
        op,
        domain,
        left: Box::new(l),
        right: Box::new(r),
    }
}

fn arith(op: ArithOp, l: ValueExpr, r: ValueExpr) -> ValueExpr {
    ValueExpr::Arith {
        op,
        left: Box::new(l),
        right: Box::new(r),
    }
}

fn v(name: &str) -> ValueExpr {
    term(var(name))
}

/// Every declaration kind, discipline, proposition, value, builtin,
/// operator, ordered domain, sum seed, statement and literal the IR had
/// when encoding 1 was frozen. A later construct adds its own carrier.
fn encoding_v1_carrier() -> Program {
    let predicates = vec![
        predicate("Account")
            .subject("id")
            .decimal("balance")
            .disciplines(vec![Discipline::UniqueBy {
                fields: vec!["id".to_string()],
            }])
            .build(),
        predicate("Entry")
            .subject("id")
            .date("on")
            .disciplines(vec![Discipline::AppendOnly])
            .build(),
        predicate("Ping")
            .subject("id")
            .timestamp("at")
            .duration("gap")
            .build(),
        predicate("Charge")
            .subject("id")
            .quantity("amount", "USD")
            .build(),
        predicate("Rate")
            .subject("id")
            .decimal("rate")
            .disciplines(vec![
                Discipline::CurrentPointerBy {
                    fields: vec!["id".to_string()],
                },
                Discipline::SupersededVia {
                    lineage: "Supersedes".into(),
                },
            ])
            .build(),
        predicate("Supersedes")
            .subject("successor")
            .subject("prior")
            .build(),
        predicate("Window")
            .subject("id")
            .date("from")
            .decimal("value")
            .disciplines(vec![Discipline::EffectiveBy {
                keys: vec!["id".to_string()],
                on: "from".to_string(),
                partial: true,
            }])
            .build(),
        predicate("Tariff")
            .subject("id")
            .date("from")
            .decimal("value")
            .disciplines(vec![Discipline::EffectiveBy {
                keys: vec!["id".to_string()],
                on: "from".to_string(),
                partial: false,
            }])
            .build(),
        predicate("Flag").subject("id").boolean("ok").build(),
        predicate("Bag")
            .subject("id")
            .collection("items")
            .any("tag")
            .build(),
        predicate("Total").subject("id").decimal("sum").build(),
    ];
    let intents = vec![IntentDecl {
        name: "Notify".into(),
        args: vec![
            ArgDecl {
                name: "id".to_string(),
                kind: PredicateArgKind::Subject,
            },
            ArgDecl {
                name: "amount".to_string(),
                kind: PredicateArgKind::Decimal,
            },
        ],
    }];
    let definitions = vec![crate::ir::Definition {
        origin: DefinitionOrigin::default(),
        ..definition(
            "funded",
            vec!["a".into()],
            exists(
                "b",
                and(vec![
                    claim("Account", vec![var("a"), var("b")]),
                    le(term(dec("0")), v("b")),
                ]),
            ),
        )
    }];
    let derived = vec![DerivedClaim {
        predicate: "Total".into(),
        keys: vec!["id".into()],
        values: vec![DerivedValue {
            name: "sum".to_string(),
            expr: sum(var("x"), claim("Account", vec![var("id"), var("x")])),
        }],
        domain: claim("Account", vec![var("id"), wildcard()]),
    }];
    let propositions = invariant(
        "propositions",
        and(vec![
            or(vec![
                claim("Flag", vec![var("f"), wildcard()]),
                not(claim("Entry", vec![var("f"), wildcard()])),
            ]),
            implies(
                claim("Account", vec![var("a"), var("n")]),
                forall(
                    "e",
                    claim("Entry", vec![var("e"), wildcard()]),
                    xor(
                        exists("p", claim("Ping", vec![var("p"), wildcard(), wildcard()])),
                        pre(claim("Rate", vec![var("a"), wildcard()])),
                    ),
                ),
            ),
            eq(v("n"), term(dec("1.50"))),
            neq(var("n"), dec("-2")),
            in_(var("a"), var("bag")),
            defined("funded", vec![var("a")]),
        ]),
    );
    let mut total = invariant(
        "tariffs",
        forall(
            "t",
            claim("Account", vec![var("t"), wildcard()]),
            exists("d", claim("Tariff", vec![var("t"), var("d"), wildcard()])),
        ),
    );
    total.totality_for = Some("Tariff".into());
    let orderings = invariant(
        "orderings",
        and(vec![
            compare(
                CompareOp::Le,
                OrderedDomain::Decimal,
                v("x"),
                term(dec("10")),
            ),
            compare(
                CompareOp::Lt,
                OrderedDomain::Date,
                v("d"),
                term(date("2026-04-01")),
            ),
            compare(
                CompareOp::Ge,
                OrderedDomain::Timestamp,
                v("t"),
                term(timestamp("2026-04-01T00:00:00Z")),
            ),
            compare(
                CompareOp::Gt,
                OrderedDomain::Duration,
                v("g"),
                term(duration("PT1H")),
            ),
        ]),
    );
    let values = invariant(
        "values",
        and(vec![
            eq(
                arith(ArithOp::Add, v("x"), arith(ArithOp::Sub, v("y"), v("z"))),
                arith(
                    ArithOp::Mul,
                    arith(ArithOp::Div, v("x"), v("y")),
                    arith(ArithOp::Mod, v("y"), term(dec("2"))),
                ),
            ),
            eq(
                ValueExpr::Sum {
                    value: Box::new(v("g")),
                    body: Box::new(claim("Ping", vec![wildcard(), wildcard(), var("g")])),
                    seed: SumSeed::Duration,
                },
                ValueExpr::Sum {
                    value: Box::new(v("q")),
                    body: Box::new(claim("Charge", vec![wildcard(), var("q")])),
                    seed: SumSeed::Quantity(Unit::from("USD".to_string())),
                },
            ),
            eq(
                ValueExpr::Extremum {
                    op: ExtremumOp::Max,
                    value: var("n"),
                    body: Box::new(claim("Account", vec![wildcard(), var("n")])),
                },
                ValueExpr::Extremum {
                    op: ExtremumOp::Min,
                    value: var("n"),
                    body: Box::new(claim("Account", vec![wildcard(), var("n")])),
                },
            ),
            eq(
                value_of("Account", vec![subj("acct"), wildcard()]),
                value_of_with_default("Rate", vec![var("a"), wildcard()], term(dec("0"))),
            ),
            eq(
                value_of_extracting("Window", vec![var("a"), wildcard(), wildcard()], 2),
                cond(
                    claim("Flag", vec![var("a"), wildcard()]),
                    abs(v("x")),
                    round(v("x"), term(dec("0.01"))),
                ),
            ),
            eq(
                period_index(term(date("2026-04-01")), term(span("P1Y")), v("d")),
                min(v("x"), max(v("y"), v("z"))),
            ),
            eq(
                period_start_of(term(date("2026-04-01")), term(span("P3M")), v("i")),
                term(qty("12.5", "USD")),
            ),
        ]),
    );
    let statements = transformation(
        "statements",
        params(&["id", "items", "amount"]),
        vec![
            require(claim("Account", vec![var("id"), wildcard()])),
            require_named("authorised", claim("Flag", vec![actor(), wildcard()])),
            bind_one(claim("Rate", vec![var("id"), var("rate")])),
            bind_one_named(
                "current",
                claim("Window", vec![var("id"), var("from"), var("value")]),
            ),
            let_("next", arith(ArithOp::Add, v("rate"), v("amount"))),
            let_new_subject("fresh"),
            assert_("Account", vec![var("fresh"), var("next")]),
            retract("Flag", vec![var("id"), wildcard()]),
            for_(
                "item",
                v("items"),
                vec![
                    assert_("Bag", vec![var("item"), var("items"), subj("tagged")]),
                    for_(
                        "inner",
                        v("items"),
                        vec![emit("Notify", vec![var("inner"), var("amount")])],
                    ),
                ],
            ),
            emit("Notify", vec![var("id"), var("amount")]),
        ],
    );
    let mut program = program("encoding_v1_carrier")
        .predicates(predicates)
        .intents(intents)
        .definitions(definitions)
        .invariants(vec![propositions, total, orderings, values])
        .transformations(vec![statements])
        .derived_claims(derived)
        .build();
    crate::disciplines::lower_disciplines(&mut program);
    program
}

#[test]
fn canonical_encoding_v1_is_frozen() {
    let program = encoding_v1_carrier();
    let preimage = canonical_preimage(&program);
    assert_eq!(
        preimage, PREIMAGE,
        "the canonical encoding of an existing programme changed; `sha256:` names \
         encoding 1, frozen. A new construct may only add rendering for itself."
    );
    assert_eq!(canonical_hash(&program), HASH);
}

fn fees(value: ValueExpr, binder: &str, seed: SumSeed) -> ValueExpr {
    ValueExpr::Sum {
        value: Box::new(value),
        body: Box::new(claim("Fee", vec![var(binder), var("x")])),
        seed,
    }
}

fn capped(total: ValueExpr) -> Program {
    program("seeds")
        .predicates(vec![
            predicate("Fee").subject("id").decimal("amount").build(),
        ])
        .invariants(vec![invariant("capped", le(total, term(dec("100"))))])
        .build()
}

/// The hash leaves seeds out, so a seed the declarations do not give
/// would change what an empty sum returns under the same hash. A target
/// lowering cannot type directly (`abs`, a nested sum) gets the decimal
/// default, never the seed it arrived with.
#[test]
fn a_sum_seed_the_declarations_do_not_give_is_refused() {
    use SumSeed::{Decimal, Duration};
    let cases = [
        (
            "a variable",
            fees(v("x"), "id", Decimal),
            fees(v("x"), "id", Duration),
        ),
        (
            "abs",
            fees(abs(v("x")), "id", Decimal),
            fees(abs(v("x")), "id", Duration),
        ),
        (
            "the outer of nested sums",
            fees(fees(v("x"), "inner", Decimal), "id", Decimal),
            fees(fees(v("x"), "inner", Decimal), "id", Duration),
        ),
        (
            "the inner of nested sums",
            fees(fees(v("x"), "inner", Decimal), "id", Decimal),
            fees(fees(v("x"), "inner", Duration), "id", Decimal),
        ),
    ];
    for (case, faithful, forged) in cases {
        let (faithful, forged) = (capped(faithful), capped(forged));
        assert_eq!(canonical_hash(&faithful), canonical_hash(&forged), "{case}");
        faithful
            .validate()
            .unwrap_or_else(|e| panic!("{case}: a decimal sum needs no lowering: {e:?}"));
        let errors = forged
            .validate()
            .expect_err(&format!("{case}: the forged seed is refused"));
        assert!(
            errors.iter().any(|e| matches!(
                e,
                crate::ValidationError::SumSeedNotFaithful {
                    context: crate::ValidationContext::Invariant { name },
                } if name == "capped"
            )),
            "{case}: expected SumSeedNotFaithful naming `capped`, got {errors:?}"
        );
    }
}

/// No source can write a version but 1, and the hash does not carry one,
/// so another version would name a different rule under the same hash.
#[test]
fn an_invariant_version_other_than_1_is_refused() {
    let current = capped(fees(v("x"), "id", SumSeed::Decimal));
    let mut versioned = current.clone();
    versioned.invariants[0].version = 2;
    assert_eq!(canonical_hash(&current), canonical_hash(&versioned));

    current.validate().expect("version 1 validates");
    let errors = versioned.validate().expect_err("version 2 is refused");
    assert!(
        errors.iter().any(|e| matches!(
            e,
            crate::ValidationError::InvariantVersionNotOne {
                version: 2,
                context: crate::ValidationContext::Invariant { name },
            } if name == "capped"
        )),
        "expected InvariantVersionNotOne naming `capped`, got {errors:?}"
    );
}
