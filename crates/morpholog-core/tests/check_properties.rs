//! Property-based robustness tests for `Program::validate`.
//!
//! Companion to the example-driven unit tests in `src/check.rs` (which
//! pin specific diagnostics against known inputs) and to the worked-
//! example suite (which proves no false positives on real programmes).
//! These tests fuzz the *shape* space: arbitrary and adversarially deep
//! IR must always make `validate` return a verdict - never panic, never
//! index out of bounds, never recurse off the stack. That is the
//! durable proof behind the contract that untrusted IR can be validated
//! before it is proposed.
//!
//! Generation stays deliberately bounded (short names, shallow nesting,
//! small vectors) so generation is cheap and shrinking reports are
//! small. The depth guard, which only triggers far below any bound the
//! random generator reaches, is exercised separately by explicitly deep
//! inputs that walk every recursive arm.
//!
//! The IR's two sorts ([`Prop`] and [`ValueExpr`]) mean the generator
//! also splits in two: `arb_prop` builds propositions, `arb_value_expr`
//! builds value expressions. They are mutually recursive (a comparator
//! relates two values, a `sum` ranges over a proposition), so the value
//! generator nests a bounded proposition generator for `sum` bodies, and
//! the proposition generator nests the value generator for comparator
//! operands.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{
    ArgDecl, Claim, CompareOp, DerivedClaim, DerivedValue, Intent, IntentDecl, Invariant,
    OrderedDomain, PredicateArgKind, PredicateDecl, Program, Prop, Stmt, Term, Transformation,
    ValidationError, Value, ValueExpr,
};
use proptest::prelude::*;

// ---------- leaf generators ----------

fn arb_pred_name() -> impl Strategy<Value = String> {
    "[A-Z][a-zA-Z0-9_]{0,8}".prop_map(|s| s)
}

fn arb_var_name() -> impl Strategy<Value = String> {
    "[a-z][a-z0-9_]{0,6}".prop_map(|s| s)
}

fn arb_kind() -> impl Strategy<Value = PredicateArgKind> {
    prop_oneof![
        Just(PredicateArgKind::Subject),
        Just(PredicateArgKind::Decimal),
        Just(PredicateArgKind::Date),
        Just(PredicateArgKind::Bool),
        Just(PredicateArgKind::Collection),
        Just(PredicateArgKind::Any),
    ]
}

fn arb_value() -> impl Strategy<Value = Value> {
    prop_oneof![
        "[a-z_]{1,8}".prop_map(|s| Value::Subject(s.into())),
        "-?[0-9]{1,6}".prop_map(Value::Decimal),
        Just(Value::Date("2026-05-24".to_string())),
    ]
}

fn arb_term() -> impl Strategy<Value = Term> {
    prop_oneof![
        arb_var_name().prop_map(Term::Var),
        Just(Term::Wildcard),
        arb_value().prop_map(Term::Literal),
        Just(Term::Actor),
    ]
}

fn arb_args() -> impl Strategy<Value = Vec<Term>> {
    prop::collection::vec(arb_term(), 0..4)
}

// ---------- value-expression generator ----------

/// A bounded value expression. `Sum` ranges over a (leaf-only)
/// proposition, so the value generator can recurse without forming an
/// unbounded mutual cycle with `arb_prop`; the property under test only
/// needs each recursive arm reached, not maximal mutual nesting.
fn arb_value_expr() -> impl Strategy<Value = ValueExpr> {
    let leaf = prop_oneof![
        arb_term().prop_map(ValueExpr::Term),
        (arb_pred_name(), arb_args()).prop_map(|(predicate, args)| ValueExpr::ValueOf {
            predicate,
            args,
            default: None,
        }),
    ];
    leaf.prop_recursive(4, 32, 4, |inner| {
        prop_oneof![
            (inner.clone(), inner.clone())
                .prop_map(|(l, r)| ValueExpr::Add(Box::new(l), Box::new(r))),
            (inner.clone(), inner.clone())
                .prop_map(|(l, r)| ValueExpr::Sub(Box::new(l), Box::new(r))),
            (arb_term(), arb_prop_leaf()).prop_map(|(value, body)| ValueExpr::Sum {
                value,
                body: Box::new(body),
            }),
            (arb_pred_name(), arb_args(), inner).prop_map(|(predicate, args, default)| {
                ValueExpr::ValueOf {
                    predicate,
                    args,
                    default: Some(Box::new(default)),
                }
            }),
        ]
    })
}

