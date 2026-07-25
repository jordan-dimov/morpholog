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
//! `actor` as a name, computed values in term-only positions, dead
//! bindings (transitively - a let used only by another dead let is
//! dead), and expansion past a node budget (substitution multiplies
//! nodes; a doubling chain grows exponentially while staying shallow,
//! which the kernel's depth guard cannot see).

use std::collections::BTreeSet;

use morpholog_core::{Prop, Term, ValueExpr, Var};

use crate::diagnostics::Span;

/// One parsed `let name = (value)` line, spans kept for refusals.
pub(crate) struct LetBinding {
    pub(crate) name: String,
    pub(crate) value: ValueExpr,
    pub(crate) span: Span,
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
    collect_binders_in_prop(&body, &mut binders);
    for b in &bindings {
        collect_binders_in_value(&b.value, &mut binders);
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

    // Substitute in declaration order: each live binding into every
    // LATER live binding's value, then into the body, under the node
    // budget.
    let mut values: Vec<ValueExpr> = bindings.iter().map(|b| b.value.clone()).collect();
    for i in 0..bindings.len() {
        if !live[i] {
            continue;
        }
        let name = Var::from(bindings[i].name.as_str());
        let value = values[i].clone();
        let value_nodes = value_nodes(&value);
        let mut refusals = Vec::new();
        for (j, later) in values.iter_mut().enumerate().skip(i + 1) {
            if live[j] {
                budgeted_substitute_value(
                    later,
                    &name,
                    &value,
                    value_nodes,
                    &bindings[i],
                    &mut refusals,
                );
            }
        }
        budgeted_substitute_prop(
            &mut body,
            &name,
            &value,
            value_nodes,
            &bindings[i],
            &mut refusals,
        );
        if !refusals.is_empty() {
            errors.extend(refusals);
            return (body, errors);
        }
    }
    (body, errors)
}

fn budgeted_substitute_prop(
    target: &mut Prop,
    name: &Var,
    value: &ValueExpr,
    value_nodes: usize,
    binding: &LetBinding,
    errors: &mut Vec<(Span, String)>,
) {
    let occurrences = count_prop(target, name);
    if occurrences == 0 {
        return;
    }
    let projected = prop_nodes(target) + occurrences * value_nodes.saturating_sub(1);
    if projected > MAX_BODY_NODES {
        errors.push((
            binding.span.clone(),
            format!(
                "expanding let `{}` would grow this body past the expression \
                 budget - inline less, or split the rule",
                binding.name
            ),
        ));
        return;
    }
    substitute_in_prop(target, name, value, binding, errors);
}

fn budgeted_substitute_value(
    target: &mut ValueExpr,
    name: &Var,
    value: &ValueExpr,
    value_nodes: usize,
    binding: &LetBinding,
    errors: &mut Vec<(Span, String)>,
) {
    let occurrences = count_value(target, name);
    if occurrences == 0 {
        return;
    }
    let projected = self::value_nodes(target) + occurrences * value_nodes.saturating_sub(1);
    if projected > MAX_BODY_NODES {
        errors.push((
            binding.span.clone(),
            format!(
                "expanding let `{}` would grow this body past the expression \
                 budget - inline less, or split the rule",
                binding.name
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
        | ValueExpr::ValueOf { .. }
        | ValueExpr::Abs(_) => None,
    }
}

fn substitute_term_slot(
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
                "computed let `{}` is used {where_}, which takes plain terms \
                 only - match a variable there and compare it with `{}` \
                 separately",
                binding.name, binding.name
            ),
        )),
    }
}

fn substitute_in_prop(
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

fn substitute_in_value(
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
            substitute_term_slot(target, name, value, binding, "as a sum target", errors);
            substitute_in_prop(body, name, value, binding, errors);
        }
        ValueExpr::ValueOf {
            predicate,
            args,
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
        ValueExpr::Abs(operand) => substitute_in_value(operand, name, value, binding, errors),
    }
}

// ------------------------------------------------------------
// Read-only walks: binder names, variable references, node counts.
// Exhaustive matches, no wildcard arms - a new IR variant must
// declare its behaviour here.
// ------------------------------------------------------------

fn collect_binders_in_prop(prop: &Prop, out: &mut BTreeSet<String>) {
    match prop {
        Prop::Exists { binding, body } => {
            out.insert(binding.to_string());
            collect_binders_in_prop(body, out);
        }
        Prop::Forall {
            binding,
            source,
            body,
        } => {
            out.insert(binding.to_string());
            collect_binders_in_prop(source, out);
            collect_binders_in_prop(body, out);
        }
        Prop::And(props) | Prop::Or(props) => {
            for p in props {
                collect_binders_in_prop(p, out);
            }
        }
        Prop::Implies { left, right } | Prop::Xor(left, right) => {
            collect_binders_in_prop(left, out);
            collect_binders_in_prop(right, out);
        }
        Prop::Not(p) | Prop::Pre(p) => collect_binders_in_prop(p, out),
        Prop::Eq(l, r) | Prop::Neq(l, r) => {
            collect_binders_in_value(l, out);
            collect_binders_in_value(r, out);
        }
        Prop::Compare { left, right, .. } => {
            collect_binders_in_value(left, out);
            collect_binders_in_value(right, out);
        }
        Prop::Claim { .. } | Prop::Defined { .. } | Prop::In(_, _) => {}
    }
}

