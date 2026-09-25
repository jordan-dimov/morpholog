//! The static check. One walk over every invariant, transformation, and
//! derived-claim body finds, before any `propose`, the errors the
//! runtime would raise:
//!
//! - **kinds** - a value in a slot, comparator, or arithmetic operand
//!   must match the expected kind (`EvalError::TypeMismatch`);
//! - **binding flow** - a name must be bound before it is used, under
//!   the runtime's export rules (`EvalError::UnboundVariable`);
//! - **actor context** - `Term::Actor` in an invariant or derived body,
//!   where there is no proposer (`UnboundActor`).
//!
//! The walk splits by sort ([`CheckCtx::walk_prop`] and
//! [`CheckCtx::infer_value`]); the IR cannot put a value where a
//! proposition belongs.
//!
//! A [`Scope`] carries kinds and bound names together. It is cloned
//! where bindings do not export (`require`, `sum`, `for`, `or`
//! branches), so those rules follow from the structure.
//!
//! `Any` is unconstrained, not a kind-eraser: a variable first seen
//! through an `Any` slot refines on its next concrete use.
//!
//! The IR has no source spans; findings carry a context instead.

use std::collections::{HashMap, HashSet};

use crate::fold;
use crate::format::{arith_token, compare_token};
use crate::ir::{
    ArithOp, Builtin, CompareOp, OrderedDomain, PredicateArgKind, PredicateDecl, Program, Prop,
    RuleName, Stmt, SumSeed, Term, Value, ValueExpr, Var, arith_result_kind,
    arith_unique_counterpart,
};
use crate::validate::{ValidationContext, ValidationError, VocabularyKind};

/// Inferred kind of a value during static analysis. Unlike the
/// declared [`PredicateArgKind`], a variable can be seen but not yet
/// pinned (`UnknownOrAny`); it refines on its first specific use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum InferredKind {
    /// Either an `Any`-declared slot or a variable not yet observed
    /// in any specific slot. Compatible with every other kind and
    /// refinable to a specific kind on first specific observation.
    UnknownOrAny,
    /// A specific kind learned from a literal, a specific-kind
    /// declaration, or a prior refinement.
    Known(PredicateArgKind),
}

impl InferredKind {
    /// Combine an existing inferred kind with a new observation.
    /// Returns `Ok(refined)` when compatible; `Err((prev, new))`
    /// when the two specific kinds genuinely conflict. The refined
    /// kind is whichever side is more specific (a `Known(X)` always
    /// wins over an `UnknownOrAny`).
    pub(crate) fn refine(
        self,
        observed: InferredKind,
    ) -> Result<InferredKind, (PredicateArgKind, PredicateArgKind)> {
        match (self, observed) {
            (InferredKind::UnknownOrAny, observed) => Ok(observed),
            (existing, InferredKind::UnknownOrAny) => Ok(existing),
            (InferredKind::Known(prev), InferredKind::Known(new)) => {
                if kinds_compatible(&prev, &new) {
                    // Prefer the more specific of the two: `Any` on
                    // either side loses to a concrete kind.
                    if matches!(prev, PredicateArgKind::Any) {
                        Ok(InferredKind::Known(new))
                    } else {
                        Ok(InferredKind::Known(prev))
                    }
                } else {
                    Err((prev, new))
                }
            }
        }
    }
}

/// Two declared kinds are compatible if either is `Any` or they are
/// equal.
fn kinds_compatible(a: &PredicateArgKind, b: &PredicateArgKind) -> bool {
    *a == PredicateArgKind::Any || *b == PredicateArgKind::Any || a == b
}

/// The comparator that DOES order `actual`, spelled for the same
/// comparison sense - `on_or_before` for a Date under `<=`, `before`
/// for a Date under `<`. `None` when nothing orders the kind.
fn comparator_suggestion(op: CompareOp, actual: &PredicateArgKind) -> Option<&'static str> {
    OrderedDomain::for_concrete_kind(actual).map(|domain| compare_token(op, domain))
}

/// Variable name -> inferred kind, for one invariant, derived claim,
/// or transformation body.
#[derive(Debug, Default, Clone)]
pub(crate) struct KindEnv {
    bindings: HashMap<Var, InferredKind>,
}

impl KindEnv {
    /// A variable's current inferred kind; `UnknownOrAny` if unseen.
    pub(crate) fn lookup(&self, name: &Var) -> InferredKind {
        self.bindings
            .get(name)
            .cloned()
            .unwrap_or(InferredKind::UnknownOrAny)
    }

    /// Observe a variable at a kind: refine if compatible, else return
    /// the conflict as `(previous, new)`.
    pub(crate) fn observe(
        &mut self,
        name: &Var,
        observed: InferredKind,
    ) -> Result<(), (PredicateArgKind, PredicateArgKind)> {
        let existing = self.lookup(name);
        let refined = existing.refine(observed)?;
        self.bindings.insert(name.clone(), refined);
        Ok(())
    }
}

/// The variables bound at a point in the walk. A variable can have a
/// known kind without being bound, e.g. one matched inside a `require`,
/// whose bindings do not export.
#[derive(Debug, Default, Clone)]
pub(crate) struct BoundEnv {
    bound: HashSet<Var>,
}

impl BoundEnv {
    fn bind(&mut self, name: &Var) {
        self.bound.insert(name.clone());
    }

    fn is_bound(&self, name: &Var) -> bool {
        self.bound.contains(name)
    }

    /// The bound names, for carrying kind evidence back out of a
    /// conditional's condition.
    fn names(&self) -> impl Iterator<Item = &Var> {
        self.bound.iter()
    }

    /// Keep only variables also bound in `other`. After an `or`, a
    /// variable is bound only if every branch bound it, since the
    /// runtime carries forward whichever branch matched.
    fn intersect_with(&mut self, other: &BoundEnv) {
        self.bound.retain(|v| other.bound.contains(v));
    }
}

/// Kinds and bound names, cloned together at scope boundaries, so one
/// walk does both kind inference and unbound-variable detection.
#[derive(Debug, Default, Clone)]
struct Scope {
    kinds: KindEnv,
    bound: BoundEnv,
}

impl Scope {
    fn new() -> Self {
        Self::default()
    }
}

/// Whether a reference binds its variables or consumes them. A claim
/// matched against state (`require`, `bind`, an invariant body, a
/// `forall` source, an `exists` body) binds them (`Match`). `admit`,
/// `retract`, `emit`, and a `value` lookup's keys need them bound
/// (`Use`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefMode {
    Match,
    Use,
}

/// The static-check visitor: the declared vocabularies, the current
/// `ValidationContext`, and the errors so far. The [`Scope`] is passed
/// separately because it is cloned at scope boundaries.
struct CheckCtx<'a> {
    predicates: HashMap<&'a str, &'a PredicateDecl>,
    intents: HashMap<&'a str, &'a crate::IntentDecl>,
    definitions: HashMap<&'a str, &'a crate::Definition>,
    /// Inferred call signature per definition, computed callees-first
    /// before any caller body is walked.
    definition_sigs: HashMap<String, DefinitionSig>,
    /// Predicates a `derived` declaration computes. Rules read admitted
    /// claims only, so a rule naming one is an error.
    derived_heads: std::collections::BTreeSet<&'a str>,
    /// Invariant names the discipline lowering produced, so a finding
    /// inside one can be attributed to the declaration instead.
    generated_invariants: std::collections::BTreeSet<String>,
    context: ValidationContext,
    errors: Vec<ValidationError>,
}

/// Inferred call signature of a definition: each parameter's kind, and
/// whether the body binds it. A parameter the body binds may arrive
/// unbound. One the body only uses (a date in a comparator, say) must
/// arrive bound at every call, as the runtime requires.
struct DefinitionSig {
    param_kinds: Vec<InferredKind>,
    generator: Vec<bool>,
}

