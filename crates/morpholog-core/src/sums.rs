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

use std::collections::BTreeMap;

use crate::ir::{Definition, PredicateArgKind, Program, Prop, Stmt, SumSeed, Term, ValueExpr, Var};

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
        definitions: &definitions,
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
    definitions: &'a [Definition],
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
        ValueExpr::Sum { value, body, seed } => {
            lower_in_prop(body, ctx);
            if let Term::Var(v) = value
                && let Some(resolved) = var_seed(v, body, ctx, 0)
            {
                *seed = resolved;
            }
        }
        ValueExpr::Abs(operand) => lower_in_value(operand, ctx),
    }
}

fn lower_in_stmt(stmt: &mut Stmt, ctx: &SeedContext<'_>) {
    match stmt {
        Stmt::Require(p) | Stmt::BindOne(p) => lower_in_prop(p, ctx),
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

/// A definition can call a definition; genuine cycles are refused at
/// validation, so this bound only insures the pass against unvalidated
/// hand-built IR.
const MAX_DEFINED_DEPTH: usize = 16;

/// The seed for a variable summed over `body`: the declared kind of the
/// first claim position that binds it, descending into definition calls
/// by mapping the call argument onto the parameter it binds. `None`
/// (no summable position found - a pre-bound variable, a subject join)
/// leaves the decimal default standing.
fn var_seed(var: &Var, body: &Prop, ctx: &SeedContext<'_>, depth: usize) -> Option<SumSeed> {
    if depth > MAX_DEFINED_DEPTH {
        return None;
    }
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
        Prop::Defined { name, args } => {
            let def = ctx.definitions.iter().find(|d| &d.name == name)?;
            args.iter()
                .zip(&def.parameters)
                .filter(|(arg, _)| matches!(arg, Term::Var(v) if v == var))
                .find_map(|(_, param)| var_seed(param, &def.body, ctx, depth + 1))
        }
        Prop::And(props) | Prop::Or(props) => {
            props.iter().find_map(|p| var_seed(var, p, ctx, depth))
        }
        Prop::Implies { left, right } | Prop::Xor(left, right) => {
            var_seed(var, left, ctx, depth).or_else(|| var_seed(var, right, ctx, depth))
        }
        Prop::Not(p) | Prop::Exists { body: p, .. } | Prop::Pre(p) => var_seed(var, p, ctx, depth),
        Prop::Forall { source, body, .. } => {
            var_seed(var, source, ctx, depth).or_else(|| var_seed(var, body, ctx, depth))
        }
        Prop::In(_, _) | Prop::Eq(_, _) | Prop::Neq(_, _) | Prop::Compare { .. } => None,
    }
}
