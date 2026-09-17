//! The static-check traversal. One walk over every invariant,
//! transformation, and derived-claim body surfaces the problems the
//! runtime would otherwise raise during a `propose`:
//!
//! - **kind/type compatibility** - values flowing into a slot, a
//!   comparator, or an arithmetic operand must match the declared or
//!   fixed expected kind (`EvalError::TypeMismatch`);
//! - **binding flow** - a name consumed where a bound value is
//!   required must have been bound first, following the runtime
//!   quartet's export rules (`EvalError::UnboundVariable`);
//! - **actor context** - `Term::Actor` in an invariant or derived body,
//!   where no proposing transition is in scope (`UnboundActor`).
//!
//! The predicate-vs-value shape boundary is no longer policed here: the
//! IR's two sorts ([`Prop`] and [`ValueExpr`]) make a value expression
//! at a predicate position - or the reverse - unrepresentable, so the
//! walk splits by sort ([`CheckCtx::walk_prop`] and
//! [`CheckCtx::infer_value`]) instead of checking shape at each node.
//!
//! A [`Scope`] threads kind inference and runtime-binding state
//! together, cloned at the boundaries (`require`, `sum`, `for`,
//! `or`-branches) where the quartet's non-export rules apply, so those
//! rules fall out of the structure rather than from special-casing.
//!
//! `Any` is unconstrained, not a kind-eraser: a variable seen first
//! through an `Any` slot stays open and refines to a specific kind on
//! its next concrete use.
//!
//! Diagnostics ship without source spans in v0; the IR drops parser
//! spans on lowering.

use std::collections::{HashMap, HashSet};

use crate::fold;
use crate::format::{arith_token, compare_token};
use crate::ir::{
    ArithOp, Builtin, CompareOp, OrderedDomain, PredicateArgKind, PredicateDecl, Program, Prop,
    RuleName, Stmt, SumSeed, Term, Value, ValueExpr, Var, arith_result_kind,
    arith_unique_counterpart,
};
use crate::validate::{ValidationContext, ValidationError, VocabularyKind};

/// Inferred kind of a value during static analysis. Distinct from
/// [`PredicateArgKind`] (which is the *declared* kind on a predicate
/// position) because variables can be observed-but-not-yet-pinned -
/// the `UnknownOrAny` state. A variable seen only through an `Any`
/// slot stays unconstrained and refines to a specific kind when
/// later observed in a specific slot.
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

/// Compatibility rule for two specific declared kinds. `Any` on
/// either side is the declaration-level escape hatch; otherwise
/// strict equality is required.
fn kinds_compatible(a: &PredicateArgKind, b: &PredicateArgKind) -> bool {
    *a == PredicateArgKind::Any || *b == PredicateArgKind::Any || a == b
}

/// The comparator that DOES order `actual`, spelled for the same
/// comparison sense - `on_or_before` for a Date under `<=`, `before`
/// for a Date under `<`. `None` when nothing orders the kind.
fn comparator_suggestion(op: CompareOp, actual: &PredicateArgKind) -> Option<&'static str> {
    OrderedDomain::for_concrete_kind(actual).map(|domain| compare_token(op, domain))
}

/// Scope-local map from variable name to inferred kind. Mutable
/// during expression and statement walks; passed by `&mut` through
/// the recursive checker. Distinct kind environments live per
/// invariant body, per derived-claim body, per transformation
/// (extended statement-by-statement following the runtime quartet
/// doctrine).
#[derive(Debug, Default, Clone)]
pub(crate) struct KindEnv {
    bindings: HashMap<Var, InferredKind>,
}

impl KindEnv {
    /// Look up a variable's current inferred kind. Returns
    /// `UnknownOrAny` for variables never observed before - that
    /// matches how an unconstrained slot would treat them.
    pub(crate) fn lookup(&self, name: &Var) -> InferredKind {
        self.bindings
            .get(name)
            .cloned()
            .unwrap_or(InferredKind::UnknownOrAny)
    }

