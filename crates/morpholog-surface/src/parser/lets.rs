//! Body-level `let` in `define` and `invariant` bodies, substituted away at parse time.
//!
//! A body `let` is an abbreviation, not a runtime binding: its value is inlined at every use.
//! Variables inside it mean whatever they mean at the use site, so a value mentioning `x` used
//! inside `exists x: ...` reads the quantified `x`, on purpose. The kernel never sees the `let`,
//! so the sugared and hand-inlined forms hash the same.
//!
//! Every refusal must happen here, since nothing is left in the IR to blame: duplicate names,
//! collisions with parameters or quantifier binders, `actor` as a name, references to itself or
//! later lets, computed values in term-only positions, unused lets, and expansion past a size
//! budget. The budget matters because a chain of lets that each double the previous one grows
//! exponentially while staying shallow.

use std::collections::{BTreeMap, BTreeSet};

use morpholog_core::{Prop, Term, ValueExpr, Var};

use super::walk::{
    Node, binders_in_prop, binders_in_value, prop_nodes, read_var, value_nodes, vars_in_prop,
    vars_in_value, walk_prop, walk_value,
};

use crate::diagnostics::Span;

/// One parsed `let name = (value)` line. `noun` is the word diagnostics use: "let" or "const".
#[derive(Debug)]
pub(crate) struct LetBinding {
    pub(crate) name: String,
    pub(crate) value: ValueExpr,
    pub(crate) span: Span,
    pub(crate) noun: &'static str,
}

/// Substitution may not grow a body past this many IR nodes. Far above any hand-written rule,
/// low enough that a doubling chain fails fast instead of exhausting memory.
const MAX_BODY_NODES: usize = 16_384;

/// Apply a body's `let` prefix to its proposition, returning it with every refusal. After any
/// refusal the proposition is unreliable and the caller must treat the parse as failed.
pub(crate) fn apply(
    bindings: Vec<LetBinding>,
    parameters: &[String],
    mut body: Prop,
) -> (Prop, Vec<(Span, String)>) {
    let mut errors: Vec<(Span, String)> = Vec::new();
    if bindings.is_empty() {
        return (body, errors);
    }

    // Name refusals: actor, duplicates, parameter and quantifier-binder collisions.
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

    // A value may refer only to earlier lets. Otherwise a forward reference would work or not
    // depending on whether the body happened to use the later let too.
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

    // A let is used if the body uses it or a later used let does. A chain used only by its
    // own unused tail is unused too.
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

    // Expand each value once against the earlier, already-expanded lets, then substitute into
    // the body. Expanded values hold no let names, so a long chain costs one step per link.
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
                // Blame the referenced let: its value is what lands in the slot or grows the tree.
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
    // Count only value-position uses. A term-slot use never grows the tree, and counting it
    // could hide the more specific term-slot error behind a budget error.
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

/// The value as a plain term, if it is one. Only a plain term can fill a term-only position.
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
        // Builtin arguments are value positions, so any value can go in.
        ValueExpr::Call { args, .. } => {
            for a in args {
                substitute_in_value(a, name, value, binding, errors);
            }
        }
    }
}

// ------------------------------------------------------------
// Read-only walks.
// ------------------------------------------------------------

/// Occurrences of `name` in the tree. With `growth_only`, only those in value position, where
/// substitution can grow the tree.
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
