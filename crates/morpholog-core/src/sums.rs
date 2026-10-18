//! Lowering pass stamping each `sum(...)`'s empty-case seed.
//!
//! A sum's kind comes from its values, but an empty sum has none, and a
//! bare-decimal zero fails every quantity or duration comparison, so an
//! aggregate rule would break on an empty book. The checker infers each
//! target's kind as it walks; this pass asks it once and writes the typed
//! zero on the node for the evaluator to return. It infers nothing of
//! its own.
//!
//! Runs in `parse_program` after `resolve_defined_calls` and
//! `lower_disciplines`, before validation. The checker recurses over
//! every body, so the programme first passes validation's own structural
//! guards (nesting, definition cycles); one that fails them keeps its
//! decimal defaults, and validation then refuses it by name. Idempotent.
//! The programme hash leaves seeds out, so validation refuses any seed
//! the checker would not give (`SumSeedNotFaithful`), and hand-built IR
//! must lower too.

use std::collections::HashMap;

use crate::ir::{Program, Prop, Stmt, SumSeed, ValueExpr};

/// Stamp every `Sum` node's empty-case seed with the typed zero of the
/// kind the checker infers for its target.
pub fn lower_sum_seeds(program: &mut Program) {
    if crate::validate::structural_errors(program).is_err() {
        return;
    }
    let seeds = crate::check::check_program(program).sum_seeds;
    // The seeds are keyed by node address. Nothing below moves or
    // rebuilds a node, only writes a seed field in place, so each address
    // still names the node the checker saw.
    for def in &mut program.definitions {
        stamp_prop(&mut def.body, &seeds);
    }
    for inv in &mut program.invariants {
        stamp_prop(&mut inv.body, &seeds);
    }
    for t in &mut program.transformations {
        for stmt in &mut t.body {
            stamp_stmt(stmt, &seeds);
        }
    }
    for dc in &mut program.derived_claims {
        stamp_prop(&mut dc.domain, &seeds);
        for v in &mut dc.values {
            stamp_value(&mut v.expr, &seeds);
        }
    }
}

type Seeds = HashMap<*const ValueExpr, SumSeed>;

fn stamp_prop(prop: &mut Prop, seeds: &Seeds) {
    match prop {
        Prop::Claim { .. } | Prop::Defined { .. } | Prop::In(_, _) => {}
        Prop::And(props) | Prop::Or(props) => {
            for p in props {
                stamp_prop(p, seeds);
            }
        }
        Prop::Implies { left, right } | Prop::Xor(left, right) => {
            stamp_prop(left, seeds);
            stamp_prop(right, seeds);
        }
        Prop::Not(p) | Prop::Exists { body: p, .. } | Prop::Pre(p) => stamp_prop(p, seeds),
        Prop::Forall { source, body, .. } => {
            stamp_prop(source, seeds);
            stamp_prop(body, seeds);
        }
        Prop::Eq(l, r) | Prop::Neq(l, r) => {
            stamp_value(l, seeds);
            stamp_value(r, seeds);
        }
        Prop::Compare { left, right, .. } => {
            stamp_value(left, seeds);
            stamp_value(right, seeds);
        }
    }
}

fn stamp_value(expr: &mut ValueExpr, seeds: &Seeds) {
    let node: *const ValueExpr = &raw const *expr;
    match expr {
        ValueExpr::Term(_) => {}
        ValueExpr::ValueOf { default, .. } => {
            if let Some(d) = default {
                stamp_value(d, seeds);
            }
        }
        ValueExpr::Arith { left, right, .. } => {
            stamp_value(left, seeds);
            stamp_value(right, seeds);
        }
        // An empty extremum has no value to type, but a sum can sit in
        // its body.
        ValueExpr::Extremum { body, .. } => stamp_prop(body, seeds),
        ValueExpr::Sum { value, body, seed } => {
            stamp_prop(body, seeds);
            stamp_value(value, seeds);
            if let Some(found) = seeds.get(&node) {
                *seed = found.clone();
            }
        }
        // Nothing of its own to type, but a sum can sit in the
        // condition or either branch.
        ValueExpr::Cond {
            when,
            then,
            otherwise,
        } => {
            stamp_prop(when, seeds);
            stamp_value(then, seeds);
            stamp_value(otherwise, seeds);
        }
        ValueExpr::Call { args, .. } => {
            for a in args {
                stamp_value(a, seeds);
            }
        }
    }
}

