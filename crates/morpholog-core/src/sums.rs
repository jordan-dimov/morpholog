//! Lowering pass resolving each `sum(...)`'s empty-case seed.
//!
//! A sum's kind comes from its values, but an empty sum has none. A
//! bare-decimal zero fails every quantity or duration comparison, so an
//! aggregate rule would break on an empty book. The summed variable's
//! declared kind is known statically, so this pass stamps the typed zero
//! on the node for the evaluator to return.
//!
//! Runs in `parse_program` after `resolve_defined_calls`, since a
//! variable bound inside a definition call takes its kind from the
//! definition's body. Idempotent. Hand-built IR that skips it keeps the
//! decimal zero, and validation refuses it (`EmptySumUntyped`) wherever
//! the checker sees a duration or quantity target.

use std::collections::{BTreeMap, BTreeSet};

use crate::definitions::DefinitionTable;
use crate::ir::{
    DefinitionName, PredicateArgKind, Program, Prop, Stmt, SumSeed, Term, Value, ValueExpr, Var,
};
use crate::validate::MAX_EXPR_DEPTH;

/// Resolve every `Sum` node's empty-case seed from the summed
/// variable's declared kind.
pub fn lower_sum_seeds(program: &mut Program) {
    let kinds: BTreeMap<String, Vec<PredicateArgKind>> = program
        .predicates
        .iter()
        .map(|p| {
            (
                p.name.to_string(),
                p.args.iter().map(|a| a.kind.clone()).collect(),
            )
        })
        .collect();
    // Cloned so definition bodies can be consulted for variable kinds
    // while the originals are themselves being lowered.
    let definitions = program.definitions.clone();
    let ctx = SeedContext {
        kinds: &kinds,
        definitions: DefinitionTable::new(&definitions),
    };
    for def in &mut program.definitions {
        lower_in_prop(&mut def.body, &ctx);
    }
    for inv in &mut program.invariants {
        lower_in_prop(&mut inv.body, &ctx);
    }
    for t in &mut program.transformations {
        for stmt in &mut t.body {
            lower_in_stmt(stmt, &ctx);
        }
    }
    for dc in &mut program.derived_claims {
        lower_in_prop(&mut dc.domain, &ctx);
        for v in &mut dc.values {
            lower_in_value(&mut v.expr, &ctx);
        }
    }
}

struct SeedContext<'a> {
    kinds: &'a BTreeMap<String, Vec<PredicateArgKind>>,
    definitions: DefinitionTable<'a>,
}

fn lower_in_prop(prop: &mut Prop, ctx: &SeedContext<'_>) {
    match prop {
        Prop::Claim { .. } | Prop::Defined { .. } | Prop::In(_, _) => {}
        Prop::And(props) | Prop::Or(props) => {
            for p in props {
                lower_in_prop(p, ctx);
            }
        }
        Prop::Implies { left, right } | Prop::Xor(left, right) => {
            lower_in_prop(left, ctx);
            lower_in_prop(right, ctx);
        }
        Prop::Not(p) | Prop::Exists { body: p, .. } | Prop::Pre(p) => lower_in_prop(p, ctx),
        Prop::Forall { source, body, .. } => {
            lower_in_prop(source, ctx);
            lower_in_prop(body, ctx);
        }
        Prop::Eq(l, r) | Prop::Neq(l, r) => {
            lower_in_value(l, ctx);
            lower_in_value(r, ctx);
        }
        Prop::Compare { left, right, .. } => {
            lower_in_value(left, ctx);
            lower_in_value(right, ctx);
        }
    }
}

fn lower_in_value(value: &mut ValueExpr, ctx: &SeedContext<'_>) {
    match value {
        ValueExpr::Term(_) => {}
        ValueExpr::ValueOf { default, .. } => {
            if let Some(d) = default {
                lower_in_value(d, ctx);
            }
        }
        ValueExpr::Arith { left, right, .. } => {
            lower_in_value(left, ctx);
            lower_in_value(right, ctx);
        }
        // An empty extremum has no value to type, but a sum can sit in
        // its body.
        ValueExpr::Extremum { body, .. } => lower_in_prop(body, ctx),
        ValueExpr::Sum { value, body, seed } => {
            lower_in_prop(body, ctx);
            lower_in_value(value, ctx);
            if let Some(resolved) = value_seed(value, body, ctx) {
                *seed = resolved;
            }
        }
        // Nothing of its own to type, but a sum can sit in the
        // condition or either branch.
        ValueExpr::Cond {
            when,
            then,
            otherwise,
        } => {
            lower_in_prop(when, ctx);
            lower_in_value(then, ctx);
            lower_in_value(otherwise, ctx);
        }
        ValueExpr::Call { args, .. } => {
            for a in args {
                lower_in_value(a, ctx);
            }
        }
    }
}

fn lower_in_stmt(stmt: &mut Stmt, ctx: &SeedContext<'_>) {
    match stmt {
        Stmt::Require { prop: p, .. } | Stmt::BindOne { prop: p, .. } => lower_in_prop(p, ctx),
        Stmt::Let { value, .. } => lower_in_value(value, ctx),
        Stmt::Assert(_) | Stmt::Retract { .. } | Stmt::Emit(_) | Stmt::LetNewSubject { .. } => {}
        Stmt::For {
            collection, body, ..
        } => {
            lower_in_value(collection, ctx);
            for inner in body {
                lower_in_stmt(inner, ctx);
            }
        }
    }
}