fn collect_binders_in_value(expr: &ValueExpr, out: &mut BTreeSet<String>) {
    match expr {
        // The sum target is bound by the sum body - a binder.
        ValueExpr::Sum {
            value: target,
            body,
            ..
        } => {
            if let Term::Var(v) = target {
                out.insert(v.to_string());
            }
            collect_binders_in_prop(body, out);
        }
        ValueExpr::Arith { left, right, .. } => {
            collect_binders_in_value(left, out);
            collect_binders_in_value(right, out);
        }
        ValueExpr::ValueOf { default, .. } => {
            if let Some(d) = default {
                collect_binders_in_value(d, out);
            }
        }
        ValueExpr::Abs(operand) => collect_binders_in_value(operand, out),
        ValueExpr::Term(_) => {}
    }
}

fn vars_in_term(term: &Term, out: &mut BTreeSet<String>) {
    match term {
        Term::Var(v) => {
            out.insert(v.to_string());
        }
        Term::Wildcard | Term::Literal(_) | Term::Actor => {}
    }
}

fn vars_in_prop(prop: &Prop, out: &mut BTreeSet<String>) {
    match prop {
        Prop::Claim { args, .. } | Prop::Defined { args, .. } => {
            for a in args {
                vars_in_term(a, out);
            }
        }
        Prop::In(l, r) => {
            vars_in_term(l, out);
            vars_in_term(r, out);
        }
        Prop::And(props) | Prop::Or(props) => {
            for p in props {
                vars_in_prop(p, out);
            }
        }
        Prop::Implies { left, right } | Prop::Xor(left, right) => {
            vars_in_prop(left, out);
            vars_in_prop(right, out);
        }
        Prop::Not(p) | Prop::Exists { body: p, .. } | Prop::Pre(p) => vars_in_prop(p, out),
        Prop::Forall { source, body, .. } => {
            vars_in_prop(source, out);
            vars_in_prop(body, out);
        }
        Prop::Eq(l, r) | Prop::Neq(l, r) => {
            vars_in_value(l, out);
            vars_in_value(r, out);
        }
        Prop::Compare { left, right, .. } => {
            vars_in_value(left, out);
            vars_in_value(right, out);
        }
    }
}

fn vars_in_value(expr: &ValueExpr, out: &mut BTreeSet<String>) {
    match expr {
        ValueExpr::Term(t) => vars_in_term(t, out),
        ValueExpr::Arith { left, right, .. } => {
            vars_in_value(left, out);
            vars_in_value(right, out);
        }
        ValueExpr::Sum {
            value: target,
            body,
            ..
        } => {
            vars_in_term(target, out);
            vars_in_prop(body, out);
        }
        ValueExpr::ValueOf { args, default, .. } => {
            for a in args {
                vars_in_term(a, out);
            }
            if let Some(d) = default {
                vars_in_value(d, out);
            }
        }
        ValueExpr::Abs(operand) => vars_in_value(operand, out),
    }
}

fn count_term(term: &Term, name: &Var) -> usize {
    usize::from(matches!(term, Term::Var(v) if v == name))
}

fn count_prop(prop: &Prop, name: &Var) -> usize {
    match prop {
        Prop::Claim { args, .. } | Prop::Defined { args, .. } => {
            args.iter().map(|a| count_term(a, name)).sum()
        }
        Prop::In(l, r) => count_term(l, name) + count_term(r, name),
        Prop::And(props) | Prop::Or(props) => props.iter().map(|p| count_prop(p, name)).sum(),
        Prop::Implies { left, right } | Prop::Xor(left, right) => {
            count_prop(left, name) + count_prop(right, name)
        }
        Prop::Not(p) | Prop::Exists { body: p, .. } | Prop::Pre(p) => count_prop(p, name),
        Prop::Forall { source, body, .. } => count_prop(source, name) + count_prop(body, name),
        Prop::Eq(l, r) | Prop::Neq(l, r) => count_value(l, name) + count_value(r, name),
        Prop::Compare { left, right, .. } => count_value(left, name) + count_value(right, name),
    }
}

fn count_value(expr: &ValueExpr, name: &Var) -> usize {
    match expr {
        ValueExpr::Term(t) => count_term(t, name),
        ValueExpr::Arith { left, right, .. } => count_value(left, name) + count_value(right, name),
        ValueExpr::Sum {
            value: target,
            body,
            ..
        } => count_term(target, name) + count_prop(body, name),
        ValueExpr::ValueOf { args, default, .. } => {
            args.iter().map(|a| count_term(a, name)).sum::<usize>()
                + default.as_ref().map_or(0, |d| count_value(d, name))
        }
        ValueExpr::Abs(operand) => count_value(operand, name),
    }
}

fn prop_nodes(prop: &Prop) -> usize {
    1 + match prop {
        Prop::Claim { args, .. } | Prop::Defined { args, .. } => args.len(),
        Prop::In(_, _) => 2,
        Prop::And(props) | Prop::Or(props) => props.iter().map(prop_nodes).sum(),
        Prop::Implies { left, right } | Prop::Xor(left, right) => {
            prop_nodes(left) + prop_nodes(right)
        }
        Prop::Not(p) | Prop::Exists { body: p, .. } | Prop::Pre(p) => prop_nodes(p),
        Prop::Forall { source, body, .. } => prop_nodes(source) + prop_nodes(body),
        Prop::Eq(l, r) | Prop::Neq(l, r) => value_nodes(l) + value_nodes(r),
        Prop::Compare { left, right, .. } => value_nodes(left) + value_nodes(right),
    }
}

fn value_nodes(expr: &ValueExpr) -> usize {
    1 + match expr {
        ValueExpr::Term(_) => 0,
        ValueExpr::Arith { left, right, .. } => value_nodes(left) + value_nodes(right),
        ValueExpr::Sum { body, .. } => 1 + prop_nodes(body),
        ValueExpr::ValueOf { args, default, .. } => {
            args.len() + default.as_ref().map_or(0, |d| value_nodes(d))
        }
        ValueExpr::Abs(operand) => value_nodes(operand),
    }
}
