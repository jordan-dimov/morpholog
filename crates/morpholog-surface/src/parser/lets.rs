//! Body-level `let` - parse-time substitution for `define` and
//! `invariant` bodies.
//!
//! A body `let` is an algebraic abbreviation, not a runtime binding:
//! its value is inlined at every use site before the IR exists, so
//! variables inside the value are ordinary Morpholog variables and
//! acquire exactly the meaning they have after substitution at the
//! use site (a value mentioning `x` used inside `exists x: ...` reads
//! the quantified `x` - deliberate, and pinned by test). The kernel
//! never sees a body `let`; the formatter emits the desugared form;
//! `canonical_hash` is identical for sugared and hand-desugared
//! sources - rules identity, not file identity.
//!
//! Refusals are parser-side by necessity (nothing remains in the IR
//! to blame) and deliberate: duplicate names, parameter collisions,
//! quantifier-binder collisions (refused rather than shadowed),
//! `actor` as a name, self- and forward-references (a let may use
//! earlier lets only), computed values in term-only positions, dead
//! bindings (transitively - a let used only by another dead let is
//! dead), and expansion past a node budget (substitution multiplies
//! nodes; a doubling chain grows exponentially while staying shallow,
//! which the kernel's depth guard cannot see).

use std::collections::{BTreeMap, BTreeSet};

use morpholog_core::{Prop, Term, ValueExpr, Var};

use super::walk::{
    Node, binders_in_prop, binders_in_value, prop_nodes, read_var, value_nodes, vars_in_prop,
    vars_in_value, walk_prop, walk_value,
};

use crate::diagnostics::Span;

/// One parsed `let name = (value)` line, spans kept for refusals.
/// `noun` is the diagnostic word - "let" here, "const" when the
/// programme-level pass reuses this machinery.
#[derive(Debug)]
pub(crate) struct LetBinding {
    pub(crate) name: String,
    pub(crate) value: ValueExpr,
    pub(crate) span: Span,
    pub(crate) noun: &'static str,
}

/// The expansion ceiling: substitution may not grow a body past this
/// many IR nodes. Far above any hand-authored rule; low enough that a
/// doubling chain refuses in milliseconds instead of exhausting
/// memory.
const MAX_BODY_NODES: usize = 16_384;

