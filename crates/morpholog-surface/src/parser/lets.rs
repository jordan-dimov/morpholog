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
    if count_prop(target, name) == 0 {
        return;
    }
    // Budget on value-position occurrences only: a term-slot use is a
    // 1-for-1 term swap or a refusal, never growth - counting it would
    // inflate the projection and let the budget error mask the more
    // specific term-slot diagnostic.
    let growth_sites = count_growth_prop(target, name);
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
    if count_value(target, name) == 0 {
        return;
    }
    let growth_sites = count_growth_value(target, name);
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
        | ValueExpr::Abs(_)
        | ValueExpr::Round { .. }
        | ValueExpr::Cond { .. }
        | ValueExpr::PeriodIndex { .. } => None,
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

/// Inline one `let` binding everywhere it is used.
///
/// Two kinds of site, and the difference is the whole refusal story:
/// a value position takes the whole expression, while a term slot -
/// a claim or call argument, a membership operand, a sum target, a
/// lookup key - takes a plain term or nothing. The slot the walk
/// hands over is what lets the refusal say WHERE, which is the only
/// part of this an author can act on.
struct Substitute<'a> {
    name: &'a Var,
    value: &'a ValueExpr,
    binding: &'a LetBinding,
    errors: &'a mut Vec<(Span, String)>,
}

fn slot_prose(slot: morpholog_core::fold::TermSlot<'_>) -> String {
    use morpholog_core::fold::TermSlot;
    match slot {
        TermSlot::ClaimArg { predicate, index } => {
            format!("as argument {} of `{predicate}`", index + 1)
        }
        TermSlot::DefinedArg { callee, index } => {
            format!("as argument {} of `{callee}`", index + 1)
        }
        TermSlot::LookupArg { predicate, index } => {
            format!(
                "as argument {} of the `value {predicate}` lookup",
                index + 1
            )
        }
        TermSlot::InOperand => "as an `in` operand".to_string(),
        TermSlot::SumTarget => "as a sum target".to_string(),
        TermSlot::ExtremumTarget { op } => match op {
            morpholog_core::ExtremumOp::Max => "as a max target".to_string(),
            morpholog_core::ExtremumOp::Min => "as a min target".to_string(),
        },
        // A body `let` reaches invariant and define bodies only, so
        // neither statement arguments nor a bare value slot arrive
        // here - the value hook takes the latter before the walk
        // descends.
        TermSlot::StatementArg | TermSlot::Value => "in a term position".to_string(),
    }
}

impl morpholog_core::fold::Rewrite for Substitute<'_> {
    fn value(&mut self, expr: &mut ValueExpr) -> morpholog_core::fold::Descend {
        if matches!(expr, ValueExpr::Term(Term::Var(v)) if v == self.name) {
            *expr = self.value.clone();
            // The substituted copy is the binding's own value, already
            // expanded against earlier lets; walking into it again
            // would re-substitute what is not there to substitute.
            return morpholog_core::fold::Descend::Skip;
        }
        morpholog_core::fold::Descend::Into
    }

    fn term(&mut self, term: &mut Term, slot: morpholog_core::fold::TermSlot<'_>) {
        if !matches!(term, Term::Var(v) if v == self.name) {
            return;
        }
        match substitutable_term(self.value) {
            Some(t) => *term = t,
            None => {
                let where_ = slot_prose(slot);
                self.errors.push((
                    self.binding.span.clone(),
                    format!(
                        "computed {} `{}` is used {where_}, which takes plain terms \
                         only - match a variable there and compare it with `{}` \
                         separately",
                        self.binding.noun, self.binding.name, self.binding.name
                    ),
                ));
            }
        }
    }
}

pub(super) fn substitute_in_prop(
    prop: &mut Prop,
    name: &Var,
    value: &ValueExpr,
    binding: &LetBinding,
    errors: &mut Vec<(Span, String)>,
) {
    let mut s = Substitute {
        name,
        value,
        binding,
        errors,
    };
    morpholog_core::fold::rewrite_prop(prop, &mut s);
}

pub(super) fn substitute_in_value(
    expr: &mut ValueExpr,
    name: &Var,
    value: &ValueExpr,
    binding: &LetBinding,
    errors: &mut Vec<(Span, String)>,
) {
    let mut s = Substitute {
        name,
        value,
        binding,
        errors,
    };
    morpholog_core::fold::rewrite_value(expr, &mut s);
}

// ------------------------------------------------------------
// Read-only walks: binder names, variable references, node counts.
// Exhaustive matches, no wildcard arms - a new IR variant must
// declare its behaviour here.
// ------------------------------------------------------------