/// The seed for an expression target:
/// - a variable: the kind of the claim position that binds it;
/// - a literal: its own kind (`sum(1 t | ...)` counts in tonnes);
/// - a `value` lookup: the declared kind of the extracted position;
/// - a conditional: its branches, when they agree;
/// - a nested sum: its own resolved seed;
/// - arithmetic: the rule matrix over the operands' seeds. With one side
///   unknown, a unique rule still decides (`qty * factor` is a quantity
///   of `qty`'s unit).
///
/// Anything else (an outer-bound variable, a builtin call) keeps the
/// decimal default. If the checker then sees a duration or quantity
/// there, it refuses the programme (`EmptySumUntyped`), so the default
/// is never a wrong zero in a committed programme.
fn value_seed(value: &ValueExpr, body: &Prop, ctx: &SeedContext<'_>) -> Option<SumSeed> {
    let kind = match value {
        ValueExpr::Term(Term::Var(v)) => return var_seed(v, body, ctx, &mut BTreeSet::new()),
        ValueExpr::Term(Term::Literal(Value::Quantity { unit, .. })) => {
            return Some(SumSeed::Quantity(unit.clone()));
        }
        ValueExpr::Term(Term::Literal(Value::Duration(_))) => return Some(SumSeed::Duration),
        ValueExpr::Term(Term::Literal(Value::Decimal(_))) => return Some(SumSeed::Decimal),
        ValueExpr::Arith { op, left, right } => {
            let l = value_seed(left, body, ctx).map(seed_kind);
            let r = value_seed(right, body, ctx).map(seed_kind);
            match (l, r) {
                (Some(l), Some(r)) => crate::ir::arith_result_kind(*op, &l, &r)?,
                (Some(k), None) => crate::ir::arith_unique_counterpart(*op, &k, true)?.1,
                (None, Some(k)) => crate::ir::arith_unique_counterpart(*op, &k, false)?.1,
                (None, None) => return None,
            }
        }
        // The declared kind at the extracted position, as the checker's
        // `value_of_result_kind` reads it.
        ValueExpr::ValueOf {
            predicate, extract, ..
        } => {
            let kinds = ctx.kinds.get(predicate.as_str())?;
            kinds.get(*extract)?.clone()
        }
        // A conditional is typed only when both branches agree; the
        // sum's body supplies the bindings either branch would consume.
        ValueExpr::Cond {
            then, otherwise, ..
        } => {
            let t = value_seed(then, body, ctx)?;
            let o = value_seed(otherwise, body, ctx)?;
            if t != o {
                return None;
            }
            return Some(t);
        }
        // The recursion above already resolved the nested sum's seed.
        ValueExpr::Sum { seed, .. } => return Some(seed.clone()),
        _ => return None,
    };
    match kind {
        PredicateArgKind::Decimal => Some(SumSeed::Decimal),
        PredicateArgKind::Duration => Some(SumSeed::Duration),
        PredicateArgKind::Quantity(u) => Some(SumSeed::Quantity(u)),
        _ => None,
    }
}

fn seed_kind(seed: SumSeed) -> PredicateArgKind {
    match seed {
        SumSeed::Decimal => PredicateArgKind::Decimal,
        SumSeed::Duration => PredicateArgKind::Duration,
        SumSeed::Quantity(u) => PredicateArgKind::Quantity(u),
    }
}

/// The seed for a variable summed over `body`: the declared kind of the
/// first claim position that binds it, following definition calls
/// through their parameters. `None` (a pre-bound variable, a subject
/// join) keeps the decimal default.
fn var_seed(
    var: &Var,
    body: &Prop,
    ctx: &SeedContext<'_>,
    seen: &mut BTreeSet<DefinitionName>,
) -> Option<SumSeed> {
    match body {
        Prop::Claim { predicate, args } => {
            let kinds = ctx.kinds.get(predicate.as_str())?;
            args.iter()
                .zip(kinds)
                .find_map(|(arg, kind)| match (arg, kind) {
                    (Term::Var(v), PredicateArgKind::Quantity(unit)) if v == var => {
                        Some(SumSeed::Quantity(unit.clone()))
                    }
                    (Term::Var(v), PredicateArgKind::Duration) if v == var => {
                        Some(SumSeed::Duration)
                    }
                    (Term::Var(v), PredicateArgKind::Decimal) if v == var => Some(SumSeed::Decimal),
                    _ => None,
                })
        }
        // `seen` stops cycles; this budget stops long acyclic chains,
        // which would otherwise overflow the stack. The pass runs before
        // validation's depth guard. It uses validation's own limit, so
        // any chain cut here is one validation rejects anyway with
        // `NestingTooDeep`.
        Prop::Defined { .. } if seen.len() >= MAX_EXPR_DEPTH => None,
        Prop::Defined { name, args } => ctx.definitions.enter(name, seen, |def, seen| {
            args.iter()
                .zip(&def.parameters)
                .filter(|(arg, _)| matches!(arg, Term::Var(v) if v == var))
                .find_map(|(_, param)| var_seed(param, &def.body, ctx, seen))
        }),
        Prop::And(props) | Prop::Or(props) => {
            props.iter().find_map(|p| var_seed(var, p, ctx, seen))
        }
        Prop::Implies { left, right } | Prop::Xor(left, right) => {
            var_seed(var, left, ctx, seen).or_else(|| var_seed(var, right, ctx, seen))
        }
        Prop::Not(p) | Prop::Exists { body: p, .. } | Prop::Pre(p) => var_seed(var, p, ctx, seen),
        Prop::Forall { source, body, .. } => {
            var_seed(var, source, ctx, seen).or_else(|| var_seed(var, body, ctx, seen))
        }
        Prop::In(_, _) | Prop::Eq(_, _) | Prop::Neq(_, _) | Prop::Compare { .. } => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::ir::Definition;
    use crate::ir_builder::{defined, invariant, predicate, program, sum};
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
}