/// Apply a body's `let` prefix to its proposition. Returns the
/// substituted proposition plus every refusal found; on any refusal
/// the returned proposition is best-effort and the caller must treat
/// the parse as failed.
pub(crate) fn apply(
    bindings: Vec<LetBinding>,
    parameters: &[String],
    mut body: Prop,
) -> (Prop, Vec<(Span, String)>) {
    let mut errors: Vec<(Span, String)> = Vec::new();
    if bindings.is_empty() {
        return (body, errors);
    }

    // Name-level refusals first: actor, duplicates, parameter and
    // quantifier-binder collisions.
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut binders = BTreeSet::new();
    binders_in_prop(&body, &mut binders);
    for b in &bindings {
        binders_in_value(&b.value, &mut binders);
    }
    for b in &bindings {
        if b.name == "actor" {
            errors.push((
                b.span.clone(),
                "`actor` cannot name a let value - `actor` always denotes the \
                 proposing transition's actor"
                    .to_string(),
            ));
        }
        if !seen.insert(b.name.as_str()) {
            errors.push((
                b.span.clone(),
                format!(
                    "duplicate let `{}` in this body; each let names one value",
                    b.name
                ),
            ));
        }
        if parameters.iter().any(|p| p == &b.name) {
            errors.push((
                b.span.clone(),
                format!(
                    "let `{}` collides with a parameter of the same name",
                    b.name
                ),
            ));
        }
        if binders.contains(b.name.as_str()) {
            errors.push((
                b.span.clone(),
                format!(
                    "let `{}` collides with a quantifier binding of the same name \
                     in this body - rename one",
                    b.name
                ),
            ));
        }
    }
    if !errors.is_empty() {
        return (body, errors);
    }

    // Order refusals: a value may reference EARLIER lets only. Without
    // this, substitution order would accidentally resolve a forward
    // reference whenever the later let also appears in the body, and
    // report it dead otherwise - legality must not hinge on an
    // unrelated use. Self-reference is the degenerate case.
    let declaration_index: BTreeMap<&str, usize> = bindings
        .iter()
        .enumerate()
        .map(|(i, b)| (b.name.as_str(), i))
        .collect();
    for (i, b) in bindings.iter().enumerate() {
        let mut used = BTreeSet::new();
        vars_in_value(&b.value, &mut used);
        for name in &used {
            match declaration_index.get(name.as_str()) {
                Some(&j) if j == i => errors.push((
                    b.span.clone(),
                    format!("let `{}` references itself", b.name),
                )),
                Some(&j) if j > i => errors.push((
                    b.span.clone(),
                    format!(
                        "let `{}` references `{name}`, which is declared later - \
                         a let may use earlier lets only",
                        b.name
                    ),
                )),
                _ => {}
            }
        }
    }
    if !errors.is_empty() {
        return (body, errors);
    }

    // Liveness, backwards: a let is live when the body uses it, or a
    // LATER live let's value uses it. Anything else is dead and
    // refused - including a chain whose head is only used by its own
    // dead tail.
    let mut live_names: BTreeSet<String> = BTreeSet::new();
    vars_in_prop(&body, &mut live_names);
    let mut live = vec![false; bindings.len()];
    for (i, b) in bindings.iter().enumerate().rev() {
        if live_names.contains(&b.name) {
            live[i] = true;
            vars_in_value(&b.value, &mut live_names);
        }
    }
    for (i, b) in bindings.iter().enumerate() {
        if !live[i] {
            errors.push((b.span.clone(), format!("let `{}` is never used", b.name)));
        }
    }
    if !errors.is_empty() {
        return (body, errors);
    }

    // Expansion, one visit per value: resolve each value once against
    // the already-expanded earlier lets it actually references (the
    // order check above guarantees earlier-only), then substitute the
    // lets the body directly references. Expanded values are closed -
    // they contain no let names - so a long chain costs one
    // substitution per link, never a rewrite of every later value.
    let mut expanded: Vec<Option<ValueExpr>> = vec![None; bindings.len()];
    for (i, b) in bindings.iter().enumerate() {
        let mut value = b.value.clone();
        let mut used = BTreeSet::new();
        vars_in_value(&b.value, &mut used);
        let mut refusals = Vec::new();
        for name in &used {
            if let Some(&j) = declaration_index.get(name.as_str())
                && let Some(prior) = expanded[j].as_ref()
            {
                // Diagnostics blame the REFERENCED let - it owns the
                // computed value hitting a term slot, and its
                // expansion is what grows the tree.
                budgeted_substitute_value(
                    &mut value,
                    &Var::from(name.as_str()),
                    prior,
                    value_nodes(prior),
                    &bindings[j],
                    &mut refusals,
                );
            }
        }
        if !refusals.is_empty() {
            errors.extend(refusals);
            return (body, errors);
        }
        expanded[i] = Some(value);
    }
    let mut body_names = BTreeSet::new();
    vars_in_prop(&body, &mut body_names);
    for name in &body_names {
        if let Some(&i) = declaration_index.get(name.as_str())
            && let Some(value) = expanded[i].as_ref()
        {
            let mut refusals = Vec::new();
            budgeted_substitute_prop(
                &mut body,
                &Var::from(name.as_str()),
                value,
                value_nodes(value),
                &bindings[i],
                &mut refusals,
            );
            if !refusals.is_empty() {
                errors.extend(refusals);
                return (body, errors);
            }
        }
    }
    (body, errors)
}