    /// Observe a variable at the given inferred kind. Refines the
    /// stored kind if compatible; reports a conflict otherwise.
    ///
    /// The conflict tuple is `(previous, new)` so the caller can
    /// emit a `VariableKindConflict` diagnostic with both kinds
    /// named.
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

/// Set of variable names that are runtime-bound (available) at a
/// point in the walk. Distinct from [`KindEnv`]: a variable can be
/// kind-known but not bound (e.g. matched inside a `require`, whose
/// bindings do not export). Cloned at the same scope boundaries as
/// `KindEnv` so the quartet's non-export rules fall out for free.
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

    /// The bound names themselves, for walks that must merge kind
    /// evidence about entry-bound variables back across a non-export
    /// boundary (the conditional's condition).
    fn names(&self) -> impl Iterator<Item = &Var> {
        self.bound.iter()
    }

    /// Keep only variables also bound in `other`. Used to merge
    /// `or`-branch bindings: a variable is guaranteed bound after a
    /// disjunction only if every branch bound it, since the runtime
    /// carries whichever branch's witness forward and a name absent
    /// from some branch may be unbound at a later conjunct.
    fn intersect_with(&mut self, other: &BoundEnv) {
        self.bound.retain(|v| other.bound.contains(v));
    }
}

/// Per-walk mutable analysis state: the kind environment and the
/// bound-variable environment, threaded together and cloned
/// together at scope boundaries (`require`, `sum`, `for`, and
/// `or`-branches). Pairing them is what lets one traversal do both
/// kind inference and unbound-variable detection.
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

/// Whether a reference's variable arguments are being *introduced*
/// or *consumed*. A claim in predicate position (`require`, `bind`,
/// invariant body, `forall` source, `exists` body) matches against
/// state, so its variables become bound (`Match`). A claim or
/// intent in `admit` / `retract` / `emit`, and the key arguments of
/// a `value` lookup, consume already-bound values (`Use`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RefMode {
    Match,
    Use,
}

/// The static-check visitor. Holds the programme's declared
/// vocabularies, the current `ValidationContext`, and the
/// accumulating error list. The per-walk [`Scope`] (kind +
/// bound-variable environments) is passed separately because it is
/// cloned at scope boundaries - `require`, `sum`, `for`, and
/// `or`-branches each walk a clone whose refinements and bindings
/// do not leak back.
struct CheckCtx<'a> {
    predicates: HashMap<&'a str, &'a PredicateDecl>,
    intents: HashMap<&'a str, &'a crate::IntentDecl>,
    definitions: HashMap<&'a str, &'a crate::Definition>,
    /// Inferred call signature per definition, computed callees-first
    /// before any caller body is walked.
    definition_sigs: HashMap<String, DefinitionSig>,
    /// Predicates a `derived` declaration computes. The kernel evaluates
    /// against admitted claims, and a derived is a read model refreshed
    /// out of band, so naming one anywhere a rule reads state is a
    /// modelling error rather than a rule that happens to match nothing.
    derived_heads: std::collections::BTreeSet<&'a str>,
    /// Invariant names the discipline lowering produced, so a finding
    /// inside one can be attributed to the declaration instead.
    generated_invariants: std::collections::BTreeSet<String>,
    context: ValidationContext,
    errors: Vec<ValidationError>,
}

