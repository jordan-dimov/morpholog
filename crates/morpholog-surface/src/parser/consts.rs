//! Programme-level `const`: a named value substituted into every body at parse time.
//!
//! `const name = (value)` names one figure the whole rulebook shares, such as a rounding
//! quantum. Like a body `let` ([`super::lets`]) it is gone before the IR exists, so naming a value
//! or inlining it by hand hashes the same. Unlike a `let`, it reaches every body in the file.
//!
//! A const is built only from literals and earlier consts, and may not appear where arguments
//! bind (claim patterns, definition calls, `bind`). Its name may not collide with any local
//! anywhere in the programme, and an unused const is refused.

use std::collections::{BTreeMap, BTreeSet};

use morpholog_core::{DerivedClaim, Invariant, Stmt, Term, Transformation, ValueExpr, Var};

use super::lets::{
    LetBinding, budgeted_substitute_prop, budgeted_substitute_value, substitute_term_slot,
};
use super::walk::{
    Node, binders_in_prop, binders_in_value, value_nodes, vars_in_prop, vars_in_stmt, vars_in_term,
    vars_in_value, walk_prop, walk_stmt, walk_value,
};
use crate::diagnostics::Span;

/// The declarations the const pass rewrites, plus the body `let` names it checks for collisions
/// (the lets themselves are already substituted away).
pub(crate) struct ConstTargets<'a> {
    pub(crate) definitions: &'a mut [(morpholog_core::Definition, Span)],
    pub(crate) invariants: &'a mut [(Invariant, Span)],
    pub(crate) transformations: &'a mut [(Transformation, Span, Vec<Span>)],
    pub(crate) derived_claims: &'a mut [(DerivedClaim, Span)],
    pub(crate) body_let_names: &'a [(String, Span)],
}