pub(super) fn budgeted_substitute_prop(
    target: &mut Prop,
    name: &Var,
    value: &ValueExpr,
    value_nodes: usize,
    binding: &LetBinding,
    errors: &mut Vec<(Span, String)>,
) {
    if count_prop(target, name, false) == 0 {
        return;
    }
    // Budget on value-position occurrences only: a term-slot use is a
    // 1-for-1 term swap or a refusal, never growth - counting it would
    // inflate the projection and let the budget error mask the more
    // specific term-slot diagnostic.
    let growth_sites = count_prop(target, name, true);
    let projected = prop_nodes(target) + growth_sites * value_nodes.saturating_sub(1);
    if projected > MAX_BODY_NODES {
        errors.push((
            binding.span.clone(),
            format!(
                "expanding {} `{}` would grow this body past the expression \
                 budget - inline less, or split the rule",
                binding.noun, binding.name
            ),
        ));
        return;
    }
    substitute_in_prop(target, name, value, binding, errors);
}

pub(super) fn budgeted_substitute_value(
    target: &mut ValueExpr,
    name: &Var,
    value: &ValueExpr,
    value_nodes: usize,
    binding: &LetBinding,
    errors: &mut Vec<(Span, String)>,
) {
    if count_value(target, name, false) == 0 {
        return;
    }
    let growth_sites = count_value(target, name, true);
    let projected = self::value_nodes(target) + growth_sites * value_nodes.saturating_sub(1);
    if projected > MAX_BODY_NODES {
        errors.push((
            binding.span.clone(),
            format!(
                "expanding {} `{}` would grow this body past the expression \
                 budget - inline less, or split the rule",
                binding.noun, binding.name
            ),
        ));
        return;
    }
    substitute_in_value(target, name, value, binding, errors);
}

/// A computed value can only stand where a value expression stands; a
/// term-only position takes it solely when the value IS a plain term.
fn substitutable_term(value: &ValueExpr) -> Option<Term> {
    match value {
        ValueExpr::Term(t) => Some(t.clone()),
        ValueExpr::Arith { .. }
        | ValueExpr::Sum { .. }
        | ValueExpr::Extremum { .. }
        | ValueExpr::ValueOf { .. }
        | ValueExpr::Call { .. }
        | ValueExpr::Cond { .. } => None,
    }
}

pub(super) fn substitute_term_slot(
    term: &mut Term,
    name: &Var,
    value: &ValueExpr,
    binding: &LetBinding,
    where_: &str,
    errors: &mut Vec<(Span, String)>,
) {
    if !matches!(term, Term::Var(v) if v == name) {
        return;
    }
    match substitutable_term(value) {
        Some(t) => *term = t,
        None => errors.push((
            binding.span.clone(),
            format!(
                "computed {} `{}` is used {where_}, which takes plain terms \
                 only - match a variable there and compare it with `{}` \
                 separately",
                binding.noun, binding.name, binding.name
            ),
        )),
    }
}

pub(super) fn substitute_in_prop(
    prop: &mut Prop,
    name: &Var,
    value: &ValueExpr,
    binding: &LetBinding,
    errors: &mut Vec<(Span, String)>,
) {
    match prop {
        Prop::Claim { predicate, args } => {
            for (i, arg) in args.iter_mut().enumerate() {
                let where_ = format!("as argument {} of `{predicate}`", i + 1);
                substitute_term_slot(arg, name, value, binding, &where_, errors);
            }
        }
        Prop::Defined { name: callee, args } => {
            for (i, arg) in args.iter_mut().enumerate() {
                let where_ = format!("as argument {} of `{callee}`", i + 1);
                substitute_term_slot(arg, name, value, binding, &where_, errors);
            }
        }
        Prop::In(l, r) => {
            substitute_term_slot(l, name, value, binding, "as an `in` operand", errors);
            substitute_term_slot(r, name, value, binding, "as an `in` operand", errors);
        }
        Prop::And(props) | Prop::Or(props) => {
            for p in props {
                substitute_in_prop(p, name, value, binding, errors);
            }
        }
        Prop::Implies { left, right } | Prop::Xor(left, right) => {
            substitute_in_prop(left, name, value, binding, errors);
            substitute_in_prop(right, name, value, binding, errors);
        }
        Prop::Not(p) | Prop::Exists { body: p, .. } | Prop::Pre(p) => {
            substitute_in_prop(p, name, value, binding, errors);
        }
        Prop::Forall { source, body, .. } => {
            substitute_in_prop(source, name, value, binding, errors);
            substitute_in_prop(body, name, value, binding, errors);
        }
        Prop::Eq(l, r) | Prop::Neq(l, r) => {
            substitute_in_value(l, name, value, binding, errors);
            substitute_in_value(r, name, value, binding, errors);
        }
        Prop::Compare { left, right, .. } => {
            substitute_in_value(left, name, value, binding, errors);
            substitute_in_value(right, name, value, binding, errors);
        }
    }
}