/// Run the static checks over the whole programme. Returns the
/// full list of detected problems; an empty `Vec` means the
/// programme passes. Traversal order is invariants, then
/// transformations, then derived claims, so merged diagnostics
/// come out in a predictable shape.
/// Inferred call signature of a definition: the per-parameter kind the
/// body observes, plus whether the body itself binds the parameter. A
/// body-bound parameter is generator-capable - a call argument may
/// arrive unbound and receive its value from the body's matches. A
/// parameter the body only *uses* (a window date in a comparator, say)
/// must arrive bound at every call, exactly as the runtime requires.
struct DefinitionSig {
    param_kinds: Vec<InferredKind>,
    generator: Vec<bool>,
}

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
    // below checks against an already-inferred signature. The order is
    // total because `validate_program` has already rejected cycles;
    // the `unwrap_or_default` is the defensive no-op for direct calls
    // on cyclic IR.
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
        // Classification walk: which parameters does the body itself
        // bind? Parameters start unbound and the probe's errors are
        // discarded - it asks one question, and the runtime-faithful
        // binding flow of the ordinary walk answers it.
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
        // transformation parameters, so kinds refine on use and the
        // body's own problems report once.
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
        // Parameters arrive bound and untyped: bound so later uses
        // are available, untyped so their kind refines on use. The
        // first kind observation against UnknownOrAny never conflicts.
        for param in &transformation.parameters {
            scope.bound.bind(param);
            let _ = scope.kinds.observe(param, InferredKind::UnknownOrAny);
        }
        // A name identifies one rule, so two rules answering to the same
        // name inside one transformation would make a refusal ambiguous -
        // which is the whole thing a name is for. Scoped to the
        // transformation, not the programme: two acts legitimately carry
        // the same gate verbatim.
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

        // The context carries the statement index, so a finding lands
        // on the statement it was made in, not just the body. A
        // finding inside a nested `for` keeps the top-level index.
        for (index, stmt) in transformation.body.iter().enumerate() {
            cx.context = ValidationContext::Transformation {
                name: transformation.name.to_string(),
                statement: Some(index),
            };
            cx.walk_stmt(stmt, &mut scope);
        }

        // A parameter whose inference lands on CalendarSpan (the body
        // uses it as a span operand) has no lawful argument vector:
        // spans are expression-only and no transition argument may
        // carry one. Checked after the walk so a refinement made by a
        // later statement still counts.
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
        // Disciplines are promises about governed state - what may be
        // retracted, which claims agree, which pointer is current. A
        // derived output is computed and its generations are replaced
        // wholesale on refresh, so it can keep none of them. Caught at
        // the declaration because that is where the author wrote the
        // clause: `unique by` lowers to a generated invariant, and
        // refusing THAT names a rule nobody typed, while `append only`
        // lowers to nothing and would pass unnoticed.
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
        // The domain binds its variables (claim matches), but values
        // run once per distinct head-key tuple, after witnesses
        // collapse - the runtime binds ONLY the head keys there. The
        // checker mirrors that topology: values are inferred against a
        // scope holding just the keys (kinds copied from the domain
        // walk), and an unbound use that the domain DOES bind converts
        // to the dedicated not-a-key refusal with both remedies.
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
        // A derived output is a governed read-side value; a span is
        // not one, whatever the output declaration says.
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

        // Output args check: the runtime emits claims of the form
        // `predicate(key_0, ..., key_K-1, value_0, ..., value_V-1)`.
        // The output predicate must be declared, its arity must
        // equal keys+values, and each position must match the
        // declared kind.
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
                // The value scope seeded each key's kind from the
                // domain walk and value inference may have refined it
                // since (a lookup consuming the key pins its declared
                // kind), so it is the more informed environment here.
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
    /// Walk a proposition. Threads the scope through composition
    /// (`And`, `Implies`, `Pre`); `Or` branches walk a clone so
    /// neither a refinement nor a binding in one branch reaches
    /// another. A claim here is in `Match` position - its variables
    /// become bound.
    fn walk_prop(&mut self, prop: &Prop, scope: &mut Scope) {
        match prop {
            Prop::Claim { predicate, args } => {
                // A claim-shaped node naming a definition means the
                // resolution pass was skipped (hand-built IR): fail
                // loudly with guidance instead of an Undeclared that
                // would mislead.
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
                // Disjuncts evaluate against the same base context
                // (mirrors `find_disjunction`): a branch's binding or
                // refinement must not leak to a sibling branch. But
                // the disjunction's witness flows to later conjuncts
                // (`find_conjunction` threads each conjunct's matches
                // into the next), so a variable bound in EVERY branch
                // is guaranteed bound after the `or`. Join the
                // intersection of branch-bound names into the live
                // scope; refinements are dropped (a missed refinement
                // risks only a false negative, never a false positive).
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
                // Same binding flow as the `(a or b)` it lowers to: a
                // name is guaranteed bound after the xor only if BOTH
                // operands bind it, so join the intersection (mirrors the
                // Or arm). The `not (a and b)` half binds nothing; both
                // operands are use-checked here.
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
                // The binding is introduced by the quantifier;
                // mark it bound before the body. No shadowing of an
                // outer variable of the same name (the runtime
                // unifies); binding it again is idempotent.
                scope.bound.bind(binding);
                self.walk_prop(body, scope);
            }
            Prop::Forall {
                binding,
                source,
                body,
            } => {
                // The binding ranges over `source`; mark it bound so
                // both the source (when auto-lifted to `e in coll`)
                // and the body see it. The source/body run in the
                // live scope - conservative: a forall-introduced
                // name may stay visible to a sibling conjunct rather
                // than risk a false positive by scoping it away.
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
                    // The decimal ordered domain admits two flavours: bare
                    // decimals and unit-tagged quantities (a `Decimal[U]`
                    // IS a decimal, under a contract label the comparison
                    // must respect). Both operands must share one flavour.
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
                // `In` is a generator-or-filter (mirrors
                // `find_in_matches`): an unbound element variable is
                // bound to each collection item; a bound one filters.
                // Either way the element is bound afterward, so it is
                // never a use. The collection must already be bound
                // and Collection-kinded.
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

    /// Walk a statement, threading the scope per the runtime
    /// require/bind/let/for quartet:
    ///
    /// - `Require` walks a clone (matches and refinements do not
    ///   export - this is the key binding-flow rule).
    /// - `BindOne` walks the live scope (its matches bind and flow
    ///   forward).
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
                // Body runs under a scoped clone so the loop binding
                // and any body-introduced names do not leak across
                // iterations or beyond the loop.
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
        // A variable read through `abs(...)` refines toward `expected` too,
        // when abs preserves that kind - so `abs(d) <cmp> duration(...)`
        // pins `d` to Duration. Done before inferring, so the inference
        // then sees the refined operand.
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

    /// Both operands of a temporal-domain (`Date`/`Timestamp`/
    /// `Duration`) comparison, under the same pair discipline as the
    /// decimal domain: an ordered comparison may infer operand kinds
    /// only after BOTH operands are admissible to the domain it names.
    /// Judging comes first; a refused comparison then contributes no
    /// inference at all, because refining the healthy side out of a
    /// comparison already known to be ill-typed would push a spurious
    /// conflict onto its later uses. Unlike the generic
    /// [`Self::check_operand_kind`] (which also serves `for`,
    /// arithmetic, `round`, and the period builtins, and must keep its
    /// diagnostics), a refused bare variable is an operand mismatch
    /// naming the comparator that WOULD order it, never a
    /// variable-kind conflict. Unknown operands of a clean pair refine
    /// toward the domain's kind (how `on_or_before` pins a free
    /// parameter to Date).
    fn check_temporal_domain_operands(
        &mut self,
        left: &ValueExpr,
        right: &ValueExpr,
        op: CompareOp,
        domain: OrderedDomain,
        scope: &mut Scope,
    ) {
        let expected = match domain {
            // Never called with Decimal (the two-flavour pair rule owns
            // it); the total mapping keeps this helper panic-free.
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
                // A variable inside `abs(...)` refines too, so
                // `abs(gap) no_longer_than allowed` pins `gap` to
                // Duration.
                self.refine_through_abs(operand, &expected, scope);
            }
        }
    }

    /// One temporal-domain operand: report a KNOWN kind the domain does
    /// not order, naming the comparator that would, and say whether it
    /// was refused. Judgment only - refinement is the pair's decision,
    /// made after both verdicts.
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
    /// when `expected` is a kind abs preserves (decimal, quantity,
    /// duration). Bare variables are refined by the callers directly; this
    /// reaches the one a kind-preserving unary wraps, so `abs(x) <= 10`
    /// still pins `x` to Decimal. A non-abs operand, or an abs in a
    /// non-magnitude comparison (where abs is itself an error), is left
    /// alone.
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
        // Peel nested `abs(abs(x))` down to the variable underneath:
        // every layer preserves the kind, so the expectation reaches
        // the operand unchanged.
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
    /// refused. A bare variable of unknown kind is a use whose flavour
    /// the cross-refinement step decides (the other operand); a KNOWN
    /// kind - variable or not - outside the domain's two flavours (bare
    /// decimal, unit-tagged quantity) is reported against the
    /// bare-decimal expectation and degrades to unknown, so it is
    /// neither reported twice nor refined toward the bad kind.
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

    /// Both operands of a decimal-domain comparison: each must be a
    /// bare decimal or a quantity, and the two must agree - two bare
    /// decimals, or two quantities of the SAME unit. A known side
    /// refines an unknown variable to its own flavour (so
    /// `settled <= due` infers the settlement parameter at the due
    /// figure's unit); two unknowns default to the bare-decimal
    /// flavour, the domain's neutral reading.
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
        // A comparison already outside its domain contributes no
        // cross-operand inference: refining the healthy side toward the
        // decimal default would push a second, spurious conflict onto
        // every later use of that variable.
        if l_refused || r_refused {
            return;
        }
        let refine =
            |this: &mut Self, operand: &ValueExpr, kind: PredicateArgKind, scope: &mut Scope| {
                if let ValueExpr::Term(Term::Var(name)) = operand {
                    this.observe_or_report(scope, name, InferredKind::Known(kind));
                } else {
                    // A variable inside `abs(...)` refines too, so
                    // `abs(x) <= 10` pins `x` to Decimal.
                    this.refine_through_abs(operand, &kind, scope);
                }
            };
        match (l, r) {
            (InferredKind::Known(a), InferredKind::Known(b)) => {
                if kinds_compatible(&a, &b) {
                    // Compatible pair: variable operands still refine
                    // toward the more specific side (`Any` from a
                    // polymorphic slot loses to the literal's kind).
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

    /// Strict equality between two value operands. If both produce
    /// a `Known` kind they must be compatible; when one is a bare
    /// variable and the other contributes a concrete kind, the
    /// variable refines to it. `Subject == Decimal` is a kind
    /// error, never a coercion. Backs both `Eq` and `Neq` (both
    /// take `ValueExpr` operands).
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

    /// The shared two-value kind unification: `Any` compatibility,
    /// most-specific joining, bare-variable write-back. Equality and
    /// the conditional's branches share the algebra and own their
    /// diagnostics through `mismatch`. Returns the combined kind so a
    /// caller that IS a value expression (the conditional) can carry
    /// it as its own inferred kind.
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
    /// (must be bound); literals carry their kind; `Arith` recursively
    /// checks Decimal operands and returns Decimal; `Sum` returns Decimal
    /// after a body-first walk under a cloned scope; `ValueOf` returns its
    /// wildcard slot's declared kind.
    fn infer_value(&mut self, expr: &ValueExpr, scope: &mut Scope) -> InferredKind {
        match expr {
            ValueExpr::Term(term) => {
                if let Term::Var(name) = term {
                    self.use_var(scope, name);
                }
                // A wildcard never carries a value. Claim patterns and
                // `value` lookups hold their wildcards as bare `Term`s,
                // so anything reaching this arm is a value position.
                // Deduplicated per context: operand-checking paths can
                // infer the same node twice, and one wildcard is one
                // finding.
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
            // The condition walks under a cloned scope (its bindings
            // do not export - `require`'s rule); the branches infer
            // against the OUTER scope and unify with no ordering
            // allow-list: selection is not ordering, so subject tags,
            // booleans, and collections are lawful branch kinds.
            ValueExpr::Cond {
                when,
                then,
                otherwise,
            } => {
                let mut scoped = scope.clone();
                self.walk_prop(when, &mut scoped);
                // Witnesses do not export, but the condition's USE of
                // a variable already bound on entry is ordinary kind
                // evidence (`Member(who)` pins `who: Subject`), and
                // dropping it with the clone would let the branch
                // unification below refine the same variable to a
                // contradictory kind that only fails at runtime.
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
                // Every operator flows through the rule matrix (Mul /
                // Div stopped being decimal-only when quantities
                // brought scaling and ratios). Infer both sides; when
                // both are known, the matrix decides (and a missing
                // rule is an error here, at authoring time, not at
                // evaluation). When one side is known and exactly one
                // rule fits it, the other side is forced and a bare
                // variable there is refined.
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
                    // One side known: when exactly one rule fits that
                    // side, the other side's kind is forced and a bare
                    // variable there is refined (an externally supplied
                    // turn time in `tendered_at + turn_time` infers
                    // Duration). When several rules fit (`Timestamp -
                    // x` could subtract an instant or a span), nothing
                    // is assumed.
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
                    // Both unknown: Mul / Div / Mod keep their
                    // historical bare-decimal default (a unit cannot
                    // be inferred from nothing, and `rate * factor`
                    // with two free parameters has always read as
                    // decimal arithmetic). Add / Sub / Min / Max stay
                    // unrefined, as the time kinds left them.
                    _ if matches!(op, ArithOp::Mul | ArithOp::Div | ArithOp::Mod) => {
                        self.check_operand_kind(left, PredicateArgKind::Decimal, operator, scope);
                        self.check_operand_kind(right, PredicateArgKind::Decimal, operator, scope);
                        InferredKind::Known(PredicateArgKind::Decimal)
                    }
                    _ => InferredKind::UnknownOrAny,
                }
            }
            ValueExpr::Extremum { op, value, body } => {
                // Body-first on a cloned scope, as `Sum` does, so
                // body-bound names do not leak outward.
                let mut scoped = scope.clone();
                self.walk_prop(body, &mut scoped);
                if let Term::Var(name) = value {
                    self.use_var(&scoped, name);
                }
                let resolved = resolved_term_kind(value, &scoped.kinds);
                // An extremum yields one of the members it ranged over,
                // so its kind is the member kind - provided that kind has
                // an order at all.
                if let InferredKind::Known(actual) = resolved {
                    // An allow-list, not an enumeration of the
                    // unordered kinds: a kind added later has no order
                    // until someone gives it one, and defaulting to
                    // refuse keeps that decision explicit. Collections
                    // are what the first cut let through.
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
                // Body-first inference on a cloned scope so body-
                // bound names (the iteration binding, plus any
                // others the body introduces) do not leak into the
                // surrounding expression. Outer bindings stay
                // visible via the clone. The target is a full value
                // expression inferred in that scope, so an arith rule
                // violation or unbound variable inside it is reported
                // with the body's bindings in force.
                let mut scoped = scope.clone();
                self.walk_prop(body, &mut scoped);
                let resolved = self.infer_value(value, &mut scoped);
                // A sum of durations is the laytime-counting shape; a
                // sum of decimals is every aggregate before it. Any
                // other known kind is an authoring-time error.
                if let InferredKind::Known(
                    k @ (PredicateArgKind::Duration | PredicateArgKind::Quantity(_)),
                ) = resolved
                {
                    // The seed pass sees less than this scope does (an
                    // outer-bound variable, a builtin call), so the two
                    // authorities can disagree - and the stored seed is
                    // what an empty sum evaluates to. A mismatch is a
                    // guaranteed runtime type error on the first empty
                    // book; refuse it here instead.
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
                    // The runtime returns either the looked-up value
                    // or the default, so a kind mismatch between them
                    // is the same class of error as a comparator
                    // mismatch.
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
            // One arm for every builtin; which builtin decides the
            // rule, so the decision is an exhaustive match rather than
            // a table row that could go unfilled. Arity is settled
            // first: a wrong count is refused by name before any
            // operand is judged, so the author reads one clear error
            // instead of a cascade about kinds.
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

    /// The static semantics of each builtin: what its arguments must
    /// be, what it yields, and any refusal a literal argument earns
    /// here rather than at runtime.
    ///
    /// Exhaustive over [`Builtin`] with no wildcard - `abs` preserves
    /// its operand's kind while `round` and the period builtins impose
    /// fixed ones, which is a real difference and has to be written
    /// down per builtin, not defaulted.
    fn infer_builtin(
        &mut self,
        builtin: Builtin,
        args: &[ValueExpr],
        scope: &mut Scope,
    ) -> InferredKind {
        match builtin {
            Builtin::Abs => match self.infer_value(&args[0], scope) {
                // abs preserves the kind of a signed value; any other
                // known kind is an authoring-time error.
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
                // The established operand path: refines a bare variable
                // to Decimal, accepts Any (unconstrained kinds refine at
                // a later concrete use), reports incompatible concrete
                // kinds as OperandKindMismatch.
                self.check_operand_kind(&args[0], PredicateArgKind::Decimal, "round", scope);
                self.check_operand_kind(&args[1], PredicateArgKind::Decimal, "round", scope);
                // A literal quantum must be positive; a variable quantum
                // is the runtime backstop's job.
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
                // A literal zero span must be refused here; a span
                // arriving through a defined-call parameter is the
                // runtime backstop's job (the round quantum pattern).
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
                // A literal fractional index must be refused here; an
                // index arriving computed is the runtime backstop's job.
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
            // Same-kind and kind-preserving, over the SAME domain the
            // evaluator supports: decimals, durations, and quantities
            // that agree on their unit. These rows lived in the
            // arithmetic matrix before `min`/`max` became builtins, and
            // the restriction has to move with them - two dates share a
            // kind but have no midpoint, and accepting them here would
            // only defer the refusal to runtime.
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
                    // One side known: the other must agree, so push the
                    // known kind into it. That refines a bare variable
                    // exactly as the operand path does elsewhere -
                    // `min(x, 1.0)` still tells the schema `x` is a
                    // decimal.
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

    /// The shared span rule of the period builtins, at the static
    /// tier: a literal zero span is refused here by name; a span
    /// arriving through a defined-call parameter is the runtime
    /// backstop's job (the round quantum pattern).
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

    /// The kinds `min`/`max` are defined over: those the language
    /// already orders, since taking a smaller of two is asking the
    /// comparator's question and keeping the answer instead of the
    /// verdict. Dates and timestamps are in for exactly that reason -
    /// `d1 on_or_before d2` is lawful, so "the earlier of the two" is
    /// a question with an answer.
    ///
    /// Calendar spans are deliberately OUT. They carry months and days
    /// that no common measure reconciles - P1M against P30D has no
    /// truth without a date to land on - so they compare only for
    /// equality, and an ordering here would have to invent one.
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

    /// A variable used where a bound value is required. Flags
    /// `UnboundVariable` if nothing has bound it at this point.
    /// Record an error at the current context.
    fn report(&mut self, error: impl FnOnce(ValidationContext) -> ValidationError) {
        let context = self.context.clone();
        self.errors.push(error(context));
    }

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
    /// argument against the inferred signature. A generator-capable
    /// parameter binds an unbound variable argument (like a claim
    /// match); a use-only parameter demands its argument already
    /// bound - the same distinction the runtime frame enforces.
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
        // Absent only when the topo pre-pass was skipped on cyclic IR,
        // which `validate_program` rejects before reaching here.
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
                        // The body never binds this parameter, and a
                        // wildcard argument supplies nothing - the
                        // same unbound-name failure the runtime
                        // reports for this call.
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

    /// Shared declared + arity + arg check for a reference in
    /// either vocabulary. The `.copied()` detaches the declaration
    /// from the borrow of `self`, so the subsequent `&mut self` arg
    /// walk is free of a borrow conflict.
    fn check_reference(
        &mut self,
        vocabulary: VocabularyKind,
        name: &str,
        args: &[Term],
        mode: RefMode,
        scope: &mut Scope,
    ) {
        // A derived predicate is computed from admitted claims and
        // refreshed out of band; nothing ever admits one. A rule that
        // matches one can never fire, and a rule that WRITES one gives a
        // single name two sources - the view the runtime computes and the
        // rows the transformation left. Both are refused here rather than
        // left to fail against a live database.
        // Not inside a generated discipline invariant: that rule is
        // machinery the author cannot see, and the discipline clause it
        // came from is refused at the declaration instead. Reporting both
        // would bury the actionable error under two about a rule nobody
        // wrote - the same reason the lint tier skips them.
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
            // Defined calls never route through here: a definition's
            // parameters are inferred, not declared as kinded args, so the
            // `Prop::Defined` walk checks them directly. If this arm is ever
            // reached the reference surfaces as Undeclared - loudly wrong
            // rather than silently passed.
            VocabularyKind::Definition => None,
            // A derived head is never a reference target: rules naming one
            // are refused above as DerivedInRule. Reaching here surfaces as
            // Undeclared, the same loud wrongness as a definition.
            VocabularyKind::Derived => None,
        };
        let Some(decl_args) = decl_args else {
            // A predicate-position reference that names a definition is
            // a category error with its own guidance (definitions are
            // proposition-valued; admit/retract/value need a claim),
            // not an undeclared name.
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
        // Calendar spans are expression-only, and `Any` would otherwise
        // let one through: a span literal or a span-kinded variable in
        // any claim or intent argument position is refused here, so the
        // mistake surfaces at check time rather than as the runtime's
        // own refusal on every proposal.
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

/// Whether a proposition references `Term::Actor` anywhere in its
/// tree. Used to flag `actor` in invariant and derived-claim
/// bodies, where the runtime raises `EvalError::UnboundActor`
/// because no proposing transition is in scope.
fn prop_mentions_actor(prop: &Prop) -> bool {
    fold::any_term_in_prop(prop, &|t, _| is_actor(t))
}

/// Value-sort companion to [`prop_mentions_actor`].
fn value_mentions_actor(expr: &ValueExpr) -> bool {
    fold::any_term_in_value(expr, &|t, _| is_actor(t))
}

/// Every rule name a statement carries, descending into `for` bodies - a
/// named gate inside a loop is as identifiable as one at the top level, so
/// it competes for the same names.
fn collect_rule_names<'s>(stmt: &'s Stmt, out: &mut Vec<&'s RuleName>) {
    match stmt {
        Stmt::Require { name, .. } | Stmt::BindOne { name, .. } => out.extend(name.as_ref()),
        Stmt::For { body, .. } => {
            for inner in body {
                collect_rule_names(inner, out);
            }
        }
        Stmt::Let { .. }
        | Stmt::LetNewSubject { .. }
        | Stmt::Assert(_)
        | Stmt::Retract { .. }
        | Stmt::Emit(_) => {}
    }
}

/// Whether `name` occurs in any term position of the proposition,
/// honouring quantifier shadowing. Used to flag a definition
/// parameter the body never references: such a parameter can never
/// be given a value by the body, so a call with an unbound argument
/// for it is a guaranteed runtime error, and a ground argument is
/// dead weight.
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