/// Apply the programme's `const` declarations to every body and return every refusal. After any
/// refusal the bodies are unreliable and the caller must treat the parse as failed.
pub(crate) fn apply(consts: Vec<LetBinding>, targets: ConstTargets<'_>) -> Vec<(Span, String)> {
    let mut errors: Vec<(Span, String)> = Vec::new();
    if consts.is_empty() {
        return errors;
    }

    // Name-level refusals: actor, duplicates.
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for c in &consts {
        if c.name == "actor" {
            errors.push((
                c.span.clone(),
                "`actor` cannot name a const - `actor` always denotes the \
                 proposing transition's actor"
                    .to_string(),
            ));
        }
        if !seen.insert(c.name.as_str()) {
            errors.push((
                c.span.clone(),
                format!("duplicate const `{}`; each const names one value", c.name),
            ));
        }
    }

    // A programme-wide name must not clash with any local. Claim-pattern variables do not count.
    let const_names: BTreeSet<&str> = consts.iter().map(|c| c.name.as_str()).collect();
    let mut locals: Vec<(String, &'static str)> = Vec::new();
    for (d, _) in targets.definitions.iter() {
        for p in &d.parameters {
            locals.push((p.to_string(), "parameter"));
        }
        collect_binders_named(&d.body, &mut locals);
    }
    for (i, _) in targets.invariants.iter() {
        collect_binders_named(&i.body, &mut locals);
    }
    for (t, _, _) in targets.transformations.iter() {
        for p in &t.parameters {
            locals.push((p.to_string(), "parameter"));
        }
        for s in &t.body {
            collect_stmt_locals(s, &mut locals);
        }
    }
    for (d, _) in targets.derived_claims.iter() {
        for k in &d.keys {
            locals.push((k.to_string(), "derived key"));
        }
        collect_binders_named(&d.domain, &mut locals);
        for v in &d.values {
            let mut set = BTreeSet::new();
            binders_in_value(&v.expr, &mut set);
            locals.extend(set.into_iter().map(|n| (n, "quantifier binding")));
        }
    }
    for c in &consts {
        if let Some((_, what)) = locals.iter().find(|(n, _)| n == &c.name) {
            errors.push((
                c.span.clone(),
                format!(
                    "const `{}` collides with a {what} of the same name - a \
                     programme-wide name must not be shadowed; rename one",
                    c.name
                ),
            ));
        }
    }
    for (name, span) in targets.body_let_names {
        if const_names.contains(name.as_str()) {
            errors.push((
                span.clone(),
                format!(
                    "let `{name}` collides with the programme-level const of \
                     the same name - rename one"
                ),
            ));
        }
    }
    if !errors.is_empty() {
        return errors;
    }

    // A const may refer only to earlier consts.
    let declaration_index: BTreeMap<&str, usize> = consts
        .iter()
        .enumerate()
        .map(|(i, c)| (c.name.as_str(), i))
        .collect();
    for (i, c) in consts.iter().enumerate() {
        let mut used = BTreeSet::new();
        vars_in_value(&c.value, &mut used);
        for name in &used {
            match declaration_index.get(name.as_str()) {
                Some(&j) if j == i => errors.push((
                    c.span.clone(),
                    format!("const `{}` references itself", c.name),
                )),
                Some(&j) if j > i => errors.push((
                    c.span.clone(),
                    format!(
                        "const `{}` references `{name}`, which is declared later - \
                         a const may use earlier consts only",
                        c.name
                    ),
                )),
                _ => {}
            }
        }
    }
    if !errors.is_empty() {
        return errors;
    }

    // Only literals and earlier consts. A free variable would pick up whatever local exists at
    // each use site; `actor` changes per proposal; `sum` and `value` read state.
    for c in &consts {
        refuse_open_initialiser(c, &const_names, &mut errors);
    }
    if !errors.is_empty() {
        return errors;
    }

    // Where arguments bind (claim patterns, definition calls, `bind`), a const would silently
    // turn a binding into a filter, changing a rule from far away. Slots that do not bind, such
    // as `admit` arguments or `value` lookup keys, are ordinary uses.
    for (d, span) in targets.definitions.iter() {
        walk_prop(&d.body, &mut |n| {
            refuse_pattern_node(&n, &const_names, span, &mut errors)
        });
    }
    for (i, span) in targets.invariants.iter() {
        walk_prop(&i.body, &mut |n| {
            refuse_pattern_node(&n, &const_names, span, &mut errors)
        });
    }
    for (t, span, _) in targets.transformations.iter() {
        for s in &t.body {
            walk_stmt(s, &mut |n| {
                refuse_pattern_node(&n, &const_names, span, &mut errors)
            });
        }
    }
    for (d, span) in targets.derived_claims.iter() {
        walk_prop(&d.domain, &mut |n| {
            refuse_pattern_node(&n, &const_names, span, &mut errors)
        });
        for v in &d.values {
            walk_value(&v.expr, &mut |n| {
                refuse_pattern_node(&n, &const_names, span, &mut errors)
            });
        }
    }
    if !errors.is_empty() {
        return errors;
    }

    // A const is used if a body uses it or a later used const does.
    let mut live_names: BTreeSet<String> = BTreeSet::new();
    for (d, _) in targets.definitions.iter() {
        vars_in_prop(&d.body, &mut live_names);
    }
    for (i, _) in targets.invariants.iter() {
        vars_in_prop(&i.body, &mut live_names);
    }
    for (t, _, _) in targets.transformations.iter() {
        for s in &t.body {
            vars_in_stmt(s, &mut live_names);
        }
    }
    for (d, _) in targets.derived_claims.iter() {
        vars_in_prop(&d.domain, &mut live_names);
        for v in &d.values {
            vars_in_value(&v.expr, &mut live_names);
        }
    }
    let mut live = vec![false; consts.len()];
    for (i, c) in consts.iter().enumerate().rev() {
        if live_names.contains(&c.name) {
            live[i] = true;
            vars_in_value(&c.value, &mut live_names);
        }
    }
    for (i, c) in consts.iter().enumerate() {
        if !live[i] {
            errors.push((c.span.clone(), format!("const `{}` is never used", c.name)));
        }
    }
    if !errors.is_empty() {
        return errors;
    }

    // Expand each const once against the earlier expansions, then
    // substitute into every body under the per-body budget.
    let mut expanded: Vec<Option<ValueExpr>> = vec![None; consts.len()];
    for (i, c) in consts.iter().enumerate() {
        let mut value = c.value.clone();
        let mut used = BTreeSet::new();
        vars_in_value(&c.value, &mut used);
        for name in &used {
            if let Some(&j) = declaration_index.get(name.as_str())
                && let Some(prior) = expanded[j].as_ref()
            {
                budgeted_substitute_value(
                    &mut value,
                    &Var::from(name.as_str()),
                    prior,
                    value_nodes(prior),
                    &consts[j],
                    &mut errors,
                );
            }
        }
        if !errors.is_empty() {
            return errors;
        }
        expanded[i] = Some(value);
    }

    let substitute_prop = |body: &mut morpholog_core::Prop, errors: &mut Vec<(Span, String)>| {
        let mut names = BTreeSet::new();
        vars_in_prop(body, &mut names);
        for name in &names {
            if let Some(&i) = declaration_index.get(name.as_str())
                && let Some(value) = expanded[i].as_ref()
            {
                budgeted_substitute_prop(
                    body,
                    &Var::from(name.as_str()),
                    value,
                    value_nodes(value),
                    &consts[i],
                    errors,
                );
            }
        }
    };
    let substitute_value = |expr: &mut ValueExpr, errors: &mut Vec<(Span, String)>| {
        let mut names = BTreeSet::new();
        vars_in_value(expr, &mut names);
        for name in &names {
            if let Some(&i) = declaration_index.get(name.as_str())
                && let Some(value) = expanded[i].as_ref()
            {
                budgeted_substitute_value(
                    expr,
                    &Var::from(name.as_str()),
                    value,
                    value_nodes(value),
                    &consts[i],
                    errors,
                );
            }
        }
    };
    let substitute_terms = |args: &mut [Term], describe: &str, errors: &mut Vec<(Span, String)>| {
        for (n, arg) in args.iter_mut().enumerate() {
            let mut names = BTreeSet::new();
            vars_in_term(arg, &mut names);
            for name in &names {
                if let Some(&i) = declaration_index.get(name.as_str())
                    && let Some(value) = expanded[i].as_ref()
                {
                    let where_ = format!("as argument {} of {describe}", n + 1);
                    substitute_term_slot(
                        arg,
                        &Var::from(name.as_str()),
                        value,
                        &consts[i],
                        &where_,
                        errors,
                    );
                }
            }
        }
    };

    for (d, _) in targets.definitions.iter_mut() {
        substitute_prop(&mut d.body, &mut errors);
    }
    for (i, _) in targets.invariants.iter_mut() {
        substitute_prop(&mut i.body, &mut errors);
    }
    for (d, _) in targets.derived_claims.iter_mut() {
        substitute_prop(&mut d.domain, &mut errors);
        for v in &mut d.values {
            substitute_value(&mut v.expr, &mut errors);
        }
    }
    for (t, _, _) in targets.transformations.iter_mut() {
        for s in &mut t.body {
            substitute_stmt(
                s,
                &substitute_prop,
                &substitute_value,
                &substitute_terms,
                &mut errors,
            );
        }
    }
    errors
}

fn substitute_stmt(
    stmt: &mut Stmt,
    substitute_prop: &impl Fn(&mut morpholog_core::Prop, &mut Vec<(Span, String)>),
    substitute_value: &impl Fn(&mut ValueExpr, &mut Vec<(Span, String)>),
    substitute_terms: &impl Fn(&mut [Term], &str, &mut Vec<(Span, String)>),
    errors: &mut Vec<(Span, String)>,
) {
    match stmt {
        Stmt::Require { prop: p, .. } | Stmt::BindOne { prop: p, .. } => substitute_prop(p, errors),
        Stmt::Let { value, .. } => substitute_value(value, errors),
        Stmt::LetNewSubject { .. } => {}
        Stmt::Assert(c) => {
            let describe = format!("`{}`", c.predicate);
            substitute_terms(&mut c.args, &describe, errors);
        }
        Stmt::Retract { predicate, args } => {
            let describe = format!("`{predicate}`");
            substitute_terms(args, &describe, errors);
        }
        Stmt::Emit(intent) => {
            let describe = format!("`{}`", intent.name);
            substitute_terms(&mut intent.args, &describe, errors);
        }
        Stmt::For {
            collection, body, ..
        } => {
            substitute_value(collection, errors);
            for s in body {
                substitute_stmt(
                    s,
                    substitute_prop,
                    substitute_value,
                    substitute_terms,
                    errors,
                );
            }
        }
    }
}

/// Quantifier binders in a proposition, labelled for the collision
/// diagnostic.
fn collect_binders_named(prop: &morpholog_core::Prop, out: &mut Vec<(String, &'static str)>) {
    let mut set = BTreeSet::new();
    binders_in_prop(prop, &mut set);
    out.extend(set.into_iter().map(|n| (n, "quantifier binding")));
}

/// Statement-level locals: `let`, `for`, and `new Subject()` bindings,
/// plus quantifier binders inside statement propositions and values.
fn collect_stmt_locals(stmt: &Stmt, out: &mut Vec<(String, &'static str)>) {
    match stmt {
        Stmt::Require { prop: p, .. } | Stmt::BindOne { prop: p, .. } => {
            collect_binders_named(p, out)
        }
        Stmt::Let { name, value } => {
            out.push((name.to_string(), "statement binding"));
            let mut set = BTreeSet::new();
            binders_in_value(value, &mut set);
            out.extend(set.into_iter().map(|n| (n, "quantifier binding")));
        }
        Stmt::LetNewSubject { name } => out.push((name.to_string(), "statement binding")),
        Stmt::Assert(_) | Stmt::Retract { .. } | Stmt::Emit(_) => {}
        Stmt::For {
            binding,
            collection,
            body,
        } => {
            out.push((binding.to_string(), "statement binding"));
            let mut set = BTreeSet::new();
            binders_in_value(collection, &mut set);
            out.extend(set.into_iter().map(|n| (n, "quantifier binding")));
            for s in body {
                collect_stmt_locals(s, out);
            }
        }
    }
}

/// A const initialiser must be closed: literals and earlier consts
/// only. Reports every violation with its reason.
fn refuse_open_initialiser(
    c: &LetBinding,
    const_names: &BTreeSet<&str>,
    errors: &mut Vec<(Span, String)>,
) {
    fn walk(
        expr: &ValueExpr,
        c: &LetBinding,
        const_names: &BTreeSet<&str>,
        errors: &mut Vec<(Span, String)>,
    ) {
        match expr {
            ValueExpr::Term(Term::Var(v)) => {
                if !const_names.contains(v.as_str()) {
                    errors.push((
                        c.span.clone(),
                        format!(
                            "const `{}` references `{v}`, which is not a const - a \
                             const is built from literals and earlier consts only \
                             (a free variable would mean something different at \
                             every use site)",
                            c.name
                        ),
                    ));
                }
            }
            ValueExpr::Term(Term::Actor) => errors.push((
                c.span.clone(),
                format!(
                    "const `{}` references `actor`, which varies with every \
                     proposal - not a constant",
                    c.name
                ),
            )),
            ValueExpr::Term(Term::Wildcard) => errors.push((
                c.span.clone(),
                format!(
                    "const `{}` contains a wildcard, which is not a value",
                    c.name
                ),
            )),
            ValueExpr::Term(Term::Literal(_)) => {}
            ValueExpr::Arith { left, right, .. } => {
                walk(left, c, const_names, errors);
                walk(right, c, const_names, errors);
            }
            // A builtin is pure, so it is constant when its arguments are.
            ValueExpr::Call { args, .. } => {
                for a in args {
                    walk(a, c, const_names, errors);
                }
            }
            ValueExpr::Cond { .. } => errors.push((
                c.span.clone(),
                format!(
                    "const `{}` contains `if`, which evaluates a proposition - \
                     a decision belongs in a rule, not a const",
                    c.name
                ),
            )),
            // Name only the construct the author wrote.
            ValueExpr::Sum { .. } | ValueExpr::Extremum { .. } | ValueExpr::ValueOf { .. } => {
                let construct = match expr {
                    ValueExpr::Sum { .. } => "`sum`",
                    ValueExpr::Extremum { op, .. } => match op {
                        morpholog_core::ExtremumOp::Max => "`max(.. | ..)`",
                        morpholog_core::ExtremumOp::Min => "`min(.. | ..)`",
                    },
                    _ => "`value`",
                };
                errors.push((
                    c.span.clone(),
                    format!(
                        "const `{}` reads state ({construct}) - a figure that \
                     changes with the ledger belongs in a rule, not a const",
                        c.name
                    ),
                ))
            }
        }
    }
    walk(&c.value, c, const_names, errors);
}

fn refuse_pattern_slot(
    args: &[Term],
    shape: &str,
    const_names: &BTreeSet<&str>,
    decl_span: &Span,
    errors: &mut Vec<(Span, String)>,
) {
    for arg in args {
        if let Term::Var(v) = arg
            && const_names.contains(v.as_str())
        {
            errors.push((
                decl_span.clone(),
                format!(
                    "const `{v}` stands in {shape} in this declaration - pattern \
                     arguments bind relationally, and a constant there would \
                     silently filter instead of bind; match a variable and \
                     compare it with `{v}` explicitly"
                ),
            ));
        }
    }
}

/// Refuse a const in a claim pattern, where it would filter instead of bind. Definition calls
/// are not resolved yet when this runs; if that changes, they get the same message.
fn refuse_pattern_node(
    node: &Node<'_>,
    const_names: &BTreeSet<&str>,
    decl_span: &Span,
    errors: &mut Vec<(Span, String)>,
) {
    use morpholog_core::Prop;
    let Node::Prop(prop) = node else { return };
    let (shape, args) = match prop {
        Prop::Claim { predicate, args } => (format!("the `{predicate}` claim pattern"), args),
        Prop::Defined { name, args } => (format!("the `{name}` claim pattern"), args),
        _ => return,
    };
    refuse_pattern_slot(args, &shape, const_names, decl_span, errors);
}