pub(super) fn substitute_in_value(
    expr: &mut ValueExpr,
    name: &Var,
    value: &ValueExpr,
    binding: &LetBinding,
    errors: &mut Vec<(Span, String)>,
) {
    match expr {
        ValueExpr::Term(Term::Var(v)) if v == name => {
            *expr = value.clone();
        }
        ValueExpr::Term(_) => {}
        ValueExpr::Arith { left, right, .. } => {
            substitute_in_value(left, name, value, binding, errors);
            substitute_in_value(right, name, value, binding, errors);
        }
        ValueExpr::Sum {
            value: target,
            body,
            ..
        } => {
            substitute_in_value(target, name, value, binding, errors);
            substitute_in_prop(body, name, value, binding, errors);
        }
        ValueExpr::Extremum {
            op,
            value: target,
            body,
        } => {
            substitute_term_slot(
                target,
                name,
                value,
                binding,
                match op {
                    morpholog_core::ExtremumOp::Max => "as a max target",
                    morpholog_core::ExtremumOp::Min => "as a min target",
                },
                errors,
            );
            substitute_in_prop(body, name, value, binding, errors);
        }
        ValueExpr::Cond {
            when,
            then,
            otherwise,
        } => {
            substitute_in_prop(when, name, value, binding, errors);
            substitute_in_value(then, name, value, binding, errors);
            substitute_in_value(otherwise, name, value, binding, errors);
        }
        ValueExpr::ValueOf {
            predicate,
            args,
            extract: _,
            default,
        } => {
            for (i, arg) in args.iter_mut().enumerate() {
                let where_ = format!("as argument {} of the `value {predicate}` lookup", i + 1);
                substitute_term_slot(arg, name, value, binding, &where_, errors);
            }
            if let Some(d) = default {
                substitute_in_value(d, name, value, binding, errors);
            }
        }
        // Builtin arguments are ordinary value positions: a computed
        // let inlines whole, no term-slot restriction.
        ValueExpr::Call { args, .. } => {
            for a in args {
                substitute_in_value(a, name, value, binding, errors);
            }
        }
    }
}

// ------------------------------------------------------------
// Read-only walks: binder names, variable references, node counts.
// Exhaustive matches, no wildcard arms - a new IR variant must
// declare its behaviour here.
// ------------------------------------------------------------

/// Occurrences of `name` in the tree; with `growth_only`, only those in
/// value position, where substitution can enlarge the tree. Slot
/// occurrences swap one term for another or are refused.
fn count_prop(prop: &Prop, name: &Var, growth_only: bool) -> usize {
    let mut n = 0;
    walk_prop(prop, &mut |node| {
        n += usize::from(is_use(&node, name, growth_only))
    });
    n
}

fn count_value(expr: &ValueExpr, name: &Var, growth_only: bool) -> usize {
    let mut n = 0;
    walk_value(expr, &mut |node| {
        n += usize::from(is_use(&node, name, growth_only))
    });
    n
}

fn is_use(node: &Node<'_>, name: &Var, growth_only: bool) -> bool {
    read_var(node).is_some_and(|(v, in_value)| v == name && (in_value || !growth_only))
}