pub(super) fn collect_binders_in_prop(prop: &Prop, out: &mut BTreeSet<String>) {
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

pub(super) fn collect_binders_in_value(expr: &ValueExpr, out: &mut BTreeSet<String>) {
    match expr {
        // The sum target is CONSUMED against the bindings the sum body
        // supplies - it introduces nothing, so it is not a binder. A
        // let flowing into it is an ordinary term-slot substitution.
        ValueExpr::Sum { body, .. } | ValueExpr::Extremum { body, .. } => {
            collect_binders_in_prop(body, out)
        }
        // The condition's quantifier binders count for collision
        // detection like a sum body's; the branches recurse.
        ValueExpr::Cond {
            when,
            then,
            otherwise,
        } => {
            collect_binders_in_prop(when, out);
            collect_binders_in_value(then, out);
            collect_binders_in_value(otherwise, out);
        }
        ValueExpr::PeriodIndex { anchor, span, at } => {
            collect_binders_in_value(anchor, out);
            collect_binders_in_value(span, out);
            collect_binders_in_value(at, out);
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
        ValueExpr::Round { value, quantum } => {
            collect_binders_in_value(value, out);
            collect_binders_in_value(quantum, out);
        }
        ValueExpr::Term(_) => {}
    }
}

pub(super) fn vars_in_term(term: &Term, out: &mut BTreeSet<String>) {
    match term {
        Term::Var(v) => {
            out.insert(v.to_string());
        }
        Term::Wildcard | Term::Literal(_) | Term::Actor => {}
    }
}

pub(super) fn vars_in_prop(prop: &Prop, out: &mut BTreeSet<String>) {
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

pub(super) fn vars_in_value(expr: &ValueExpr, out: &mut BTreeSet<String>) {
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
        }
        | ValueExpr::Extremum {
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
        ValueExpr::Cond {
            when,
            then,
            otherwise,
        } => {
            vars_in_prop(when, out);
            vars_in_value(then, out);
            vars_in_value(otherwise, out);
        }
        ValueExpr::PeriodIndex { anchor, span, at } => {
            vars_in_value(anchor, out);
            vars_in_value(span, out);
            vars_in_value(at, out);
        }
        ValueExpr::Abs(operand) => vars_in_value(operand, out),
        ValueExpr::Round { value, quantum } => {
            vars_in_value(value, out);
            vars_in_value(quantum, out);
        }
    }
}

/// How many times `name` appears anywhere in the tree - term slots
/// included. The descent belongs to `morpholog_core::fold`; only the
/// question is local.
struct CountOccurrences<'a> {
    name: &'a Var,
    seen: usize,
}

impl morpholog_core::fold::Visit for CountOccurrences<'_> {
    fn term(&mut self, term: &Term, _slot: morpholog_core::fold::TermSlot<'_>) {
        if matches!(term, Term::Var(v) if v == self.name) {
            self.seen += 1;
        }
    }
}

fn count_prop(prop: &Prop, name: &Var) -> usize {
    let mut c = CountOccurrences { name, seen: 0 };
    morpholog_core::fold::visit_prop(prop, &mut c);
    c.seen
}

fn count_value(expr: &ValueExpr, name: &Var) -> usize {
    let mut c = CountOccurrences { name, seen: 0 };
    morpholog_core::fold::visit_value(expr, &mut c);
    c.seen
}

/// Occurrences that can ENLARGE the tree when substituted: value
/// position only. A term slot (claim or call argument, membership
/// operand, sum target, lookup key) takes a 1-for-1 term swap when the
/// value is a plain term, and a refusal otherwise, so it cannot grow.
struct CountGrowthSites<'a> {
    name: &'a Var,
    seen: usize,
}

impl morpholog_core::fold::Visit for CountGrowthSites<'_> {
    fn value(&mut self, value: &ValueExpr) {
        if matches!(value, ValueExpr::Term(Term::Var(v)) if v == self.name) {
            self.seen += 1;
        }
    }
}

fn count_growth_prop(prop: &Prop, name: &Var) -> usize {
    let mut c = CountGrowthSites { name, seen: 0 };
    morpholog_core::fold::visit_prop(prop, &mut c);
    c.seen
}

fn count_growth_value(expr: &ValueExpr, name: &Var) -> usize {
    let mut c = CountGrowthSites { name, seen: 0 };
    morpholog_core::fold::visit_value(expr, &mut c);
    c.seen
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

pub(super) fn value_nodes(expr: &ValueExpr) -> usize {
    1 + match expr {
        ValueExpr::Term(_) => 0,
        ValueExpr::Arith { left, right, .. } => value_nodes(left) + value_nodes(right),
        ValueExpr::Sum { body, .. } | ValueExpr::Extremum { body, .. } => 1 + prop_nodes(body),
        ValueExpr::ValueOf { args, default, .. } => {
            args.len() + default.as_ref().map_or(0, |d| value_nodes(d))
        }
        ValueExpr::Abs(operand) => value_nodes(operand),
        ValueExpr::Round { value, quantum } => value_nodes(value) + value_nodes(quantum),
        ValueExpr::Cond {
            when,
            then,
            otherwise,
        } => prop_nodes(when) + value_nodes(then) + value_nodes(otherwise),
        ValueExpr::PeriodIndex { anchor, span, at } => {
            value_nodes(anchor) + value_nodes(span) + value_nodes(at)
        }
    }
}