fn stamp_stmt(stmt: &mut Stmt, seeds: &Seeds) {
    match stmt {
        Stmt::Require { prop: p, .. } | Stmt::BindOne { prop: p, .. } => stamp_prop(p, seeds),
        Stmt::Let { value, .. } => stamp_value(value, seeds),
        Stmt::Assert(_) | Stmt::Retract { .. } | Stmt::Emit(_) | Stmt::LetNewSubject { .. } => {}
        Stmt::For {
            collection, body, ..
        } => {
            stamp_value(collection, seeds);
            for inner in body {
                stamp_stmt(inner, seeds);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::ir::Definition;
    use crate::ir_builder::{defined, invariant, predicate, program, sum};
    use crate::validate::MAX_EXPR_DEPTH;
    use crate::{CompareOp, OrderedDomain, Term, Value, Var};

    /// A programme whose one invariant sums through `layers` chained
    /// definition calls down to a quantity-kinded claim position.
    fn chained(layers: usize) -> Program {
        let mut definitions: Vec<Definition> = Vec::new();
        for i in 0..layers {
            let body = if i + 1 == layers {
                Prop::Claim {
                    predicate: "P".into(),
                    args: vec![Term::Var(Var::from("x"))],
                }
            } else {
                defined(&format!("d{}", i + 1), vec![Term::Var(Var::from("x"))])
            };
            definitions.push(Definition {
                origin: crate::ir::DefinitionOrigin::Authored,
                name: format!("d{i}").into(),
                parameters: vec![Var::from("x")],
                body,
            });
        }
        let body = Prop::Compare {
            op: CompareOp::Le,
            domain: OrderedDomain::Decimal,
            left: Box::new(sum(
                Term::Var(Var::from("v")),
                defined("d0", vec![Term::Var(Var::from("v"))]),
            )),
            right: Box::new(ValueExpr::Term(Term::Literal(Value::Decimal(
                "100".to_string(),
            )))),
        };
        let mut p = program("depth")
            .predicates(vec![predicate("P").quantity("qty", "t").build()])
            .invariants(vec![invariant("capped", body)])
            .build();
        p.definitions = definitions;
        p
    }

    fn seed_of(p: &Program) -> SumSeed {
        let Prop::Compare { left, .. } = &p.invariants[0].body else {
            panic!("comparison expected");
        };
        let ValueExpr::Sum { seed, .. } = left.as_ref() else {
            panic!("sum expected");
        };
        seed.clone()
    }

    /// A duration literal as the sum target carries its own kind, like
    /// a quantity literal. A sum in a transformation statement (a `let`)
    /// is lowered too, not only one in an invariant.
    #[test]
    fn literal_targets_and_statement_sums_lower() {
        use crate::ir_builder::{let_, params, transformation};
        let body = Prop::Claim {
            predicate: "P".into(),
            args: vec![Term::Wildcard],
        };
        let dur_sum = sum(
            Term::Literal(Value::Duration("PT1H".to_string())),
            body.clone(),
        );
        let qty_sum = sum(
            Term::Var(Var::from("q")),
            Prop::Claim {
                predicate: "P".into(),
                args: vec![Term::Var(Var::from("q"))],
            },
        );
        let mut p = program("stmt_sums")
            .predicates(vec![predicate("P").quantity("qty", "t").build()])
            .transformations(vec![transformation(
                "tally",
                params(&[]),
                vec![let_("d", dur_sum), let_("q_total", qty_sum)],
            )])
            .build();
        lower_sum_seeds(&mut p);
        let seeds: Vec<SumSeed> = p.transformations[0]
            .body
            .iter()
            .map(|stmt| match stmt {
                Stmt::Let {
                    value: ValueExpr::Sum { seed, .. },
                    ..
                } => seed.clone(),
                other => panic!("let-sum expected, got {other:?}"),
            })
            .collect();
        assert_eq!(
            seeds,
            vec![SumSeed::Duration, SumSeed::Quantity("t".into())]
        );
    }

    /// A chain of distinct definitions resolves the summed variable's
    /// kind all the way down, up to the depth validation permits.
    #[test]
    fn a_long_chain_of_definitions_still_resolves_the_summed_kind() {
        let mut deep = chained(64);
        lower_sum_seeds(&mut deep);
        assert_eq!(seed_of(&deep), SumSeed::Quantity("t".into()));

        // The deepest chain validation accepts still resolves.
        let mut at_limit = chained(MAX_EXPR_DEPTH - 1);
        lower_sum_seeds(&mut at_limit);
        assert_eq!(seed_of(&at_limit), SumSeed::Quantity("t".into()));
    }

    /// Lowering runs before validation's depth guard, so a huge acyclic
    /// chain must stop here. The author then gets validation's
    /// diagnostic, not a crash.
    #[test]
    fn an_oversized_acyclic_chain_returns_instead_of_exhausting_the_stack() {
        let mut huge = chained(50_000);
        lower_sum_seeds(&mut huge);
        assert_eq!(seed_of(&huge), SumSeed::Decimal);
        let errors = huge.validate().expect_err("validation refuses it");
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, crate::ValidationError::NestingTooDeep { .. })),
            "the author should see NestingTooDeep, got {errors:?}"
        );
    }

    /// A cycle in hand-built IR, which this pass sees before validation
    /// refuses it: the pass terminates and the variable keeps the
    /// decimal default.
    #[test]
    fn a_cyclic_definition_terminates_and_falls_back() {
        let mut p = chained(3);
        // Close the chain: the last definition calls the first.
        let last = p.definitions.len() - 1;
        p.definitions[last].body = defined("d0", vec![Term::Var(Var::from("x"))]);
        lower_sum_seeds(&mut p);
        assert_eq!(seed_of(&p), SumSeed::Decimal);
    }

    /// Lowering runs before validation, so it must come back from a
    /// programme the checker can only refuse: a builtin with the wrong
    /// arity in the target, and a claim that names a definition. The
    /// seed keeps its default and validation names the fault.
    #[test]
    fn malformed_targets_and_unresolved_calls_leave_the_default_and_validate_by_name() {
        use crate::ir_builder::{call, implies, le, term};
        use crate::{Builtin, ValidationError};
        let body = |target: ValueExpr, over: Prop| {
            implies(
                Prop::Claim {
                    predicate: "P".into(),
                    args: vec![Term::Var(Var::from("x"))],
                },
                le(
                    sum(target, over),
                    term(Term::Literal(Value::Decimal("100".to_string()))),
                ),
            )
        };
        let x = || ValueExpr::Term(Term::Var(Var::from("x")));
        let p_x = || Prop::Claim {
            predicate: "P".into(),
            args: vec![Term::Var(Var::from("x"))],
        };
        let cases: [(&str, Prop, fn(&ValidationError) -> bool); 2] = [
            (
                "round with one argument",
                body(call(Builtin::Round, vec![x()]), p_x()),
                |e: &ValidationError| matches!(e, ValidationError::BuiltinArity { .. }),
            ),
            (
                "a claim naming a definition",
                body(
                    x(),
                    Prop::Claim {
                        predicate: "d0".into(),
                        args: vec![Term::Var(Var::from("x"))],
                    },
                ),
                |e: &ValidationError| matches!(e, ValidationError::UnresolvedDefinitionCall { .. }),
            ),
        ];
        for (case, inv_body, expected) in cases {
            let mut p = program("hostile")
                .predicates(vec![predicate("P").quantity("qty", "t").build()])
                .definitions(vec![Definition {
                    origin: crate::ir::DefinitionOrigin::Authored,
                    name: "d0".into(),
                    parameters: vec![Var::from("x")],
                    body: p_x(),
                }])
                .invariants(vec![invariant("capped", inv_body)])
                .build();
            lower_sum_seeds(&mut p);
            let errors = p.validate().expect_err(case);
            assert!(errors.iter().any(expected), "{case}: got {errors:?}");
        }
    }

    /// A body nested beyond the depth validation allows is not walked:
    /// lowering returns with the default seed and validation refuses the
    /// nesting, as it would have before anything recursed.
    #[test]
    fn a_body_too_deep_is_not_walked() {
        use crate::ir_builder::{abs, implies, le, term};
        let mut target = ValueExpr::Term(Term::Var(Var::from("x")));
        for _ in 0..(MAX_EXPR_DEPTH + 8) {
            target = abs(target);
        }
        let mut p = program("deep")
            .predicates(vec![predicate("P").quantity("qty", "t").build()])
            .invariants(vec![invariant(
                "capped",
                implies(
                    Prop::Claim {
                        predicate: "P".into(),
                        args: vec![Term::Var(Var::from("x"))],
                    },
                    le(
                        sum(
                            target,
                            Prop::Claim {
                                predicate: "P".into(),
                                args: vec![Term::Var(Var::from("x"))],
                            },
                        ),
                        term(Term::Literal(Value::Decimal("100".to_string()))),
                    ),
                ),
            )])
            .build();
        lower_sum_seeds(&mut p);
        let errors = p.validate().expect_err("validation refuses the nesting");
        assert!(
            errors
                .iter()
                .any(|e| matches!(e, crate::ValidationError::NestingTooDeep { .. })),
            "got {errors:?}"
        );
    }

    /// A seed is never read back from the node: a quantity seed forged on
    /// a target the checker cannot type is refused, with both seeds named.
    #[test]
    fn a_forged_seed_on_an_untyped_target_is_refused() {
        use crate::ir_builder::{le, params, require, term, transformation, wildcard};
        let mut p = program("forged")
            .predicates(vec![predicate("P").subject("s").build()])
            .transformations(vec![transformation(
                "t",
                params(&["p"]),
                vec![require(le(
                    sum(
                        Term::Var(Var::from("p")),
                        Prop::Claim {
                            predicate: "P".into(),
                            args: vec![wildcard()],
                        },
                    ),
                    term(Term::Literal(Value::Decimal("100".to_string()))),
                ))],
            )])
            .build();
        let Stmt::Require {
            prop: Prop::Compare { left, .. },
            ..
        } = &mut p.transformations[0].body[0]
        else {
            panic!("require of a comparison expected");
        };
        let ValueExpr::Sum { seed, .. } = left.as_mut() else {
            panic!("sum expected");
        };
        *seed = SumSeed::Quantity("t".into());
        let errors = p.validate().expect_err("the forged seed is refused");
        assert!(
            errors.iter().any(|e| matches!(
                e,
                crate::ValidationError::SumSeedNotFaithful {
                    found: SumSeed::Quantity(_),
                    expected: SumSeed::Decimal,
                    ..
                }
            )),
            "got {errors:?}"
        );
        // Lowering restores the checker's seed and the programme validates.
        lower_sum_seeds(&mut p);
        p.validate().expect("lowered, the programme validates");
    }
}