/// Run the static checks over the whole programme. Returns every
/// problem found; empty means it passes. Definitions are walked first,
/// then invariants, transformations, and derived claims, so the order
/// of diagnostics is predictable.
pub(crate) fn check_program(program: &Program) -> Vec<ValidationError> {
    let mut cx = CheckCtx {
        derived_heads: program
            .derived_claims
            .iter()
            .map(|d| d.predicate.as_str())
            .collect(),
        generated_invariants: program
            .invariants
            .iter()
            .filter(|i| i.origin != crate::ir::InvariantOrigin::Authored)
            .map(|i| i.name.to_string())
            .collect(),
        predicates: program
            .predicates
            .iter()
            .map(|d| (d.name.as_str(), d))
            .collect(),
        intents: program
            .intents
            .iter()
            .map(|d| (d.name.as_str(), d))
            .collect(),
        definitions: program
            .definitions
            .iter()
            .map(|d| (d.name.as_str(), d))
            .collect(),
        definition_sigs: HashMap::new(),
        // Reassigned per top-level item below; this placeholder is
        // never the context of an emitted error.
        context: ValidationContext::Invariant {
            name: String::new(),
        },
        errors: Vec::new(),
    };

    // Definitions first, callees before callers, so every call site
    // checks against an inferred signature. `validate_program` already
    // rejected cycles; `unwrap_or_default` covers direct calls on cyclic
    // IR.
    let definition_order =
        crate::definitions::definition_topo_order(&program.definitions).unwrap_or_default();
    for i in definition_order {
        let def = &program.definitions[i];
        cx.context = ValidationContext::Definition {
            name: def.name.to_string(),
        };
        // Bodies are context-free: no `actor` (pass it as a call
        // argument) and no `pre(...)` (wrap the call instead), so a
        // definition means the same thing in a gate as in an invariant.
        if prop_mentions_actor(&def.body) {
            let context = cx.context.clone();
            cx.errors
                .push(ValidationError::ActorNotAvailable { context });
        }
        if fold::mentions_pre(&def.body) {
            let context = cx.context.clone();
            cx.errors.push(ValidationError::PreNotAvailable { context });
        }
        // Probe walk: which parameters does the body bind? Parameters
        // start unbound and the probe's errors are discarded.
        let mut probe = Scope::new();
        let kept = std::mem::take(&mut cx.errors);
        cx.walk_prop(&def.body, &mut probe);
        cx.errors = kept;
        let generator: Vec<bool> = def
            .parameters
            .iter()
            .map(|p| probe.bound.is_bound(p))
            .collect();
        // Real walk: parameters arrive bound and untyped, like
        // transformation parameters, so kinds refine on use.
        let mut scope = Scope::new();
        for param in &def.parameters {
            scope.bound.bind(param);
            let _ = scope.kinds.observe(param, InferredKind::UnknownOrAny);
        }
        cx.walk_prop(&def.body, &mut scope);
        for param in &def.parameters {
            if !occurs_in_prop(param, &def.body) {
                cx.errors.push(ValidationError::ParameterNotReferenced {
                    definition: def.name.to_string(),
                    parameter: param.to_string(),
                });
            }
        }
        let param_kinds = def
            .parameters
            .iter()
            .map(|p| scope.kinds.lookup(p))
            .collect();
        cx.definition_sigs.insert(
            def.name.to_string(),
            DefinitionSig {
                param_kinds,
                generator,
            },
        );
    }

    for inv in &program.invariants {
        cx.context = ValidationContext::Invariant {
            name: inv.name.to_string(),
        };
        if prop_mentions_actor(&inv.body) {
            let context = cx.context.clone();
            cx.errors
                .push(ValidationError::ActorNotAvailable { context });
        }
        if let Some(target) = &inv.totality_for {
            match program.predicates.iter().find(|d| &d.name == target) {
                None => cx.errors.push(ValidationError::UnknownTotalityTarget {
                    invariant: inv.name.to_string(),
                    predicate: target.to_string(),
                }),
                Some(decl)
                    if decl.disciplines.iter().any(|d| {
                        matches!(d, crate::ir::Discipline::EffectiveBy { partial: true, .. })
                    }) =>
                {
                    cx.errors.push(ValidationError::PartialContradictsTotality {
                        predicate: target.to_string(),
                        invariant: inv.name.to_string(),
                    });
                }
                Some(_) => {}
            }
        }
        let mut scope = Scope::new();
        cx.walk_prop(&inv.body, &mut scope);
    }

    for transformation in &program.transformations {
        let mut scope = Scope::new();
        // Parameters arrive bound and untyped, so their kind refines on
        // first use.
        for param in &transformation.parameters {
            scope.bound.bind(param);
            let _ = scope.kinds.observe(param, InferredKind::UnknownOrAny);
        }
        // A duplicate name within one transformation would make a
        // refusal ambiguous. Two transformations may share a gate name.
        let mut seen: HashSet<&RuleName> = HashSet::new();
        for (index, stmt) in transformation.body.iter().enumerate() {
            let mut names = Vec::new();
            collect_rule_names(stmt, &mut names);
            for name in names {
                if !seen.insert(name) {
                    cx.errors.push(ValidationError::DuplicateRuleName {
                        context: ValidationContext::Transformation {
                            name: transformation.name.to_string(),
                            statement: Some(index),
                        },
                        name: name.to_string(),
                    });
                }
            }
        }

        // The context carries the statement index so a finding points
        // at its statement. Inside a nested `for`, the top-level index.
        for (index, stmt) in transformation.body.iter().enumerate() {
            cx.context = ValidationContext::Transformation {
                name: transformation.name.to_string(),
                statement: Some(index),
            };
            cx.walk_stmt(stmt, &mut scope);
        }

        // A parameter inferred as CalendarSpan can never be supplied,
        // since no transition argument may carry a span. Checked after
        // the walk so later refinements count.
        for param in &transformation.parameters {
            if scope.kinds.lookup(param) == InferredKind::Known(PredicateArgKind::CalendarSpan) {
                cx.errors
                    .push(ValidationError::CalendarSpanEscapesExpression {
                        place: format!(
                            "parameter `{param}` (no transition argument may carry a span; \
                             write the span as a literal in the body)"
                        ),
                        context: ValidationContext::Transformation {
                            name: transformation.name.to_string(),
                            statement: None,
                        },
                    });
            }
        }
    }

    for derived in &program.derived_claims {
        cx.context = ValidationContext::DerivedClaim {
            predicate: derived.predicate.to_string(),
        };
        // Disciplines are promises about governed state. A derived
        // output is replaced wholesale on refresh, so it can keep none.
        // Refused at the declaration, where the author wrote the clause.
        if let Some(decl) = cx.predicates.get(derived.predicate.as_str()).copied()
            && !decl.disciplines.is_empty()
        {
            cx.errors.push(ValidationError::DisciplineOnDerived {
                predicate: derived.predicate.to_string(),
            });
        }
        if prop_mentions_actor(&derived.domain)
            || derived.values.iter().any(|v| value_mentions_actor(&v.expr))
        {
            let context = cx.context.clone();
            cx.errors
                .push(ValidationError::ActorNotAvailable { context });
        }
        // Values run once per distinct key tuple, with only the keys
        // bound. So values are inferred in a scope holding just the keys,
        // and a use of another domain variable gets the not-a-key error.
        let mut scope = Scope::new();
        cx.walk_prop(&derived.domain, &mut scope);
        let mut value_scope = Scope::new();
        for key in &derived.keys {
            if scope.bound.is_bound(key) {
                value_scope.bound.bind(key);
            }
            let kind = scope.kinds.lookup(key);
            let _ = value_scope.kinds.observe(key, kind);
        }
        let before_values = cx.errors.len();
        let value_kinds: Vec<InferredKind> = derived
            .values
            .iter()
            .map(|v| cx.infer_value(&v.expr, &mut value_scope))
            .collect();
        for error in &mut cx.errors[before_values..] {
            if let ValidationError::UnboundVariable { variable, context } = error
                && scope.bound.is_bound(&Var::from(variable.as_str()))
            {
                *error = ValidationError::DerivedValueNotAKey {
                    variable: std::mem::take(variable),
                    context: context.clone(),
                };
            }
        }
        // A derived output may never be a span.
        for (i, kind) in value_kinds.iter().enumerate() {
            if *kind == InferredKind::Known(PredicateArgKind::CalendarSpan) {
                let context = cx.context.clone();
                cx.errors
                    .push(ValidationError::CalendarSpanEscapesExpression {
                        place: format!("derived value #{i} of `{}`", derived.predicate),
                        context,
                    });
            }
        }

        // Output is `predicate(keys.., values..)`: declared, with arity
        // keys + values, and each position of its declared kind.
        let Some(decl) = cx.predicates.get(derived.predicate.as_str()).copied() else {
            let context = cx.context.clone();
            cx.errors.push(ValidationError::Undeclared {
                vocabulary: VocabularyKind::Predicate,
                name: derived.predicate.to_string(),
                context,
            });
            continue;
        };
        let output_arity = derived.keys.len() + derived.values.len();
        if decl.args.len() != output_arity {
            let context = cx.context.clone();
            cx.errors.push(ValidationError::ArityMismatch {
                vocabulary: VocabularyKind::Predicate,
                name: derived.predicate.to_string(),
                expected: decl.args.len(),
                actual: output_arity,
                context,
            });
        }
        let n = output_arity.min(decl.args.len());
        for position in 0..n {
            let actual = if position < derived.keys.len() {
                // The value scope may have refined a key's kind since the
                // domain walk, so it knows more.
                value_scope.kinds.lookup(&derived.keys[position])
            } else {
                value_kinds[position - derived.keys.len()].clone()
            };
            let expected = decl.args[position].kind.clone();
            if let InferredKind::Known(actual_kind) = actual
                && !kinds_compatible(&expected, &actual_kind)
            {
                let context = cx.context.clone();
                cx.errors.push(ValidationError::ArgKindMismatch {
                    vocabulary: VocabularyKind::Predicate,
                    name: derived.predicate.to_string(),
                    position,
                    expected,
                    actual: actual_kind,
                    context,
                });
            }
        }
    }

    cx.errors
}