/// A leaf-only proposition, used as the body of a `sum` inside the value
/// generator to keep the mutual recursion bounded.
fn arb_prop_leaf() -> impl Strategy<Value = Prop> {
    prop_oneof![
        (arb_pred_name(), arb_args()).prop_map(|(predicate, args)| Prop::Claim { predicate, args }),
        (arb_term(), arb_term()).prop_map(|(a, b)| Prop::In(a, b)),
    ]
}

// ---------- recursive proposition generator ----------

fn arb_prop() -> impl Strategy<Value = Prop> {
    let leaf = prop_oneof![
        (arb_pred_name(), arb_args()).prop_map(|(predicate, args)| Prop::Claim { predicate, args }),
        (arb_value_expr(), arb_value_expr()).prop_map(|(a, b)| Prop::Neq(Box::new(a), Box::new(b))),
        (arb_term(), arb_term()).prop_map(|(a, b)| Prop::In(a, b)),
    ];
    // depth 4, ~32 total nodes, up to 4 children per collection node.
    leaf.prop_recursive(4, 32, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 1..4).prop_map(Prop::And),
            prop::collection::vec(inner.clone(), 1..4).prop_map(Prop::Or),
            inner.clone().prop_map(|e| Prop::Not(Box::new(e))),
            inner.clone().prop_map(|e| Prop::Pre(Box::new(e))),
            (inner.clone(), inner.clone()).prop_map(|(l, r)| Prop::Implies {
                left: Box::new(l),
                right: Box::new(r),
            }),
            (arb_value_expr(), arb_value_expr())
                .prop_map(|(l, r)| Prop::Eq(Box::new(l), Box::new(r))),
            (arb_value_expr(), arb_value_expr()).prop_map(|(l, r)| Prop::Compare {
                op: CompareOp::Le,
                domain: OrderedDomain::Decimal,
                left: Box::new(l),
                right: Box::new(r),
            }),
            (arb_value_expr(), arb_value_expr()).prop_map(|(l, r)| Prop::Compare {
                op: CompareOp::Le,
                domain: OrderedDomain::Date,
                left: Box::new(l),
                right: Box::new(r),
            }),
            (arb_var_name(), inner.clone()).prop_map(|(binding, body)| Prop::Exists {
                binding,
                body: Box::new(body),
            }),
            (arb_var_name(), inner.clone(), inner).prop_map(|(binding, source, body)| {
                Prop::Forall {
                    binding,
                    source: Box::new(source),
                    body: Box::new(body),
                }
            }),
        ]
    })
}

// ---------- recursive statement generator ----------

fn arb_stmt() -> impl Strategy<Value = Stmt> {
    let leaf = prop_oneof![
        arb_prop().prop_map(Stmt::Require),
        arb_prop().prop_map(Stmt::BindOne),
        (arb_var_name(), arb_value_expr()).prop_map(|(name, value)| Stmt::Let { name, value }),
        arb_var_name().prop_map(|name| Stmt::LetNewSubject { name }),
        (arb_pred_name(), arb_args())
            .prop_map(|(predicate, args)| Stmt::Assert(Claim { predicate, args })),
        (arb_pred_name(), arb_args())
            .prop_map(|(predicate, args)| Stmt::Retract { predicate, args }),
        (arb_pred_name(), arb_args()).prop_map(|(name, args)| Stmt::Emit(Intent { name, args })),
    ];
    // `for` is the only statement that nests statements.
    leaf.prop_recursive(3, 16, 3, |inner| {
        (
            arb_var_name(),
            arb_value_expr(),
            prop::collection::vec(inner, 1..3),
        )
            .prop_map(|(binding, collection, body)| Stmt::For {
                binding,
                collection,
                body,
            })
    })
}

// ---------- programme generator ----------

fn arb_arg_decl() -> impl Strategy<Value = ArgDecl> {
    (arb_var_name(), arb_kind()).prop_map(|(name, kind)| ArgDecl { name, kind })
}

fn arb_decl_args() -> impl Strategy<Value = Vec<ArgDecl>> {
    prop::collection::vec(arb_arg_decl(), 0..4)
}

