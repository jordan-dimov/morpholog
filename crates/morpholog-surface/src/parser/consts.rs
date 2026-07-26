//! Programme-level `const` - parse-time substitution across every
//! body in the file.
//!
//! A `const name = (value)` names one figure the whole rulebook
//! shares - a rounding quantum, a conversion divisor - and is
//! substituted away before the IR exists, exactly like body-level
//! `let` ([`super::lets`]): the kernel never sees it, the formatter
//! emits the inlined form, and `canonical_hash` is identical for the
//! named and hand-inlined spellings. What `const` adds over `let` is
//! REACH: it substitutes into invariant and define bodies, derived
//! `over` and `value` clauses, and transformation statements - the
//! contexts that have no local-naming alternative.
//!
//! Refusals, all parse-time with spans, per the shadowing-is-refused
//! doctrine: duplicate consts; `actor` as a name; self- and
//! forward-references among consts (earlier-only, like lets); a const
//! name colliding with ANY parameter, quantifier binder, statement
//! binding (`let`/`for`/`new Subject()`), derived key, or body-level
//! `let` anywhere in the programme - a programme-wide name must be
//! programme-wide unambiguous; computed consts in term-only slots;
//! and a const no body uses (dead vocabulary, transitively). Claim-
//! and bind-pattern variables are NOT binders here, mirroring `let`'s
//! algebraic doctrine: a const flowing into a pattern argument is an
//! ordinary term substitution.

use std::collections::{BTreeMap, BTreeSet};

use morpholog_core::{DerivedClaim, Invariant, Stmt, Term, Transformation, ValueExpr, Var};

use super::lets::{
    LetBinding, budgeted_substitute_prop, budgeted_substitute_value, collect_binders_in_prop,
    collect_binders_in_value, substitute_term_slot, value_nodes, vars_in_prop, vars_in_term,
    vars_in_value,
};
use crate::diagnostics::Span;

/// The declarations a const pass reads and rewrites, borrowed from the
/// parser's collected programme plus the body-`let` names the earlier
/// per-body pass consumed (they are substituted away by now, so the
/// collision check needs them carried forward).
pub(crate) struct ConstTargets<'a> {
    pub(crate) definitions: &'a mut [(morpholog_core::Definition, Span)],
    pub(crate) invariants: &'a mut [(Invariant, Span)],
    pub(crate) transformations: &'a mut [(Transformation, Span, Vec<Span>)],
    pub(crate) derived_claims: &'a mut [(DerivedClaim, Span)],
    pub(crate) body_let_names: &'a [(String, Span)],
}

/// Apply the programme's `const` declarations to every body. Returns
/// every refusal found; on any refusal the bodies are best-effort and
/// the caller must treat the parse as failed.
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

    // Collisions: a programme-wide name must be programme-wide
    // unambiguous. Locals of every flavour count; claim-pattern
    // variables deliberately do not.
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
            collect_binders_in_value(&v.expr, &mut set);
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

    // Order refusals among consts: earlier-only, never resolved by
    // substitution order.
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

    // Liveness across the whole programme, backwards through the
    // consts: used by any body, or by a later live const.
    let mut live_names: BTreeSet<String> = BTreeSet::new();
    for (d, _) in targets.definitions.iter() {
        vars_in_prop(&d.body, &mut live_names);
    }
    for (i, _) in targets.invariants.iter() {
        vars_in_prop(&i.body, &mut live_names);
    }
    for (t, _, _) in targets.transformations.iter() {
        for s in &t.body {
            stmt_vars(s, &mut live_names);
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
        Stmt::Require(p) | Stmt::BindOne(p) => substitute_prop(p, errors),
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
    collect_binders_in_prop(prop, &mut set);
    out.extend(set.into_iter().map(|n| (n, "quantifier binding")));
}

/// Statement-level locals: `let`, `for`, and `new Subject()` bindings,
/// plus quantifier binders inside statement propositions and values.
fn collect_stmt_locals(stmt: &Stmt, out: &mut Vec<(String, &'static str)>) {
    match stmt {
        Stmt::Require(p) | Stmt::BindOne(p) => collect_binders_named(p, out),
        Stmt::Let { name, value } => {
            out.push((name.to_string(), "statement binding"));
            let mut set = BTreeSet::new();
            collect_binders_in_value(value, &mut set);
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
            collect_binders_in_value(collection, &mut set);
            out.extend(set.into_iter().map(|n| (n, "quantifier binding")));
            for s in body {
                collect_stmt_locals(s, out);
            }
        }
    }
}

/// Free variables a statement reads or writes with - the usage scan
/// behind const liveness.
fn stmt_vars(stmt: &Stmt, out: &mut BTreeSet<String>) {
    match stmt {
        Stmt::Require(p) | Stmt::BindOne(p) => vars_in_prop(p, out),
        Stmt::Let { value, .. } => vars_in_value(value, out),
        Stmt::LetNewSubject { .. } => {}
        Stmt::Assert(c) => {
            for a in &c.args {
                vars_in_term(a, out);
            }
        }
        Stmt::Retract { args, .. } => {
            for a in args {
                vars_in_term(a, out);
            }
        }
        Stmt::Emit(intent) => {
            for a in &intent.args {
                vars_in_term(a, out);
            }
        }
        Stmt::For {
            collection, body, ..
        } => {
            vars_in_value(collection, out);
            for s in body {
                stmt_vars(s, out);
            }
        }
    }
}