impl CheckCtx<'_> {
    /// Walk a proposition. `And`, `Implies`, and `Pre` thread the scope;
    /// `Or` branches each walk a clone. Claims here bind their variables.
    fn walk_prop(&mut self, prop: &Prop, scope: &mut Scope) {
        match prop {
            Prop::Claim { predicate, args } => {
                // A claim naming a definition means call resolution was
                // skipped (hand-built IR). Say so, not just "undeclared".
                if self.definitions.contains_key(predicate.as_str()) {
                    self.report(|context| ValidationError::UnresolvedDefinitionCall {
                        name: predicate.to_string(),
                        context,
                    });
                } else {
                    self.check_predicate_ref(predicate.as_str(), args, RefMode::Match, scope);
                }
            }
            Prop::Defined { name, args } => {
                self.check_defined_call(name.as_str(), args, scope);
            }
            Prop::And(items) => {
                // Conjuncts thread the scope forward: each branch
                // sees bindings and refinements from earlier ones.
                for item in items {
                    self.walk_prop(item, scope);
                }
            }
            Prop::Or(items) => {
                // Each branch starts from the same scope, as at runtime.
                // A name bound in every branch is bound after the `or`.
                // Refinements are dropped: that can only miss an error,
                // never invent one.
                let mut merged: Option<BoundEnv> = None;
                for item in items {
                    let mut branch = scope.clone();
                    self.walk_prop(item, &mut branch);
                    merged = Some(match merged {
                        None => branch.bound,
                        Some(mut acc) => {
                            acc.intersect_with(&branch.bound);
                            acc
                        }
                    });
                }
                if let Some(merged) = merged {
                    scope.bound = merged;
                }
            }
            Prop::Xor(left, right) => {
                // As with `or`: a name is bound after the xor only if
                // both operands bind it.
                let mut lb = scope.clone();
                self.walk_prop(left, &mut lb);
                let mut rb = scope.clone();
                self.walk_prop(right, &mut rb);
                let mut merged = lb.bound;
                merged.intersect_with(&rb.bound);
                scope.bound = merged;
            }
            Prop::Not(inner) | Prop::Pre(inner) => {
                self.walk_prop(inner, scope);
            }
            Prop::Implies { left, right } => {
                self.walk_prop(left, scope);
                self.walk_prop(right, scope);
            }
            Prop::Exists { binding, body } => {
                // The quantifier binds it before the body. An outer
                // variable of the same name is not shadowed (the runtime
                // unifies).
                scope.bound.bind(binding);
                self.walk_prop(body, scope);
            }
            Prop::Forall {
                binding,
                source,
                body,
            } => {
                // Bound for both the source (`e in coll`) and the body.
                // Walked in the live scope, so the name may stay visible
                // afterwards; that avoids false positives.
                scope.bound.bind(binding);
                self.walk_prop(source, scope);
                self.walk_prop(body, scope);
            }
            Prop::Compare {
                op,
                domain,
                left,
                right,
            } => {
                match domain {
                    // The decimal domain has two flavours: bare decimals
                    // and quantities (`Decimal[U]`). Both operands must
                    // share one.
                    OrderedDomain::Decimal => {
                        self.check_decimal_domain_operands(left, right, *op, scope);
                    }
                    OrderedDomain::Date | OrderedDomain::Timestamp | OrderedDomain::Duration => {
                        self.check_temporal_domain_operands(left, right, *op, *domain, scope);
                    }
                }
            }
            Prop::Eq(left, right) => {
                self.check_equality_operands(left, right, "=", scope);
            }
            Prop::Neq(left, right) => {
                self.check_equality_operands(left, right, "!=", scope);
            }
            Prop::In(element, collection) => {
                // `In` binds an unbound element to each item, or filters
                // on a bound one; either way it is bound afterwards. The
                // collection must be bound and a Collection.
                if let Term::Var(name) = element {
                    scope.bound.bind(name);
                    let _ = scope.kinds.observe(name, InferredKind::UnknownOrAny);
                }
                match collection {
                    Term::Var(name) => {
                        self.use_var(scope, name);
                        self.observe_or_report(
                            scope,
                            name,
                            InferredKind::Known(PredicateArgKind::Collection),
                        );
                    }
                    Term::Wildcard => {}
                    other => {
                        if let InferredKind::Known(actual) = term_kind(other)
                            && !kinds_compatible(&PredicateArgKind::Collection, &actual)
                        {
                            self.report(|context| ValidationError::OperandKindMismatch {
                                operator: "in",
                                expected: PredicateArgKind::Collection,
                                actual,
                                suggestion: None,
                                context,
                            });
                        }
                    }
                }
            }
        }
    }

    /// Walk a statement, threading the scope as the runtime does:
    ///
    /// - `Require` walks a clone (its matches do not export).
    /// - `BindOne` walks the live scope (its matches bind).
    /// - `Let` / `LetNewSubject` bind their name.
    /// - `Assert` / `Retract` / `Emit` consume args (`Use` mode).
    /// - `For` consumes the collection and binds the loop variable
    ///   in a scoped clone.
    fn walk_stmt(&mut self, stmt: &Stmt, scope: &mut Scope) {
        match stmt {
            Stmt::Require { prop, .. } => {
                let mut scoped = scope.clone();
                self.walk_prop(prop, &mut scoped);
            }
            Stmt::BindOne { prop, .. } => {
                self.walk_prop(prop, scope);
            }
            Stmt::Let { name, value } => {
                let value_kind = self.infer_value(value, scope);
                scope.bound.bind(name);
                self.observe_or_report(scope, name, value_kind);
            }
            Stmt::LetNewSubject { name } => {
                scope.bound.bind(name);
                self.observe_or_report(scope, name, InferredKind::Known(PredicateArgKind::Subject));
            }
            Stmt::Assert(claim) => {
                self.check_predicate_ref(
                    claim.predicate.as_str(),
                    &claim.args,
                    RefMode::Use,
                    scope,
                );
            }
            Stmt::Retract { predicate, args } => {
                self.check_predicate_ref(predicate.as_str(), args, RefMode::Use, scope);
            }
            Stmt::For {
                binding,
                collection,
                body,
            } => {
                self.check_operand_kind(collection, PredicateArgKind::Collection, "for", scope);
                // A clone, so loop bindings do not leak out.
                let mut scoped = scope.clone();
                scoped.bound.bind(binding);
                let _ = scoped.kinds.observe(binding, InferredKind::UnknownOrAny);
                for inner in body {
                    self.walk_stmt(inner, &mut scoped);
                }
            }
            Stmt::Emit(intent) => {
                self.check_intent_ref(intent.name.as_str(), &intent.args, RefMode::Use, scope);
            }
        }
    }

    /// Check that a value-shaped operand evaluates to the expected
    /// kind. A bare variable is a use: it must be bound, and its
    /// kind refines toward `expected`. Anything else infers its
    /// kind and emits `OperandKindMismatch` on disagreement.
    fn check_operand_kind(
        &mut self,
        operand: &ValueExpr,
        expected: PredicateArgKind,
        operator: &'static str,
        scope: &mut Scope,
    ) {
        if let ValueExpr::Term(Term::Var(name)) = operand {
            self.use_var(scope, name);
            self.observe_or_report(scope, name, InferredKind::Known(expected));
            return;
        }
        // A variable inside `abs(...)` refines too, so
        // `abs(d) <cmp> duration(...)` pins `d` to Duration.
        self.refine_through_abs(operand, &expected, scope);
        let inferred = self.infer_value(operand, scope);
        if let InferredKind::Known(actual) = inferred
            && !kinds_compatible(&expected, &actual)
        {
            self.report(|context| ValidationError::OperandKindMismatch {
                operator,
                expected,
                actual,
                suggestion: None,
                context,
            });
        }
    }

    /// Both operands of a `Date`/`Timestamp`/`Duration` comparison. Both
    /// are judged first; if either is refused, nothing is inferred, so
    /// the healthy side gets no spurious later conflict. A refused bare
    /// variable is an operand mismatch naming the comparator that would
    /// order it, not a variable-kind conflict. In a clean pair, unknown
    /// operands refine to the domain's kind (so `on_or_before` pins a
    /// free parameter to Date).
    fn check_temporal_domain_operands(
        &mut self,
        left: &ValueExpr,
        right: &ValueExpr,
        op: CompareOp,
        domain: OrderedDomain,
        scope: &mut Scope,
    ) {
        let expected = match domain {
            // Never called with Decimal; mapped anyway to stay
            // panic-free.
            OrderedDomain::Decimal => PredicateArgKind::Decimal,
            OrderedDomain::Date => PredicateArgKind::Date,
            OrderedDomain::Timestamp => PredicateArgKind::Timestamp,
            OrderedDomain::Duration => PredicateArgKind::Duration,
        };
        let l_refused = self.judge_ordered_operand(left, op, domain, &expected, scope);
        let r_refused = self.judge_ordered_operand(right, op, domain, &expected, scope);
        if l_refused || r_refused {
            return;
        }
        for operand in [left, right] {
            if let ValueExpr::Term(Term::Var(name)) = operand {
                self.observe_or_report(scope, name, InferredKind::Known(expected.clone()));
            } else {
                // `abs(gap) no_longer_than allowed` pins `gap` to Duration.
                self.refine_through_abs(operand, &expected, scope);
            }
        }
    }

    /// One temporal operand: report a known kind the domain does not
    /// order, naming the comparator that would, and return whether it
    /// was refused. Refines nothing.
    fn judge_ordered_operand(
        &mut self,
        operand: &ValueExpr,
        op: CompareOp,
        domain: OrderedDomain,
        expected: &PredicateArgKind,
        scope: &mut Scope,
    ) -> bool {
        let inferred = if let ValueExpr::Term(Term::Var(name)) = operand {
            self.use_var(scope, name);
            scope.kinds.lookup(name)
        } else {
            self.infer_value(operand, scope)
        };
        if let InferredKind::Known(actual) = inferred
            && !domain.admits(&actual)
        {
            let suggestion = comparator_suggestion(op, &actual);
            self.report(|context| ValidationError::OperandKindMismatch {
                operator: compare_token(op, domain),
                expected: expected.clone(),
                actual,
                suggestion,
                context,
            });
            return true;
        }
        false
    }

    /// Refine the variable inside an `abs(...)` operand toward `expected`,
    /// when abs preserves that kind (decimal, quantity, duration), so
    /// `abs(x) <= 10` pins `x` to Decimal. Anything else is left alone.
    fn refine_through_abs(
        &mut self,
        operand: &ValueExpr,
        expected: &PredicateArgKind,
        scope: &mut Scope,
    ) {
        if !matches!(
            operand,
            ValueExpr::Call {
                builtin: Builtin::Abs,
                ..
            }
        ) || !matches!(
            expected,
            PredicateArgKind::Decimal | PredicateArgKind::Quantity(_) | PredicateArgKind::Duration
        ) {
            return;
        }
        // Peel nested `abs(abs(x))` down to the variable.
        let mut cur = operand;
        while let ValueExpr::Call {
            builtin: Builtin::Abs,
            args,
        } = cur
            && let [inner] = args.as_slice()
        {
            cur = inner;
        }
        if let ValueExpr::Term(Term::Var(name)) = cur {
            self.observe_or_report(scope, name, InferredKind::Known(expected.clone()));
        }
    }

    /// One operand of a decimal-domain comparison, plus whether it was
    /// refused. An unknown variable's flavour is decided later from the
    /// other operand. A known kind that is neither a decimal nor a
    /// quantity is reported once and treated as unknown from then on.
    fn infer_decimal_domain_operand(
        &mut self,
        operand: &ValueExpr,
        op: CompareOp,
        scope: &mut Scope,
    ) -> (InferredKind, bool) {
        let inferred = if let ValueExpr::Term(Term::Var(name)) = operand {
            self.use_var(scope, name);
            scope.kinds.lookup(name)
        } else {
            self.infer_value(operand, scope)
        };
        if let InferredKind::Known(actual) = &inferred
            && !OrderedDomain::Decimal.admits(actual)
        {
            let suggestion = comparator_suggestion(op, actual);
            self.report(|context| ValidationError::OperandKindMismatch {
                operator: compare_token(op, OrderedDomain::Decimal),
                expected: PredicateArgKind::Decimal,
                actual: actual.clone(),
                suggestion,
                context,
            });
            return (InferredKind::UnknownOrAny, true);
        }
        (inferred, false)
    }

    /// Both operands of a decimal-domain comparison: two bare decimals,
    /// or two quantities of the same unit. A known side refines an
    /// unknown variable to match (`settled <= due` gives `settled` the
    /// unit of `due`); two unknowns default to bare decimal.
    fn check_decimal_domain_operands(
        &mut self,
        left: &ValueExpr,
        right: &ValueExpr,
        op: CompareOp,
        scope: &mut Scope,
    ) {
        let operator = compare_token(op, OrderedDomain::Decimal);
        let (l, l_refused) = self.infer_decimal_domain_operand(left, op, scope);
        let (r, r_refused) = self.infer_decimal_domain_operand(right, op, scope);
        // A refused comparison infers nothing, or the healthy side would
        // pick up spurious conflicts later.
        if l_refused || r_refused {
            return;
        }
        let refine =
            |this: &mut Self, operand: &ValueExpr, kind: PredicateArgKind, scope: &mut Scope| {
                if let ValueExpr::Term(Term::Var(name)) = operand {
                    this.observe_or_report(scope, name, InferredKind::Known(kind));
                } else {
                    // `abs(x) <= 10` pins `x` to Decimal.
                    this.refine_through_abs(operand, &kind, scope);
                }
            };
        match (l, r) {
            (InferredKind::Known(a), InferredKind::Known(b)) => {
                if kinds_compatible(&a, &b) {
                    // Variables still refine toward the more specific
                    // side (`Any` loses to a literal's kind).
                    let specific = more_specific(a, b);
                    refine(self, left, specific.clone(), scope);
                    refine(self, right, specific, scope);
                } else {
                    self.report(|context| ValidationError::OperandKindMismatch {
                        operator,
                        expected: a,
                        actual: b,
                        suggestion: None,
                        context,
                    });
                }
            }
            (InferredKind::Known(k), InferredKind::UnknownOrAny) => {
                refine(self, right, k, scope);
            }
            (InferredKind::UnknownOrAny, InferredKind::Known(k)) => {
                refine(self, left, k, scope);
            }
            (InferredKind::UnknownOrAny, InferredKind::UnknownOrAny) => {
                refine(self, left, PredicateArgKind::Decimal, scope);
                refine(self, right, PredicateArgKind::Decimal, scope);
            }
        }
    }

    /// Strict equality, for `Eq` and `Neq`. Two known kinds must be
    /// compatible; a bare variable refines to the other side's kind.
    /// `Subject == Decimal` is an error, never a coercion.
    fn check_equality(
        &mut self,
        left: EqualityOperand<'_>,
        right: EqualityOperand<'_>,
        operator: &'static str,
        scope: &mut Scope,
    ) {
        self.unify_value_kinds(left, right, scope, |l, r, context| {
            ValidationError::EqualityKindMismatch {
                operator,
                left: l,
                right: r,
                context,
            }
        });
    }

    /// Unify two value kinds, for equality and for a conditional's
    /// branches: check compatibility, keep the more specific kind, and
    /// refine bare variables. Each caller supplies its own `mismatch`
    /// error. Returns the combined kind.
    fn unify_value_kinds(
        &mut self,
        left: EqualityOperand<'_>,
        right: EqualityOperand<'_>,
        scope: &mut Scope,
        mismatch: impl FnOnce(PredicateArgKind, PredicateArgKind, ValidationContext) -> ValidationError,
    ) -> InferredKind {
        let combined = match (left.0, right.0) {
            (InferredKind::Known(l), InferredKind::Known(r)) => {
                if !kinds_compatible(&l, &r) {
                    self.report(|context| mismatch(l, r, context));
                    None
                } else {
                    Some(InferredKind::Known(more_specific(l, r)))
                }
            }
            (k @ InferredKind::Known(_), InferredKind::UnknownOrAny)
            | (InferredKind::UnknownOrAny, k @ InferredKind::Known(_)) => Some(k),
            (InferredKind::UnknownOrAny, InferredKind::UnknownOrAny) => None,
        };
        if let Some(refined) = &combined {
            for name in [left.1, right.1].into_iter().flatten() {
                self.observe_or_report(scope, name, refined.clone());
            }
        }
        combined.unwrap_or(InferredKind::UnknownOrAny)
    }

    fn check_equality_operands(
        &mut self,
        left: &ValueExpr,
        right: &ValueExpr,
        operator: &'static str,
        scope: &mut Scope,
    ) {
        // `infer_value` use-checks a bare-variable operand.
        let left_op = (self.infer_value(left, scope), value_var_name(left));
        let right_op = (self.infer_value(right, scope), value_var_name(right));
        self.check_equality(left_op, right_op, operator, scope);
    }

    /// Infer the kind of a value expression. A bare variable is a use
    /// (must be bound); literals carry their kind; `Arith` follows the
    /// arithmetic rule matrix; `Sum` walks its body in a cloned scope and
    /// yields a decimal, duration, or quantity; `ValueOf` yields the
    /// declared kind of the extracted slot.
    fn infer_value(&mut self, expr: &ValueExpr, scope: &mut Scope) -> InferredKind {
        match expr {
            ValueExpr::Term(term) => {
                if let Term::Var(name) = term {
                    self.use_var(scope, name);
                }
                // A wildcard has no value, and patterns hold theirs as
                // bare `Term`s, so this is a value position. Reported once
                // per context, since a node can be inferred twice.
                if matches!(term, Term::Wildcard) {
                    let error = ValidationError::WildcardAsValue {
                        context: self.context.clone(),
                    };
                    if !self.errors.contains(&error) {
                        self.errors.push(error);
                    }
                    return InferredKind::UnknownOrAny;
                }
                resolved_term_kind(term, &scope.kinds)
            }
            // The condition walks a clone (its bindings do not export,
            // like `require`). The branches infer in the outer scope and
            // must unify; any kind is allowed, since selecting is not
            // ordering.
            ValueExpr::Cond {
                when,
                then,
                otherwise,
            } => {
                let mut scoped = scope.clone();
                self.walk_prop(when, &mut scoped);
                // Bindings do not export, but kind evidence about an
                // already-bound variable does (`Member(who)` pins
                // `who: Subject`). Dropping it would let the branches
                // refine the variable to a contradictory kind.
                let entry_bound: Vec<Var> = scope.bound.names().cloned().collect();
                for name in entry_bound {
                    let refined = scoped.kinds.lookup(&name);
                    if matches!(refined, InferredKind::Known(_)) {
                        self.observe_or_report(scope, &name, refined);
                    }
                }
                let then_op = (self.infer_value(then, scope), value_var_name(then));
                let otherwise_op = (
                    self.infer_value(otherwise, scope),
                    value_var_name(otherwise),
                );
                self.unify_value_kinds(then_op, otherwise_op, scope, |t, o, context| {
                    ValidationError::CondBranchKindMismatch {
                        then_kind: t,
                        otherwise_kind: o,
                        context,
                    }
                })
            }
            ValueExpr::Arith { op, left, right } => {
                let operator = arith_token(*op);
                // Every operator goes through the rule matrix. With both
                // sides known, the matrix decides, and a missing rule is
                // an authoring-time error.
                let l = self.infer_value(left, scope);
                let r = self.infer_value(right, scope);
                match (l, r) {
                    (InferredKind::Known(a), InferredKind::Known(b)) => {
                        match arith_result_kind(*op, &a, &b) {
                            Some(kind) => InferredKind::Known(kind),
                            None => {
                                self.report(|context| ValidationError::NoArithRule {
                                    operator,
                                    left: a,
                                    right: b,
                                    context,
                                });
                                InferredKind::UnknownOrAny
                            }
                        }
                    }
                    // One side known: if exactly one rule fits, a bare
                    // variable on the other side is refined (`tendered_at
                    // + turn_time` makes `turn_time` a Duration). If
                    // several fit (`Timestamp - x`), nothing is assumed.
                    (InferredKind::Known(k), InferredKind::UnknownOrAny) => {
                        match arith_unique_counterpart(*op, &k, true) {
                            Some((expected, result)) => {
                                self.check_operand_kind(right, expected, operator, scope);
                                InferredKind::Known(result)
                            }
                            None => InferredKind::UnknownOrAny,
                        }
                    }
                    (InferredKind::UnknownOrAny, InferredKind::Known(k)) => {
                        match arith_unique_counterpart(*op, &k, false) {
                            Some((expected, result)) => {
                                self.check_operand_kind(left, expected, operator, scope);
                                InferredKind::Known(result)
                            }
                            None => InferredKind::UnknownOrAny,
                        }
                    }
                    // Both unknown: Mul / Div / Mod default to bare
                    // decimal, since a unit cannot come from nothing.
                    // Add / Sub stay unrefined.
                    _ if matches!(op, ArithOp::Mul | ArithOp::Div | ArithOp::Mod) => {
                        self.check_operand_kind(left, PredicateArgKind::Decimal, operator, scope);
                        self.check_operand_kind(right, PredicateArgKind::Decimal, operator, scope);
                        InferredKind::Known(PredicateArgKind::Decimal)
                    }
                    _ => InferredKind::UnknownOrAny,
                }
            }
            ValueExpr::Extremum { op, value, body } => {
                // Body first, in a clone, as `Sum` does.
                let mut scoped = scope.clone();
                self.walk_prop(body, &mut scoped);
                if let Term::Var(name) = value {
                    self.use_var(&scoped, name);
                }
                let resolved = resolved_term_kind(value, &scoped.kinds);
                // An extremum yields a member, so it has the member's
                // kind, which must be ordered.
                if let InferredKind::Known(actual) = resolved {
                    // An allow-list, so a new kind is unordered until
                    // someone decides otherwise.
                    if !matches!(
                        actual,
                        PredicateArgKind::Decimal
                            | PredicateArgKind::Date
                            | PredicateArgKind::Timestamp
                            | PredicateArgKind::Duration
                            | PredicateArgKind::Quantity(_)
                    ) {
                        self.report(|context| ValidationError::UnorderedExtremum {
                            op: op.as_str(),
                            actual: actual.clone(),
                            context,
                        });
                        return InferredKind::UnknownOrAny;
                    }
                    return InferredKind::Known(actual);
                }
                InferredKind::UnknownOrAny
            }
            ValueExpr::Sum { value, body, seed } => {
                // Body first, in a clone, so its bindings do not leak.
                // The target is inferred in that scope, with the body's
                // bindings in force.
                let mut scoped = scope.clone();
                self.walk_prop(body, &mut scoped);
                let resolved = self.infer_value(value, &mut scoped);
                // Sums of decimals, durations, and quantities are fine;
                // any other known kind is an error.
                if let InferredKind::Known(
                    k @ (PredicateArgKind::Duration | PredicateArgKind::Quantity(_)),
                ) = resolved
                {
                    // The seed pass sees less than this scope (an
                    // outer-bound variable, a builtin call), and an empty
                    // sum returns the seed. A mismatch would be a runtime
                    // type error on the first empty book, so refuse it.
                    let seed_kind = match seed {
                        SumSeed::Decimal => PredicateArgKind::Decimal,
                        SumSeed::Duration => PredicateArgKind::Duration,
                        SumSeed::Quantity(u) => PredicateArgKind::Quantity(u.clone()),
                    };
                    if seed_kind != k {
                        self.report(|context| ValidationError::EmptySumUntyped {
                            target: k.clone(),
                            seed: seed_kind,
                            context,
                        });
                    }
                    return InferredKind::Known(k);
                }
                if let InferredKind::Known(actual) = resolved
                    && !kinds_compatible(&PredicateArgKind::Decimal, &actual)
                {
                    self.report(|context| ValidationError::OperandKindMismatch {
                        operator: "sum",
                        expected: PredicateArgKind::Decimal,
                        actual,
                        suggestion: None,
                        context,
                    });
                }
                InferredKind::Known(PredicateArgKind::Decimal)
            }
            ValueExpr::ValueOf {
                predicate,
                args,
                extract,
                default,
            } => {
                // A lookup consumes its key arguments (the wildcard
                // marks the extracted value, not a binding).
                self.check_predicate_ref(predicate.as_str(), args, RefMode::Use, scope);
                if !matches!(args.get(*extract), Some(Term::Wildcard)) {
                    self.report(|context| ValidationError::InvalidValueExtraction {
                        predicate: predicate.to_string(),
                        extract: *extract,
                        context,
                    });
                }
                let result_kind =
                    value_of_result_kind(predicate.as_str(), *extract, &self.predicates);
                if let Some(default_expr) = default {
                    let default_kind = self.infer_value(default_expr, scope);
                    // Either the value or the default is returned, so
                    // their kinds must agree.
                    if let (InferredKind::Known(expected), InferredKind::Known(actual)) =
                        (result_kind.clone(), default_kind)
                        && !kinds_compatible(&expected, &actual)
                    {
                        self.report(|context| ValidationError::OperandKindMismatch {
                            operator: "value default",
                            expected,
                            actual,
                            suggestion: None,
                            context,
                        });
                    }
                }
                result_kind
            }
            // Arity first, so a wrong count is one clear error rather
            // than a cascade about kinds.
            ValueExpr::Call { builtin, args } => {
                if args.len() != builtin.arity() {
                    self.report(|context| ValidationError::BuiltinArity {
                        builtin: builtin.name(),
                        expected: builtin.arity(),
                        found: args.len(),
                        context,
                    });
                    for a in args {
                        let _ = self.infer_value(a, scope);
                    }
                    return InferredKind::UnknownOrAny;
                }
                self.infer_builtin(*builtin, args, scope)
            }
        }
    }

    /// The static rules of each builtin: what its arguments must be,
    /// what it yields, and which literal arguments are refused here
    /// rather than at runtime. Exhaustive over [`Builtin`], because the
    /// rules genuinely differ (`abs` keeps its operand's kind, `round`
    /// fixes one).
    fn infer_builtin(
        &mut self,
        builtin: Builtin,
        args: &[ValueExpr],
        scope: &mut Scope,
    ) -> InferredKind {
        match builtin {
            Builtin::Abs => match self.infer_value(&args[0], scope) {
                // abs keeps a signed value's kind; other kinds are errors.
                InferredKind::Known(
                    k @ (PredicateArgKind::Decimal
                    | PredicateArgKind::Quantity(_)
                    | PredicateArgKind::Duration),
                ) => InferredKind::Known(k),
                InferredKind::Known(kind) => {
                    self.report(|context| ValidationError::AbsKind { kind, context });
                    InferredKind::UnknownOrAny
                }
                InferredKind::UnknownOrAny => InferredKind::UnknownOrAny,
            },
            Builtin::Round => {
                // Refines a bare variable to Decimal, accepts Any, and
                // reports other kinds as OperandKindMismatch.
                self.check_operand_kind(&args[0], PredicateArgKind::Decimal, "round", scope);
                self.check_operand_kind(&args[1], PredicateArgKind::Decimal, "round", scope);
                // A literal quantum must be positive; the runtime checks
                // a variable one.
                if let ValueExpr::Term(Term::Literal(Value::Decimal(s))) = &args[1]
                    && s.parse::<rust_decimal::Decimal>()
                        .is_ok_and(|d| d <= rust_decimal::Decimal::ZERO)
                {
                    self.report(|context| ValidationError::RoundQuantumNotPositive {
                        quantum: s.clone(),
                        context,
                    });
                }
                InferredKind::Known(PredicateArgKind::Decimal)
            }
            Builtin::PeriodIndex => {
                self.check_operand_kind(&args[0], PredicateArgKind::Date, "period_index", scope);
                self.check_operand_kind(
                    &args[1],
                    PredicateArgKind::CalendarSpan,
                    "period_index",
                    scope,
                );
                self.check_operand_kind(&args[2], PredicateArgKind::Date, "period_index", scope);
                // A literal zero span is refused here; the runtime checks
                // one passed in through a parameter.
                self.refuse_literal_zero_span(&args[1], "period_index");
                InferredKind::Known(PredicateArgKind::Decimal)
            }
            Builtin::PeriodStartOf => {
                self.check_operand_kind(&args[0], PredicateArgKind::Date, "period_start_of", scope);
                self.check_operand_kind(
                    &args[1],
                    PredicateArgKind::CalendarSpan,
                    "period_start_of",
                    scope,
                );
                self.check_operand_kind(
                    &args[2],
                    PredicateArgKind::Decimal,
                    "period_start_of",
                    scope,
                );
                self.refuse_literal_zero_span(&args[1], "period_start_of");
                // A literal fractional index is refused here; the runtime
                // checks a computed one.
                if let ValueExpr::Term(Term::Literal(Value::Decimal(s))) = &args[2]
                    && s.parse::<rust_decimal::Decimal>()
                        .is_ok_and(|d| !d.is_integer())
                {
                    self.report(|context| ValidationError::PeriodIndexNotWhole {
                        index: s.clone(),
                        context,
                    });
                }
                InferredKind::Known(PredicateArgKind::Date)
            }
            // Both arguments of one kind, which is the result kind,
            // over the kinds `ordered_domain` allows.
            Builtin::Min | Builtin::Max => {
                let name = builtin.name();
                let left = self.infer_value(&args[0], scope);
                let right = self.infer_value(&args[1], scope);
                match (left, right) {
                    (InferredKind::Known(a), InferredKind::Known(b)) if a == b => {
                        self.ordered_domain(name, a)
                    }
                    (InferredKind::Known(a), InferredKind::Known(b)) => {
                        self.report(|context| ValidationError::OperandKindMismatch {
                            operator: name,
                            expected: a,
                            actual: b,
                            suggestion: None,
                            context,
                        });
                        InferredKind::UnknownOrAny
                    }
                    // One side known: the other must agree, so a bare
                    // variable refines (`min(x, 1.0)` makes `x` a decimal).
                    (InferredKind::Known(k), InferredKind::UnknownOrAny) => {
                        let resolved = self.ordered_domain(name, k);
                        if let InferredKind::Known(k) = &resolved {
                            self.check_operand_kind(&args[1], k.clone(), name, scope);
                        }
                        resolved
                    }
                    (InferredKind::UnknownOrAny, InferredKind::Known(k)) => {
                        let resolved = self.ordered_domain(name, k);
                        if let InferredKind::Known(k) = &resolved {
                            self.check_operand_kind(&args[0], k.clone(), name, scope);
                        }
                        resolved
                    }
                    _ => InferredKind::UnknownOrAny,
                }
            }
        }
    }

    /// The period builtins' span rule: a literal zero span is refused
    /// here by name; the runtime checks one passed in through a
    /// parameter.
    fn refuse_literal_zero_span(&mut self, span_arg: &ValueExpr, builtin: &'static str) {
        if let ValueExpr::Term(Term::Literal(Value::CalendarSpan(text))) = span_arg
            && let Ok(parsed) = crate::calendar::parse_calendar_span(text)
            && parsed.months == 0
            && parsed.days == 0
        {
            self.report(|context| ValidationError::PeriodSpanNotPositive {
                builtin,
                span: parsed.to_string(),
                context,
            });
        }
    }

    /// The kinds `min`/`max` accept: those the language already orders,
    /// dates and timestamps included.
    ///
    /// Calendar spans are excluded: P1M against P30D has no answer
    /// without a date, so spans compare only for equality.
    fn ordered_domain(&mut self, builtin: &'static str, kind: PredicateArgKind) -> InferredKind {
        if matches!(
            kind,
            PredicateArgKind::Decimal
                | PredicateArgKind::Quantity(_)
                | PredicateArgKind::Duration
                | PredicateArgKind::Date
                | PredicateArgKind::Timestamp
        ) {
            return InferredKind::Known(kind);
        }
        self.report(|context| ValidationError::BuiltinKind {
            builtin,
            kind,
            context,
        });
        InferredKind::UnknownOrAny
    }

    /// Record an error at the current context.
    fn report(&mut self, error: impl FnOnce(ValidationContext) -> ValidationError) {
        let context = self.context.clone();
        self.errors.push(error(context));
    }

    /// A variable used where a bound value is required. Flags
    /// `UnboundVariable` if nothing has bound it at this point.
    fn use_var(&mut self, scope: &Scope, name: &Var) {
        if !scope.bound.is_bound(name) {
            self.report(|context| ValidationError::UnboundVariable {
                variable: name.to_string(),
                context,
            });
        }
    }

    /// Check a predicate reference end to end: declared, right
    /// arity, then arg kinds (and, per `mode`, binding or use of
    /// its variable arguments).
    fn check_predicate_ref(
        &mut self,
        predicate: &str,
        args: &[Term],
        mode: RefMode,
        scope: &mut Scope,
    ) {
        self.check_reference(VocabularyKind::Predicate, predicate, args, mode, scope);
    }

    /// Check a definition call: declared, right arity, then each
    /// argument against the inferred signature. A parameter the body
    /// binds can bind an unbound argument; a use-only parameter needs
    /// its argument already bound, as at runtime.
    fn check_defined_call(&mut self, name: &str, args: &[Term], scope: &mut Scope) {
        let Some(def) = self.definitions.get(name).copied() else {
            self.report(|context| ValidationError::Undeclared {
                vocabulary: VocabularyKind::Definition,
                name: name.into(),
                context,
            });
            return;
        };
        if def.parameters.len() != args.len() {
            self.report(|context| ValidationError::ArityMismatch {
                vocabulary: VocabularyKind::Definition,
                name: name.into(),
                expected: def.parameters.len(),
                actual: args.len(),
                context,
            });
        }
        // Absent only for cyclic IR, which `validate_program` rejects
        // first.
        let Some(sig) = self.definition_sigs.get(name) else {
            return;
        };
        let param_kinds = sig.param_kinds.clone();
        let generator = sig.generator.clone();
        let n = args.len().min(param_kinds.len());
        for (position, arg) in args.iter().take(n).enumerate() {
            let expected = param_kinds[position].clone();
            match arg {
                Term::Var(var_name) => {
                    if generator[position] {
                        scope.bound.bind(var_name);
                    } else {
                        self.use_var(scope, var_name);
                    }
                    self.observe_or_report(scope, var_name, expected);
                }
                Term::Wildcard => {
                    if !generator[position] {
                        // The body never binds this parameter and a
                        // wildcard supplies nothing, as the runtime
                        // would report.
                        self.report(|context| ValidationError::UnboundVariable {
                            variable: def.parameters[position].to_string(),
                            context,
                        });
                    }
                }
                other => {
                    if let (InferredKind::Known(expected_kind), InferredKind::Known(actual_kind)) =
                        (expected, term_kind(other))
                        && !kinds_compatible(&expected_kind, &actual_kind)
                    {
                        self.report(|context| ValidationError::ArgKindMismatch {
                            vocabulary: VocabularyKind::Definition,
                            name: name.into(),
                            position,
                            expected: expected_kind,
                            actual: actual_kind,
                            context,
                        });
                    }
                }
            }
        }
    }

    /// Same, against the intent vocabulary; powers `Stmt::Emit`.
    fn check_intent_ref(&mut self, intent: &str, args: &[Term], mode: RefMode, scope: &mut Scope) {
        self.check_reference(VocabularyKind::Intent, intent, args, mode, scope);
    }

    /// Declared, arity, and argument checks for a reference in either
    /// vocabulary. `.copied()` releases the borrow of `self` before the
    /// `&mut self` argument walk.
    fn check_reference(
        &mut self,
        vocabulary: VocabularyKind,
        name: &str,
        args: &[Term],
        mode: RefMode,
        scope: &mut Scope,
    ) {
        // Nothing admits a derived predicate, so a rule reading one never
        // fires, and one writing it gives the name two sources. Refused
        // here, except inside a generated discipline invariant: the
        // clause behind it is already refused at the declaration.
        let generated_discipline_rule = matches!(
            &self.context,
            ValidationContext::Invariant { name } if self.generated_invariants.contains(name.as_str())
        );
        if vocabulary == VocabularyKind::Predicate
            && self.derived_heads.contains(name)
            && !generated_discipline_rule
        {
            self.report(|context| ValidationError::DerivedInRule {
                predicate: name.into(),
                context,
            });
        }
        let decl_args = match vocabulary {
            VocabularyKind::Predicate => self.predicates.get(name).copied().map(|d| &d.args),
            VocabularyKind::Intent => self.intents.get(name).copied().map(|d| &d.args),
            // Definition calls are checked by the `Prop::Defined` walk,
            // never here. If one arrives, it reports as Undeclared.
            VocabularyKind::Definition => None,
            // Rules naming a derived head are refused above as
            // DerivedInRule; if one arrives, it reports as Undeclared.
            VocabularyKind::Derived => None,
        };
        let Some(decl_args) = decl_args else {
            // Naming a definition here gets its own error: admit, retract,
            // and value need a claim, not a definition.
            if vocabulary == VocabularyKind::Predicate && self.definitions.contains_key(name) {
                self.report(|context| ValidationError::UnresolvedDefinitionCall {
                    name: name.into(),
                    context,
                });
            } else {
                self.report(|context| ValidationError::Undeclared {
                    vocabulary,
                    name: name.into(),
                    context,
                });
            }
            return;
        };
        if decl_args.len() != args.len() {
            self.report(|context| ValidationError::ArityMismatch {
                vocabulary,
                name: name.into(),
                expected: decl_args.len(),
                actual: args.len(),
                context,
            });
        }
        // Arity is reported above; the kinds are checked over the
        // positions both sides have.
        for (position, (arg, decl_arg)) in args.iter().zip(decl_args).enumerate() {
            self.check_one_arg(
                vocabulary,
                name,
                position,
                arg,
                decl_arg.kind.clone(),
                mode,
                scope,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn check_one_arg(
        &mut self,
        vocabulary: VocabularyKind,
        name: &str,
        position: usize,
        arg: &Term,
        expected: PredicateArgKind,
        mode: RefMode,
        scope: &mut Scope,
    ) {
        // Spans are expression-only. Refuse one in any claim or intent
        // argument here, even in an `Any` slot, rather than on every
        // proposal at runtime.
        if matches!(resolved_term_kind(arg, &scope.kinds), InferredKind::Known(k) if k == PredicateArgKind::CalendarSpan)
        {
            self.report(|context| ValidationError::CalendarSpanEscapesExpression {
                place: format!("argument #{position} of {vocabulary} `{name}`"),
                context,
            });
            return;
        }
        let actual = term_kind(arg);
        if let Term::Var(var_name) = arg {
            match mode {
                RefMode::Match => scope.bound.bind(var_name),
                RefMode::Use => self.use_var(scope, var_name),
            }
            if let Err((previous, new)) =
                scope.kinds.observe(var_name, InferredKind::Known(expected))
            {
                self.report(|context| ValidationError::VariableKindConflict {
                    variable: var_name.as_str().to_string(),
                    previous,
                    new,
                    context,
                });
            }
            // VariableKindConflict is the right diagnostic when the
            // variable already held an incompatible kind.
        } else if let InferredKind::Known(actual_kind) = actual
            && !kinds_compatible(&expected, &actual_kind)
        {
            self.report(|context| ValidationError::ArgKindMismatch {
                vocabulary,
                name: name.into(),
                position,
                expected,
                actual: actual_kind,
                context,
            });
        }
    }

    /// Observe `name` at `kind` in the scope's kind environment; on
    /// a refinement conflict, push a `VariableKindConflict`.
    fn observe_or_report(&mut self, scope: &mut Scope, name: &Var, kind: InferredKind) {
        if let Err((previous, new)) = scope.kinds.observe(name, kind) {
            self.report(|context| ValidationError::VariableKindConflict {
                variable: name.to_string(),
                previous,
                new,
                context,
            });
        }
    }
}

/// One side of an equality check: the inferred kind, plus the
/// variable name if the operand was a bare variable (so a refined
/// kind can be written back to the env).
type EqualityOperand<'a> = (InferredKind, Option<&'a Var>);

fn value_var_name(expr: &ValueExpr) -> Option<&Var> {
    match expr {
        ValueExpr::Term(Term::Var(name)) => Some(name),
        _ => None,
    }
}

/// Resolve a `Term`'s kind through the kind env: variables look up
/// their current inferred kind; literals and `actor` return their
/// inherent kind. Wildcard stays UnknownOrAny.
fn resolved_term_kind(term: &Term, kinds: &KindEnv) -> InferredKind {
    match term {
        Term::Var(name) => kinds.lookup(name),
        other => term_kind(other),
    }
}

/// Prefer the more specific of two compatible kinds. `Any` loses
/// to a concrete kind; otherwise the kinds are equal.
fn more_specific(a: PredicateArgKind, b: PredicateArgKind) -> PredicateArgKind {
    if matches!(a, PredicateArgKind::Any) {
        b
    } else {
        a
    }
}

/// Look up the kind of the value position in a `ValueOf` lookup:
/// the declared kind at the lookup's `extract` index, or UnknownOrAny
/// when the predicate is undeclared or the index is out of range
/// (`InvalidValueExtraction` reports that shape separately).
fn value_of_result_kind(
    predicate: &str,
    extract: usize,
    predicates: &HashMap<&str, &PredicateDecl>,
) -> InferredKind {
    let Some(decl) = predicates.get(predicate) else {
        return InferredKind::UnknownOrAny;
    };
    decl.args
        .get(extract)
        .map(|a| InferredKind::Known(a.kind.clone()))
        .unwrap_or(InferredKind::UnknownOrAny)
}

/// Whether a `Term` is `Term::Actor`.
fn is_actor(t: &Term) -> bool {
    matches!(t, Term::Actor)
}

/// Whether a proposition references `Term::Actor` anywhere, to flag
/// `actor` in invariant and derived-claim bodies.
fn prop_mentions_actor(prop: &Prop) -> bool {
    fold::any_term_in_prop(prop, &|t, _| is_actor(t))
}

/// Value-sort companion to [`prop_mentions_actor`].
fn value_mentions_actor(expr: &ValueExpr) -> bool {
    fold::any_term_in_value(expr, &|t, _| is_actor(t))
}

/// Every rule name a statement carries, including inside `for` bodies.
fn collect_rule_names<'s>(stmt: &'s Stmt, out: &mut Vec<&'s RuleName>) {
    fold::walk_stmt(stmt, &mut |n| {
        if let fold::Node::Stmt(Stmt::Require { name, .. } | Stmt::BindOne { name, .. }) = n {
            out.extend(name.as_ref());
        }
    });
}