fn arb_pred_decl() -> impl Strategy<Value = PredicateDecl> {
    (arb_pred_name(), arb_decl_args()).prop_map(|(name, args)| PredicateDecl { name, args })
}

fn arb_intent_decl() -> impl Strategy<Value = IntentDecl> {
    (arb_pred_name(), arb_decl_args()).prop_map(|(name, args)| IntentDecl { name, args })
}

fn arb_invariant() -> impl Strategy<Value = Invariant> {
    (arb_pred_name(), arb_prop()).prop_map(|(name, body)| Invariant {
        name,
        version: 1,
        body,
    })
}

fn arb_transformation() -> impl Strategy<Value = Transformation> {
    (
        arb_pred_name(),
        prop::collection::vec(arb_var_name(), 0..4),
        prop::collection::vec(arb_stmt(), 0..4),
    )
        .prop_map(|(name, parameters, body)| Transformation {
            name,
            parameters,
            body,
        })
}

fn arb_derived_claim() -> impl Strategy<Value = DerivedClaim> {
    (
        arb_pred_name(),
        prop::collection::vec(arb_var_name(), 0..3),
        prop::collection::vec((arb_var_name(), arb_value_expr()), 0..3),
        arb_prop(),
    )
        .prop_map(|(predicate, keys, values, domain)| DerivedClaim {
            predicate,
            keys,
            values: values
                .into_iter()
                .map(|(name, expr)| DerivedValue { name, expr })
                .collect(),
            domain,
        })
}

fn arb_program() -> impl Strategy<Value = Program> {
    (
        prop::collection::vec(arb_pred_decl(), 0..4),
        prop::collection::vec(arb_intent_decl(), 0..3),
        prop::collection::vec(arb_invariant(), 0..3),
        prop::collection::vec(arb_transformation(), 0..3),
        prop::collection::vec(arb_derived_claim(), 0..2),
    )
        .prop_map(
            |(predicates, intents, invariants, transformations, derived_claims)| Program {
                name: "fuzz".to_string(),
                predicates,
                intents,
                invariants,
                transformations,
                derived_claims,
            },
        )
}

proptest! {
    /// `validate` must return a verdict on any IR we can build, well-
    /// formed or not - never panic, never index out of bounds, never
    /// recurse off the stack. Most generated programmes are malformed
    /// and validate to `Err`; the property is only that the call
    /// *returns*. proptest fails the case on any panic, with a shrunk
    /// counterexample.
    #[test]
    fn validate_returns_on_arbitrary_programmes(p in arb_program()) {
        let _ = p.validate();
    }

    /// `validate` is deterministic: the same programme produces the
    /// same verdict, including the same error order, on repeated calls.
    /// Guards against HashMap-iteration order or other nondeterminism
    /// leaking into the result that a migration would see as a churning
    /// work list.
    #[test]
    fn validate_is_deterministic(p in arb_program()) {
        prop_assert_eq!(p.validate(), p.validate());
    }
}

/// Wrap `leaf` in `depth` copies of one recursive proposition node,
/// selected by `node`. Exercises every recursive match arm of the depth
/// measure that lives on the `Prop` sort - the single-child arm
/// (`Not`/`Pre`/`Exists`), the collection arm (`And`/`Or`), the
/// two-child arm (`Implies`), the comparator (whose deepening operand is
/// a value expression), and the quantifier `Forall` (recurses through
/// both source and body). Filler operands are wildcards; the depth guard
/// short-circuits before any semantic check looks at them.
fn nest_prop(node: usize, depth: usize, leaf: Prop) -> Prop {
    let mut e = leaf;
    for _ in 0..depth {
        e = match node {
            0 => Prop::Not(Box::new(e)),
            1 => Prop::Pre(Box::new(e)),
            2 => Prop::And(vec![e]),
            3 => Prop::Or(vec![e]),
            4 => Prop::Implies {
                left: Box::new(e),
                right: Box::new(Prop::Claim {
                    predicate: "A".to_string(),
                    args: vec![],
                }),
            },
            5 => Prop::Exists {
                binding: "x".to_string(),
                body: Box::new(e),
            },
            _ => Prop::Forall {
                binding: "x".to_string(),
                source: Box::new(Prop::Claim {
                    predicate: "A".to_string(),
                    args: vec![],
                }),
                body: Box::new(e),
            },
        };
    }
    e
}

