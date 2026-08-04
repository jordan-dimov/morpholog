//! Lowering pass resolving each `sum(...)`'s empty-case seed.
//!
//! A sum's runtime kind is driven by its values, but the empty sum has
//! none - and a bare-decimal zero satisfies no quantity or duration
//! comparison, so an aggregate rule detonated the first time its book
//! was empty unless a zero-valued seed claim opened it. This pass makes
//! that ritual unnecessary: the summed variable's declared kind is
//! static knowledge, so the typed zero is resolved here, once, and
//! stamped on the node for the evaluator to return.
//!
//! Runs in `parse_program` after `resolve_defined_calls` (the kind of a
//! variable bound inside a definition call is found by descending into
//! the definition's body). Idempotent; hand-built IR that skips it
//! keeps the decimal default, which is the pre-pass behaviour.

use std::collections::{BTreeMap, BTreeSet};

use crate::definitions::DefinitionTable;
use crate::ir::{
    DefinitionName, PredicateArgKind, Program, Prop, SumSeed, Term, Value, ValueExpr, Var,
};
use crate::validate::MAX_EXPR_DEPTH;

/// Resolve every `Sum` node's empty-case seed from the summed
/// variable's declared kind. See the module doc for why this is a
/// lowering concern, not an evaluation-time lookup.
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
    let mut lowerer = LowerSeeds { ctx: &ctx };
    for def in &mut program.definitions {
        crate::fold::rewrite_prop(&mut def.body, &mut lowerer);
    }
    for inv in &mut program.invariants {
        crate::fold::rewrite_prop(&mut inv.body, &mut lowerer);
    }
    for t in &mut program.transformations {
        for stmt in &mut t.body {
            crate::fold::rewrite_stmt(stmt, &mut lowerer);
        }
    }
    for dc in &mut program.derived_claims {
        crate::fold::rewrite_prop(&mut dc.domain, &mut lowerer);
        for v in &mut dc.values {
            crate::fold::rewrite_value(&mut v.expr, &mut lowerer);
        }
    }
}

struct SeedContext<'a> {
    kinds: &'a BTreeMap<String, Vec<PredicateArgKind>>,
    definitions: DefinitionTable<'a>,
}

/// Resolve each `sum`'s seed to the kind its target carries. Only the
/// `Sum` node matters; the descent is [`crate::fold`]'s.
struct LowerSeeds<'a> {
    ctx: &'a SeedContext<'a>,
}

impl crate::fold::Rewrite for LowerSeeds<'_> {
    fn value(&mut self, expr: &mut ValueExpr) -> crate::fold::Descend {
        if let ValueExpr::Sum { value, body, seed } = expr {
            let resolved = match value {
                // A variable's kind comes from the claim position that
                // binds it; a literal target carries its kind itself
                // (`sum(1 t | ...)` counts in tonnes, empty or not).
                Term::Var(v) => var_seed(v, body, self.ctx, &mut BTreeSet::new()),
                Term::Literal(Value::Quantity { unit, .. }) => {
                    Some(SumSeed::Quantity(unit.clone()))
                }
                Term::Literal(Value::Duration(_)) => Some(SumSeed::Duration),
                _ => None,
            };
            if let Some(resolved) = resolved {
                *seed = resolved;
            }
        }
        crate::fold::Descend::Into
    }
}

/// The seed for a variable summed over `body`: the declared kind of the
/// first claim position that binds it, descending into definition calls
/// by mapping the call argument onto the parameter it binds. `None`
/// (no summable position found - a pre-bound variable, a subject join)
/// leaves the decimal default standing.
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
        // The shared table's stack guard stops cycles; the budget
        // stops depth. Both are needed and they answer different
        // hazards: `seen` catches a definition already on this path,
        // while a chain of DISTINCT definitions is acyclic and would
        // otherwise recurse until the stack ran out - this pass runs
        // during parsing, before validation's own depth guard gets to
        // refuse the programme.
        //
        // The budget is validation's constant, not a number of this
        // pass's own. A definition call is charged its callee's
        // expanded depth there, so a chain longer than this is a
        // programme validation is about to reject anyway: nothing
        // resolvable is lost by stopping, and the diagnostic the
        // author gets is `NestingTooDeep` rather than a crash.
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
    use crate::ir::Stmt;
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

    /// A duration literal as the sum target carries its kind itself,
    /// like the quantity literal beside it - and a sum sitting in a
    /// transformation STATEMENT (a `let`, not an invariant) is lowered
    /// by the statement walker, not only the invariant walker.
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

    /// A chain of DISTINCT definitions resolves the summed variable's
    /// kind all the way down, right up to the depth validation
    /// permits. The old cap truncated at 16 and silently fell back to
    /// the decimal default - a wrong answer for a programme that had
    /// done nothing unusual.
    #[test]
    fn a_long_chain_of_definitions_still_resolves_the_summed_kind() {
        let mut deep = chained(64);
        lower_sum_seeds(&mut deep);
        assert_eq!(seed_of(&deep), SumSeed::Quantity("t".into()));

        // The deepest chain validation will accept still resolves, so
        // the budget never truncates a programme that would be lawful.
        let mut at_limit = chained(MAX_EXPR_DEPTH - 1);
        lower_sum_seeds(&mut at_limit);
        assert_eq!(seed_of(&at_limit), SumSeed::Quantity("t".into()));
    }

    /// The budget's real job. Lowering runs during parsing, BEFORE
    /// validation's depth guard, so an acyclic chain long enough to
    /// exhaust the stack has to stop here - and the author still gets
    /// the diagnostic rather than a crash, because validation refuses
    /// the same programme moments later.
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

    /// The hazard the descent bound actually existed for: a CYCLE in
    /// hand-built IR, which validation would refuse but this pass runs
    /// before. The stack guard stops it - the pass terminates and the
    /// unresolvable variable keeps the decimal default.
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