/// Whether `name` occurs in any term position, honouring quantifier
/// shadowing. Flags a definition parameter the body never uses: the
/// body cannot bind it, and a ground argument would be ignored.
fn occurs_in_prop(name: &Var, prop: &Prop) -> bool {
    fold::any_term_in_prop(
        prop,
        &|t, binders| matches!(t, Term::Var(v) if v == name && !binders.contains(&v)),
    )
}

/// Inherent kind of a `Term`. Variables are `UnknownOrAny` here;
/// callers that want the env-resolved kind look it up separately.
fn term_kind(term: &Term) -> InferredKind {
    match term {
        Term::Var(_) | Term::Wildcard => InferredKind::UnknownOrAny,
        Term::Actor => InferredKind::Known(PredicateArgKind::Subject),
        Term::Literal(Value::Subject(_)) => InferredKind::Known(PredicateArgKind::Subject),
        Term::Literal(Value::Decimal(_)) => InferredKind::Known(PredicateArgKind::Decimal),
        Term::Literal(Value::Date(_)) => InferredKind::Known(PredicateArgKind::Date),
        Term::Literal(Value::Timestamp(_)) => InferredKind::Known(PredicateArgKind::Timestamp),
        Term::Literal(Value::Duration(_)) => InferredKind::Known(PredicateArgKind::Duration),
        Term::Literal(Value::CalendarSpan(_)) => {
            InferredKind::Known(PredicateArgKind::CalendarSpan)
        }
        Term::Literal(Value::Quantity { unit, .. }) => {
            InferredKind::Known(PredicateArgKind::Quantity(unit.clone()))
        }
    }
}

#[cfg(test)]
mod tests;