/// Wrap a leaf value expression in `depth` copies of one recursive value
/// node, then a comparator that puts the deep value on one side. This
/// reaches the value-sort depth arms - the two-child arithmetic arm
/// (`Add`/`Sub`) and `ValueOf` (recurses only through its `default`) -
/// and feeds the result through a `Prop::Compare` so the validator sees a
/// proposition. (`Sum` deepens through a `Prop` body, covered by the
/// proposition deep-nest test's reachability into the value sort.)
fn nest_value(node: usize, depth: usize) -> Prop {
    let filler = || Box::new(ValueExpr::Term(Term::Wildcard));
    let mut e = ValueExpr::Term(Term::Wildcard);
    for _ in 0..depth {
        e = match node {
            0 => ValueExpr::Add(Box::new(e), filler()),
            1 => ValueExpr::Sub(Box::new(e), filler()),
            _ => ValueExpr::ValueOf {
                predicate: "P".to_string(),
                args: vec![],
                default: Some(Box::new(e)),
            },
        };
    }
    Prop::Compare {
        op: CompareOp::Le,
        domain: OrderedDomain::Decimal,
        left: Box::new(e),
        right: filler(),
    }
}

#[test]
fn deeply_nested_propositions_are_rejected_not_overflowed() {
    // Each recursive proposition arm, nested far past any plausible
    // limit, must come back as a depth rejection - a returned verdict,
    // not a blown stack. The depth guard runs first and short-circuits,
    // so a pure deep-nest yields NestingTooDeep and nothing downstream.
    const DEPTH: usize = 1024;
    for node in 0..7 {
        let body = nest_prop(
            node,
            DEPTH,
            Prop::Claim {
                predicate: "A".to_string(),
                args: vec![],
            },
        );
        let p = Program {
            name: "deep".to_string(),
            predicates: vec![],
            intents: vec![],
            invariants: vec![Invariant {
                name: "i".to_string(),
                version: 1,
                body,
            }],
            transformations: vec![],
            derived_claims: vec![],
        };
        let errs = p.validate().expect_err("deep nesting must be rejected");
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::NestingTooDeep { .. })),
            "node {node}: expected NestingTooDeep, got {errs:?}"
        );
    }
}

#[test]
fn deeply_nested_value_expressions_are_rejected_not_overflowed() {
    // The value-sort recursion arms, nested far past any limit and
    // wrapped in a comparator so the validator sees a proposition, must
    // also come back as a depth rejection rather than a blown stack.
    const DEPTH: usize = 1024;
    for node in 0..3 {
        let body = nest_value(node, DEPTH);
        let p = Program {
            name: "deep".to_string(),
            predicates: vec![],
            intents: vec![],
            invariants: vec![Invariant {
                name: "i".to_string(),
                version: 1,
                body,
            }],
            transformations: vec![],
            derived_claims: vec![],
        };
        let errs = p
            .validate()
            .expect_err("deep value nesting must be rejected");
        assert!(
            errs.iter()
                .any(|e| matches!(e, ValidationError::NestingTooDeep { .. })),
            "node {node}: expected NestingTooDeep, got {errs:?}"
        );
    }
}

#[test]
fn deeply_nested_for_statements_are_rejected_not_overflowed() {
    // Statement nesting is the other recursion dimension the guard
    // covers; `for` is the only statement that nests statements.
    const DEPTH: usize = 1024;
    let mut body = vec![Stmt::Assert(Claim {
        predicate: "A".to_string(),
        args: vec![],
    })];
    for _ in 0..DEPTH {
        body = vec![Stmt::For {
            binding: "x".to_string(),
            collection: ValueExpr::Term(Term::Var("c".to_string())),
            body,
        }];
    }
    let p = Program {
        name: "deep".to_string(),
        predicates: vec![],
        intents: vec![],
        invariants: vec![],
        transformations: vec![Transformation {
            name: "t".to_string(),
            parameters: vec!["c".to_string()],
            body,
        }],
        derived_claims: vec![],
    };
    let errs = p.validate().expect_err("deep for-nesting must be rejected");
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::NestingTooDeep { .. })),
        "expected NestingTooDeep, got {errs:?}"
    );
}
