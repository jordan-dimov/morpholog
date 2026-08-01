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
    ArithOp, OrderedDomain, PredicateArgKind, PredicateDecl, Program, Prop, RuleName, Stmt, Term,
    Value, ValueExpr, Var, arith_result_kind, arith_unique_counterpart,
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
        if prop_mentions_pre(&def.body) {
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
        // The domain binds the key variables (claim matches); the
        // value expressions are inferred against the same scope, so
        // they see those bindings.
        let mut scope = Scope::new();
        cx.walk_prop(&derived.domain, &mut scope);
        let value_kinds: Vec<InferredKind> = derived
            .values
            .iter()
            .map(|v| cx.infer_value(&v.expr, &mut scope))
            .collect();
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
                scope.kinds.lookup(&derived.keys[position])
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
                    let context = self.context.clone();
                    self.errors.push(ValidationError::UnresolvedDefinitionCall {
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
                let token = compare_token(*op, *domain);
                match domain {
                    // The decimal ordered domain admits two flavours: bare
                    // decimals and unit-tagged quantities (a `Decimal[U]`
                    // IS a decimal, under a contract label the comparison
                    // must respect). Both operands must share one flavour.
                    OrderedDomain::Decimal => {
                        self.check_decimal_domain_operands(left, right, token, scope);
                    }
                    OrderedDomain::Date => {
                        self.check_operand_kind(left, PredicateArgKind::Date, token, scope);
                        self.check_operand_kind(right, PredicateArgKind::Date, token, scope);
                    }
                    OrderedDomain::Timestamp => {
                        self.check_operand_kind(left, PredicateArgKind::Timestamp, token, scope);
                        self.check_operand_kind(right, PredicateArgKind::Timestamp, token, scope);
                    }
                    OrderedDomain::Duration => {
                        self.check_operand_kind(left, PredicateArgKind::Duration, token, scope);
                        self.check_operand_kind(right, PredicateArgKind::Duration, token, scope);
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
                            let context = self.context.clone();
                            self.errors.push(ValidationError::OperandKindMismatch {
                                operator: "in",
                                expected: PredicateArgKind::Collection,
                                actual,
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
            let context = self.context.clone();
            self.errors.push(ValidationError::OperandKindMismatch {
                operator,
                expected,
                actual,
                context,
            });
        }
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
        if !matches!(operand, ValueExpr::Abs(_))
            || !matches!(
                expected,
                PredicateArgKind::Decimal
                    | PredicateArgKind::Quantity(_)
                    | PredicateArgKind::Duration
            )
        {
            return;
        }
        let mut cur = operand;
        while let ValueExpr::Abs(inner) = cur {
            cur = inner;
        }
        if let ValueExpr::Term(Term::Var(name)) = cur {
            self.observe_or_report(scope, name, InferredKind::Known(expected.clone()));
        }
    }

    /// One operand of a decimal-domain comparison. A bare variable is
    /// a use whose kind is left to the cross-refinement step (the
    /// other operand decides the flavour); anything else infers, and
    /// a known kind outside the domain's two flavours (bare decimal,
    /// unit-tagged quantity) is reported against the bare-decimal
    /// expectation.
    fn infer_decimal_domain_operand(
        &mut self,
        operand: &ValueExpr,
        operator: &'static str,
        scope: &mut Scope,
    ) -> InferredKind {
        if let ValueExpr::Term(Term::Var(name)) = operand {
            self.use_var(scope, name);
            return scope.kinds.lookup(name);
        }
        let inferred = self.infer_value(operand, scope);
        if let InferredKind::Known(actual) = &inferred
            && !matches!(
                actual,
                PredicateArgKind::Decimal | PredicateArgKind::Quantity(_) | PredicateArgKind::Any
            )
        {
            let context = self.context.clone();
            self.errors.push(ValidationError::OperandKindMismatch {
                operator,
                expected: PredicateArgKind::Decimal,
                actual: actual.clone(),
                context,
            });
            // Already reported; degrade to unknown so the pair check
            // neither reports the same operand twice nor refines a
            // variable toward the bad kind.
            return InferredKind::UnknownOrAny;
        }
        inferred
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
        operator: &'static str,
        scope: &mut Scope,
    ) {
        let l = self.infer_decimal_domain_operand(left, operator, scope);
        let r = self.infer_decimal_domain_operand(right, operator, scope);
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
                    let context = self.context.clone();
                    self.errors.push(ValidationError::OperandKindMismatch {
                        operator,
                        expected: a,
                        actual: b,
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
                    let context = self.context.clone();
                    self.errors.push(mismatch(l, r, context));
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
                                let context = self.context.clone();
                                self.errors.push(ValidationError::NoArithRule {
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
                        let context = self.context.clone();
                        self.errors.push(ValidationError::UnorderedExtremum {
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
            ValueExpr::Sum {
                value,
                body,
                seed: _,
            } => {
                // Body-first inference on a cloned scope so body-
                // bound names (the iteration binding, plus any
                // others the body introduces) do not leak into the
                // surrounding expression. Outer bindings stay
                // visible via the clone. Sum's result is Decimal.
                let mut scoped = scope.clone();
                self.walk_prop(body, &mut scoped);
                if let Term::Var(name) = value {
                    self.use_var(&scoped, name);
                }
                let resolved = resolved_term_kind(value, &scoped.kinds);
                // A sum of durations is the laytime-counting shape; a
                // sum of decimals is every aggregate before it. Any
                // other known kind is an authoring-time error.
                if let InferredKind::Known(
                    k @ (PredicateArgKind::Duration | PredicateArgKind::Quantity(_)),
                ) = resolved
                {
                    return InferredKind::Known(k);
                }
                if let InferredKind::Known(actual) = resolved
                    && !kinds_compatible(&PredicateArgKind::Decimal, &actual)
                {
                    let context = self.context.clone();
                    self.errors.push(ValidationError::OperandKindMismatch {
                        operator: "sum",
                        expected: PredicateArgKind::Decimal,
                        actual,
                        context,
                    });
                }
                InferredKind::Known(PredicateArgKind::Decimal)
            }
            ValueExpr::ValueOf {
                predicate,
                args,
                default,
            } => {
                // A lookup consumes its key arguments (the wildcard
                // marks the extracted value, not a binding).
                self.check_predicate_ref(predicate.as_str(), args, RefMode::Use, scope);
                let result_kind = value_of_result_kind(predicate.as_str(), args, &self.predicates);
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
                        let context = self.context.clone();
                        self.errors.push(ValidationError::OperandKindMismatch {
                            operator: "value default",
                            expected,
                            actual,
                            context,
                        });
                    }
                }
                result_kind
            }
            ValueExpr::Abs(inner) => match self.infer_value(inner, scope) {
                // abs preserves the kind of a signed value; any other
                // known kind is an authoring-time error.
                InferredKind::Known(
                    k @ (PredicateArgKind::Decimal
                    | PredicateArgKind::Quantity(_)
                    | PredicateArgKind::Duration),
                ) => InferredKind::Known(k),
                InferredKind::Known(kind) => {
                    let context = self.context.clone();
                    self.errors.push(ValidationError::AbsKind { kind, context });
                    InferredKind::UnknownOrAny
                }
                InferredKind::UnknownOrAny => InferredKind::UnknownOrAny,
            },
            ValueExpr::Round { value, quantum } => {
                // The established operand path: refines a bare variable
                // to Decimal, accepts Any (unconstrained kinds refine at
                // a later concrete use), reports incompatible concrete
                // kinds as OperandKindMismatch.
                self.check_operand_kind(value, PredicateArgKind::Decimal, "round", scope);
                self.check_operand_kind(quantum, PredicateArgKind::Decimal, "round", scope);
                // A literal quantum must be positive; a variable quantum
                // is the runtime backstop's job.
                if let ValueExpr::Term(Term::Literal(Value::Decimal(s))) = quantum.as_ref()
                    && s.parse::<rust_decimal::Decimal>()
                        .is_ok_and(|d| d <= rust_decimal::Decimal::ZERO)
                {
                    let context = self.context.clone();
                    self.errors.push(ValidationError::RoundQuantumNotPositive {
                        quantum: s.clone(),
                        context,
                    });
                }
                InferredKind::Known(PredicateArgKind::Decimal)
            }
        }
    }

    /// A variable used where a bound value is required. Flags
    /// `UnboundVariable` if nothing has bound it at this point.
    fn use_var(&mut self, scope: &Scope, name: &Var) {
        if !scope.bound.is_bound(name) {
            let context = self.context.clone();
            self.errors.push(ValidationError::UnboundVariable {
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
            let context = self.context.clone();
            self.errors.push(ValidationError::Undeclared {
                vocabulary: VocabularyKind::Definition,
                name: name.into(),
                context,
            });
            return;
        };
        if def.parameters.len() != args.len() {
            let context = self.context.clone();
            self.errors.push(ValidationError::ArityMismatch {
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
                        let context = self.context.clone();
                        self.errors.push(ValidationError::UnboundVariable {
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
                        let context = self.context.clone();
                        self.errors.push(ValidationError::ArgKindMismatch {
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
            let context = self.context.clone();
            self.errors.push(ValidationError::DerivedInRule {
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
        };
        let Some(decl_args) = decl_args else {
            let context = self.context.clone();
            // A predicate-position reference that names a definition is
            // a category error with its own guidance (definitions are
            // proposition-valued; admit/retract/value need a claim),
            // not an undeclared name.
            if vocabulary == VocabularyKind::Predicate && self.definitions.contains_key(name) {
                self.errors.push(ValidationError::UnresolvedDefinitionCall {
                    name: name.into(),
                    context,
                });
            } else {
                self.errors.push(ValidationError::Undeclared {
                    vocabulary,
                    name: name.into(),
                    context,
                });
            }
            return;
        };
        if decl_args.len() != args.len() {
            let context = self.context.clone();
            self.errors.push(ValidationError::ArityMismatch {
                vocabulary,
                name: name.into(),
                expected: decl_args.len(),
                actual: args.len(),
                context,
            });
        }
        self.check_args(vocabulary, name, args, decl_args, mode, scope);
    }

    /// Generic arg-list check. A literal contributes its kind; a
    /// variable binds (`Match`) or is use-checked (`Use`) and its
    /// kind refines; `Wildcard` is skipped; `Actor` contributes
    /// `Subject`. Walks only `min(args, decl)`; arity is owned by
    /// `check_reference`.
    #[allow(clippy::too_many_arguments)]
    fn check_args(
        &mut self,
        vocabulary: VocabularyKind,
        name: &str,
        args: &[Term],
        decl_args: &[crate::ArgDecl],
        mode: RefMode,
        scope: &mut Scope,
    ) {
        let n = args.len().min(decl_args.len());
        for (position, (arg, decl_arg)) in args
            .iter()
            .take(n)
            .zip(decl_args.iter().take(n))
            .enumerate()
        {
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
            let context = self.context.clone();
            self.errors
                .push(ValidationError::CalendarSpanEscapesExpression {
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
                let context = self.context.clone();
                self.errors.push(ValidationError::VariableKindConflict {
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
            let context = self.context.clone();
            self.errors.push(ValidationError::ArgKindMismatch {
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
            let context = self.context.clone();
            self.errors.push(ValidationError::VariableKindConflict {
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

/// Look up the kind of the value position in a `ValueOf` lookup.
/// The first wildcard position in `args` marks the value slot;
/// returns its declared kind, or UnknownOrAny when the predicate
/// is undeclared or has no wildcard.
fn value_of_result_kind(
    predicate: &str,
    args: &[Term],
    predicates: &HashMap<&str, &PredicateDecl>,
) -> InferredKind {
    let Some(decl) = predicates.get(predicate) else {
        return InferredKind::UnknownOrAny;
    };
    args.iter()
        .position(|a| matches!(a, Term::Wildcard))
        .and_then(|p| decl.args.get(p))
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

/// Whether a proposition contains `Prop::Pre` anywhere in its tree.
/// Used to ban `pre(...)` inside definition bodies (bodies are
/// context-free; a call wrapped in `pre(...)` at the use site covers
/// the legitimate cases).
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

fn prop_mentions_pre(prop: &Prop) -> bool {
    fold::mentions_pre(prop)
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

/// The arithmetic rule matrix over known operand kinds. `None` means
/// no rule exists and authoring-time validation reports it. Mirrors
/// the evaluator's runtime matrix exactly; the time-values test suite
/// couples the two.
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
mod tests {
    use super::*;

    #[test]
    fn refine_keeps_the_more_specific_kind_or_reports_conflict() {
        use InferredKind::{Known, UnknownOrAny};
        use PredicateArgKind::{Any, Decimal, Subject};
        // `a.refine(b)`: the unknown/any side yields to the other, the
        // more specific kind wins (an `Any` slot refines to a later
        // concrete use), and an incompatible pair reports itself.
        let cases = [
            (UnknownOrAny, Known(Decimal), Ok(Known(Decimal))),
            (Known(Decimal), UnknownOrAny, Ok(Known(Decimal))),
            (Known(Any), Known(Decimal), Ok(Known(Decimal))),
            (Known(Decimal), Known(Any), Ok(Known(Decimal))),
            (Known(Decimal), Known(Subject), Err((Decimal, Subject))),
        ];
        for (a, b, expected) in cases {
            let desc = format!("{a:?}.refine({b:?})");
            assert_eq!(a.refine(b), expected, "{desc}");
        }
    }

    #[test]
    fn kindenv_observe_then_lookup_returns_refined_kind() {
        let mut env = KindEnv::default();
        env.observe(
            &Var::from("amount"),
            InferredKind::Known(PredicateArgKind::Decimal),
        )
        .expect("first observation always succeeds against UnknownOrAny");
        assert_eq!(
            env.lookup(&Var::from("amount")),
            InferredKind::Known(PredicateArgKind::Decimal)
        );
    }

    #[test]
    fn kindenv_observe_refines_through_any() {
        let mut env = KindEnv::default();
        env.observe(&Var::from("x"), InferredKind::Known(PredicateArgKind::Any))
            .unwrap();
        env.observe(
            &Var::from("x"),
            InferredKind::Known(PredicateArgKind::Decimal),
        )
        .unwrap();
        assert_eq!(
            env.lookup(&Var::from("x")),
            InferredKind::Known(PredicateArgKind::Decimal)
        );
    }

    #[test]
    fn kindenv_observe_reports_conflict_with_previous_kinds() {
        let mut env = KindEnv::default();
        env.observe(
            &Var::from("x"),
            InferredKind::Known(PredicateArgKind::Decimal),
        )
        .unwrap();
        let err = env
            .observe(
                &Var::from("x"),
                InferredKind::Known(PredicateArgKind::Subject),
            )
            .expect_err("conflict");
        assert_eq!(err, (PredicateArgKind::Decimal, PredicateArgKind::Subject));
    }

    // ============================================================
    // check_program: claim arg checking + statement flow
    // ============================================================

    use crate::ir::{ArgDecl, Program};
    use crate::ir_builder::*;

    /// Build a `PredicateDecl` shorthand for tests.
    fn pdecl(name: &str, args: &[(&str, PredicateArgKind)]) -> crate::ir::PredicateDecl {
        crate::ir::PredicateDecl {
            name: name.into(),
            disciplines: Vec::new(),
            args: args
                .iter()
                .map(|(n, k)| ArgDecl {
                    name: n.to_string(),
                    kind: k.clone(),
                })
                .collect(),
        }
    }

    fn empty_program() -> Program {
        program("test").build()
    }

    #[test]
    fn clean_programme_returns_no_kind_errors() {
        let mut p = empty_program();
        p.predicates = vec![pdecl(
            "Policy",
            &[
                ("policy_id", PredicateArgKind::Subject),
                ("limit", PredicateArgKind::Decimal),
            ],
        )];
        p.invariants = vec![invariant(
            "any_policy_has_positive_limit",
            claim("Policy", vec![var("p"), var("l")]),
        )];
        let errs = check_program(&p);
        assert!(
            errs.is_empty(),
            "clean programme should report no errors; got {errs:?}"
        );
    }

    #[test]
    fn decimal_literal_in_subject_slot_is_flagged() {
        let mut p = empty_program();
        p.predicates = vec![pdecl("Policy", &[("policy_id", PredicateArgKind::Subject)])];
        p.invariants = vec![invariant("bad", claim("Policy", vec![dec("123")]))];
        let errs = check_program(&p);
        assert_eq!(errs.len(), 1, "expected one kind error; got {errs:?}");
        match &errs[0] {
            ValidationError::ArgKindMismatch {
                vocabulary: VocabularyKind::Predicate,
                name,
                position,
                expected,
                actual,
                ..
            } => {
                assert_eq!(name, "Policy");
                assert_eq!(*position, 0);
                assert_eq!(*expected, PredicateArgKind::Subject);
                assert_eq!(*actual, PredicateArgKind::Decimal);
            }
            other => panic!("expected ArgKindMismatch, got {other:?}"),
        }
    }

    #[test]
    fn variable_kind_refined_across_claim_uses() {
        // Pattern: bind variable `x` from a Decimal-slot Claim,
        // then use it in another Decimal-slot Claim. Should pass.
        let mut p = empty_program();
        p.predicates = vec![
            pdecl("A", &[("v", PredicateArgKind::Decimal)]),
            pdecl("B", &[("v", PredicateArgKind::Decimal)]),
        ];
        p.invariants = vec![invariant(
            "refine",
            and(vec![claim("A", vec![var("x")]), claim("B", vec![var("x")])]),
        )];
        let errs = check_program(&p);
        assert!(
            errs.is_empty(),
            "consistent refinement should pass; got {errs:?}"
        );
    }

    #[test]
    fn variable_kind_conflict_across_claim_uses_is_flagged() {
        // Pattern: bind `x` from a Decimal slot, then use in a
        // Subject slot. Conflict.
        let mut p = empty_program();
        p.predicates = vec![
            pdecl("A", &[("v", PredicateArgKind::Decimal)]),
            pdecl("B", &[("v", PredicateArgKind::Subject)]),
        ];
        p.invariants = vec![invariant(
            "conflict",
            and(vec![claim("A", vec![var("x")]), claim("B", vec![var("x")])]),
        )];
        let errs = check_program(&p);
        assert_eq!(errs.len(), 1, "expected one conflict; got {errs:?}");
        match &errs[0] {
            ValidationError::VariableKindConflict {
                variable,
                previous,
                new,
                ..
            } => {
                assert_eq!(variable, "x");
                assert_eq!(*previous, PredicateArgKind::Decimal);
                assert_eq!(*new, PredicateArgKind::Subject);
            }
            other => panic!("expected VariableKindConflict, got {other:?}"),
        }
    }

    #[test]
    fn any_slot_observes_variable_without_constraining_it() {
        // `A` declares its slot as `Any`. Variable `x` should
        // not be pinned to Any; later use in a Decimal slot
        // should refine it cleanly.
        let mut p = empty_program();
        p.predicates = vec![
            pdecl("A", &[("v", PredicateArgKind::Any)]),
            pdecl("B", &[("v", PredicateArgKind::Decimal)]),
        ];
        p.invariants = vec![invariant(
            "refines_through_any",
            and(vec![claim("A", vec![var("x")]), claim("B", vec![var("x")])]),
        )];
        let errs = check_program(&p);
        assert!(
            errs.is_empty(),
            "Any-then-Decimal should refine; got {errs:?}"
        );
    }

    #[test]
    fn actor_term_carries_subject_kind() {
        // `actor` flowing into a Decimal slot is a kind mismatch.
        // Tested in a transformation body, where `actor` is
        // legitimately available - so the only error is the kind
        // mismatch, not the actor-not-available error an invariant
        // body would add.
        let mut p = empty_program();
        p.predicates = vec![pdecl("Limit", &[("amount", PredicateArgKind::Decimal)])];
        p.transformations = vec![transformation(
            "t",
            vec![],
            vec![assert_("Limit", vec![actor()])],
        )];
        let errs = check_program(&p);
        assert_eq!(
            errs.len(),
            1,
            "actor-in-decimal-slot must flag; got {errs:?}"
        );
        match &errs[0] {
            ValidationError::ArgKindMismatch {
                vocabulary: VocabularyKind::Predicate,
                expected,
                actual,
                ..
            } => {
                assert_eq!(*expected, PredicateArgKind::Decimal);
                assert_eq!(*actual, PredicateArgKind::Subject);
            }
            other => panic!("expected ArgKindMismatch, got {other:?}"),
        }
    }

    // ----- actor-in-wrong-context -----

    #[test]
    fn actor_in_invariant_body_flags_actor_not_available() {
        // `actor` in an invariant body has no proposing transition
        // in scope; the kernel raises UnboundActor at runtime, the
        // check flags it statically. The predicate slot is Subject
        // so no kind error muddies the result.
        let mut p = empty_program();
        p.predicates = vec![pdecl("Approver", &[("who", PredicateArgKind::Subject)])];
        p.invariants = vec![invariant(
            "mentions_actor",
            claim("Approver", vec![actor()]),
        )];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::ActorNotAvailable {
                    context: ValidationContext::Invariant { .. }
                }
            )),
            "actor in an invariant body must flag ActorNotAvailable; got {errs:?}"
        );
    }

    #[test]
    fn actor_in_derived_claim_value_flags_actor_not_available() {
        // `actor` in a derived-claim value expression - same
        // unavailability as an invariant body.
        let mut p = empty_program();
        p.predicates = vec![
            pdecl(
                "Row",
                &[
                    ("k", PredicateArgKind::Subject),
                    ("v", PredicateArgKind::Subject),
                ],
            ),
            pdecl("Src", &[("k", PredicateArgKind::Subject)]),
        ];
        p.derived_claims = vec![crate::ir::DerivedClaim {
            predicate: "Row".into(),
            keys: vec!["k".into()],
            values: vec![crate::ir::DerivedValue {
                name: "v".into(),
                expr: term(actor()),
            }],
            domain: claim("Src", vec![var("k")]),
        }];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::ActorNotAvailable {
                    context: ValidationContext::DerivedClaim { .. }
                }
            )),
            "actor in a derived-claim value must flag ActorNotAvailable; got {errs:?}"
        );
    }

    /// A derived claim is a read model. Every rule that names one is
    /// refused at authoring time, because the alternative is a design
    /// that type-checks and then fails against a live database - which is
    /// how a trial lost an hour to `bind` over a derived.
    ///
    /// Table-driven over every position a rule can name a predicate,
    /// because the first report was only about `bind` and the same
    /// deadness applies to all of them.
    #[test]
    fn no_rule_may_name_a_derived_claim() {
        let derived = |p: &mut crate::ir::Program| {
            p.predicates = vec![
                pdecl("Src", &[("k", PredicateArgKind::Subject)]),
                pdecl("Row", &[("k", PredicateArgKind::Subject)]),
                pdecl("Out", &[("k", PredicateArgKind::Subject)]),
            ];
            p.derived_claims = vec![crate::ir::DerivedClaim {
                predicate: "Row".into(),
                keys: vec!["k".into()],
                values: vec![],
                domain: claim("Src", vec![var("k")]),
            }];
        };

        // Each case names `Row` - the derived - somewhere a rule reads.
        let mut invariant_case = empty_program();
        derived(&mut invariant_case);
        invariant_case.invariants = vec![invariant("reads_derived", claim("Row", vec![var("k")]))];

        let mut bind_case = empty_program();
        derived(&mut bind_case);
        bind_case.transformations = vec![transformation(
            "binds_derived",
            params(&["k"]),
            vec![bind_one(claim("Row", vec![var("k")]))],
        )];

        let mut require_case = empty_program();
        derived(&mut require_case);
        require_case.transformations = vec![transformation(
            "requires_derived",
            params(&["k"]),
            vec![require(claim("Row", vec![var("k")]))],
        )];

        let mut admit_case = empty_program();
        derived(&mut admit_case);
        admit_case.transformations = vec![transformation(
            "admits_derived",
            params(&["k"]),
            vec![assert_("Row", vec![var("k")])],
        )];

        // Deriveds do not compose: a derived's domain is evaluated
        // against admitted claims too, so naming another derived there
        // is as dead as naming one from a transformation.
        let mut derived_case = empty_program();
        derived(&mut derived_case);
        derived_case.derived_claims.push(crate::ir::DerivedClaim {
            predicate: "Out".into(),
            keys: vec!["k".into()],
            values: vec![],
            domain: claim("Row", vec![var("k")]),
        });

        for (label, program) in [
            ("invariant", invariant_case),
            ("bind", bind_case),
            ("require", require_case),
            ("admit", admit_case),
            ("another derived's domain", derived_case),
        ] {
            let errs = check_program(&program);
            assert!(
                errs.iter()
                    .any(|e| matches!(e, ValidationError::DerivedInRule { .. })),
                "{label} over a derived must be refused; got {errs:?}"
            );
        }
    }

    /// A discipline is a promise about governed state, and a derived
    /// output is not governed state.
    ///
    /// Both clause shapes matter and they fail differently: `unique by`
    /// lowers to a generated invariant, so without this the author saw an
    /// error naming a rule they never wrote, while `append only` lowers
    /// to nothing at all and passed silently - publishing an
    /// append-only promise for a view whose generations refresh replaces
    /// wholesale.
    #[test]
    fn a_derived_output_cannot_carry_a_discipline() {
        for discipline in [
            crate::ir::Discipline::AppendOnly,
            crate::ir::Discipline::UniqueBy {
                fields: vec!["k".into()],
            },
        ] {
            let mut p = empty_program();
            let mut row = pdecl("Row", &[("k", PredicateArgKind::Subject)]);
            row.disciplines = vec![discipline.clone()];
            p.predicates = vec![pdecl("Src", &[("k", PredicateArgKind::Subject)]), row];
            p.derived_claims = vec![crate::ir::DerivedClaim {
                predicate: "Row".into(),
                keys: vec!["k".into()],
                values: vec![],
                domain: claim("Src", vec![var("k")]),
            }];
            let errs = check_program(&p);
            assert!(
                errs.iter()
                    .any(|e| matches!(e, ValidationError::DisciplineOnDerived { .. })),
                "{discipline:?} on a derived head must be refused; got {errs:?}"
            );
        }
    }

    /// The acceptance side of the same rule: a derived claim reading
    /// ordinary admitted claims is exactly what a derived is for, and
    /// tightening the check must not refuse it.
    #[test]
    fn a_derived_may_read_the_claims_it_is_computed_from() {
        let mut p = empty_program();
        p.predicates = vec![
            pdecl("Src", &[("k", PredicateArgKind::Subject)]),
            pdecl("Row", &[("k", PredicateArgKind::Subject)]),
        ];
        p.derived_claims = vec![crate::ir::DerivedClaim {
            predicate: "Row".into(),
            keys: vec!["k".into()],
            values: vec![],
            domain: claim("Src", vec![var("k")]),
        }];
        let errs = check_program(&p);
        assert!(
            !errs
                .iter()
                .any(|e| matches!(e, ValidationError::DerivedInRule { .. })),
            "a derived reading its own sources must pass; got {errs:?}"
        );
    }

    #[test]
    fn actor_in_transformation_body_is_allowed() {
        // `actor` resolves inside transformation bodies - no
        // ActorNotAvailable. Slot is Subject so no kind error either.
        let mut p = empty_program();
        p.predicates = vec![pdecl("Approver", &[("who", PredicateArgKind::Subject)])];
        p.transformations = vec![transformation(
            "t",
            vec![],
            vec![assert_("Approver", vec![actor()])],
        )];
        let errs = check_program(&p);
        assert!(
            !errs
                .iter()
                .any(|e| matches!(e, ValidationError::ActorNotAvailable { .. })),
            "actor in a transformation body must not flag; got {errs:?}"
        );
    }

    // ----- Statement flow (require / bind_one / let / assert) -----

    #[test]
    fn bind_one_extends_env_for_subsequent_statements() {
        // bind_one A(x) (x: Decimal); assert B(x) (B's slot: Decimal). Clean.
        let mut p = empty_program();
        p.predicates = vec![
            pdecl("A", &[("v", PredicateArgKind::Decimal)]),
            pdecl("B", &[("v", PredicateArgKind::Decimal)]),
        ];
        p.transformations = vec![transformation(
            "t",
            vec![],
            vec![
                bind_one(claim("A", vec![var("x")])),
                assert_("B", vec![var("x")]),
            ],
        )];
        let errs = check_program(&p);
        assert!(
            errs.is_empty(),
            "bind_one then matching assert should pass; got {errs:?}"
        );
    }

    #[test]
    fn bind_one_then_conflicting_assert_flags_variable_conflict() {
        // bind_one binds x: Decimal; then assert pushes x into Subject slot.
        let mut p = empty_program();
        p.predicates = vec![
            pdecl("A", &[("v", PredicateArgKind::Decimal)]),
            pdecl("B", &[("v", PredicateArgKind::Subject)]),
        ];
        p.transformations = vec![transformation(
            "t",
            vec![],
            vec![
                bind_one(claim("A", vec![var("x")])),
                assert_("B", vec![var("x")]),
            ],
        )];
        let errs = check_program(&p);
        assert_eq!(errs.len(), 1, "expected conflict; got {errs:?}");
        assert!(matches!(
            errs[0],
            ValidationError::VariableKindConflict {
                variable: ref v,
                previous: PredicateArgKind::Decimal,
                new: PredicateArgKind::Subject,
                ..
            } if v == "x"
        ));
    }

    #[test]
    fn require_does_not_export_bindings_to_subsequent_statements() {
        // The load-bearing unbound-variable case: `require A(x)`
        // matches and binds x WITHIN the require, but does not export
        // it. The
        // later `assert B(x)` therefore uses an unbound x and must
        // flag UnboundVariable - exactly the runtime UnboundVariable
        // the gate's non-export rule would produce.
        let mut p = empty_program();
        p.predicates = vec![
            pdecl("A", &[("v", PredicateArgKind::Decimal)]),
            pdecl("B", &[("v", PredicateArgKind::Subject)]),
        ];
        p.transformations = vec![transformation(
            "t",
            vec![],
            vec![
                require(claim("A", vec![var("x")])),
                assert_("B", vec![var("x")]),
            ],
        )];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::UnboundVariable { variable: v, .. } if v == "x"
            )),
            "require must not export x to the later assert; got {errs:?}"
        );
    }

    #[test]
    fn params_flow_to_admit_without_a_binding_statement() {
        // The positive complement to require-non-export: parameters
        // are bound at transformation entry, so an `admit` using them
        // directly - no intervening `bind`/`let` - is clean.
        let mut p = empty_program();
        p.predicates = vec![pdecl(
            "Payment",
            &[
                ("payer", PredicateArgKind::Subject),
                ("limit", PredicateArgKind::Decimal),
            ],
        )];
        p.transformations = vec![transformation(
            "t",
            params(&["p", "limit"]),
            vec![assert_("Payment", vec![var("p"), var("limit")])],
        )];
        let errs = check_program(&p);
        assert!(
            errs.is_empty(),
            "parameters are bound at entry; the admit is clean. got {errs:?}"
        );
    }

    // The branch-binding-export family: an invariant
    // `A(x, n) implies (B(x, m) <op> C(...)) and n <= m`. The
    // intersection rule exports `m` to the comparator only when EVERY
    // branch binds it; if one branch leaves it unbound the runtime may
    // carry that witness forward, so `m` must stay unbound. Checked
    // across both short-circuiting connectives and both binding shapes.
    #[derive(Clone, Copy)]
    enum BranchOp {
        Or,
        Xor,
    }

    fn branch_export_program(op: BranchOp, both_branches_bind_m: bool) -> Program {
        let mut p = empty_program();
        let (c_decl, c_args) = if both_branches_bind_m {
            (
                pdecl(
                    "C",
                    &[
                        ("x", PredicateArgKind::Subject),
                        ("m", PredicateArgKind::Decimal),
                    ],
                ),
                vec![var("x"), var("m")],
            )
        } else {
            (
                pdecl("C", &[("x", PredicateArgKind::Subject)]),
                vec![var("x")],
            )
        };
        p.predicates = vec![
            pdecl(
                "A",
                &[
                    ("x", PredicateArgKind::Subject),
                    ("n", PredicateArgKind::Decimal),
                ],
            ),
            pdecl(
                "B",
                &[
                    ("x", PredicateArgKind::Subject),
                    ("m", PredicateArgKind::Decimal),
                ],
            ),
            c_decl,
        ];
        let b = claim("B", vec![var("x"), var("m")]);
        let c = claim("C", c_args);
        let branches = match op {
            BranchOp::Or => or(vec![b, c]),
            BranchOp::Xor => xor(b, c),
        };
        p.invariants = vec![invariant(
            "inv",
            implies(
                claim("A", vec![var("x"), var("n")]),
                and(vec![branches, le(term(var("n")), term(var("m")))]),
            ),
        )];
        p
    }

    #[test]
    fn branch_binding_exports_only_when_every_branch_binds() {
        for (op, both_bind, expect_ok, label) in [
            (
                BranchOp::Or,
                true,
                true,
                "or: both branches bind m -> exports",
            ),
            (
                BranchOp::Or,
                false,
                false,
                "or: one branch binds m -> no export",
            ),
            (
                BranchOp::Xor,
                true,
                true,
                "xor: both operands bind m -> exports",
            ),
            (
                BranchOp::Xor,
                false,
                false,
                "xor: one operand binds m -> no export",
            ),
        ] {
            let errs = check_program(&branch_export_program(op, both_bind));
            if expect_ok {
                assert!(errs.is_empty(), "{label}: expected no errors; got {errs:?}");
            } else {
                assert!(
                    errs.iter().any(|e| matches!(
                        e,
                        ValidationError::UnboundVariable { variable: v, .. } if v == "m"
                    )),
                    "{label}: expected UnboundVariable(m); got {errs:?}"
                );
            }
        }
    }

    #[test]
    fn in_generator_binds_unbound_element_in_sum_body() {
        // The settlement shape: `sum(x | line in lines and P(line, x))`.
        // `line` is not pre-bound; `in` binds it to each item (it is a
        // generator, not a use), so `P(line, x)` matches cleanly.
        let mut p = empty_program();
        p.predicates = vec![pdecl(
            "P",
            &[
                ("line", PredicateArgKind::Subject),
                ("amount", PredicateArgKind::Decimal),
            ],
        )];
        p.transformations = vec![transformation(
            "t",
            params(&["lines"]),
            vec![let_(
                "total",
                sum(
                    var("x"),
                    and(vec![
                        in_(var("line"), var("lines")),
                        claim("P", vec![var("line"), var("x")]),
                    ]),
                ),
            )],
        )];
        let errs = check_program(&p);
        assert!(
            errs.is_empty(),
            "`in` binds the unbound element; the sum body is clean. got {errs:?}"
        );
    }

    #[test]
    fn let_new_subject_binds_name_as_subject() {
        // let_new_subject names a fresh subject; using it in a
        // Decimal slot must flag.
        let mut p = empty_program();
        p.predicates = vec![pdecl("Amt", &[("v", PredicateArgKind::Decimal)])];
        p.transformations = vec![transformation(
            "t",
            vec![],
            vec![let_new_subject("fresh"), assert_("Amt", vec![var("fresh")])],
        )];
        let errs = check_program(&p);
        assert_eq!(
            errs.len(),
            1,
            "subject-into-decimal must flag; got {errs:?}"
        );
        assert!(matches!(
            errs[0],
            ValidationError::VariableKindConflict {
                previous: PredicateArgKind::Subject,
                new: PredicateArgKind::Decimal,
                ..
            }
        ));
    }

    #[test]
    fn retract_args_are_kind_checked() {
        // Retract is the read side of the assert pair; same kind rules apply.
        let mut p = empty_program();
        p.predicates = vec![pdecl("P", &[("id", PredicateArgKind::Subject)])];
        p.transformations = vec![transformation(
            "t",
            vec![],
            vec![retract("P", vec![dec("99")])],
        )];
        let errs = check_program(&p);
        assert_eq!(errs.len(), 1);
        assert!(matches!(
            errs[0],
            ValidationError::ArgKindMismatch {
                vocabulary: VocabularyKind::Predicate,
                ..
            }
        ));
    }

    // ============================================================
    // Value-expression inference + comparators / arithmetic
    // ============================================================

    /// One mixed operand pair (a date and a decimal), each comparator:
    /// `<=` is the decimal comparator and flags the date; `on_or_before`
    /// is the date comparator and flags the decimal. The canonical
    /// "wrong comparator" mistake the kernel surfaces as TypeMismatch
    /// at runtime.
    #[test]
    fn comparator_operand_mismatches_flag_operator_and_kinds() {
        type Comparator = fn(ValueExpr, ValueExpr) -> Prop;
        let cases: [(Comparator, &str, PredicateArgKind, PredicateArgKind); 2] = [
            (le, "<=", PredicateArgKind::Decimal, PredicateArgKind::Date),
            (
                date_le,
                "on_or_before",
                PredicateArgKind::Date,
                PredicateArgKind::Decimal,
            ),
        ];
        for (comparator, want_op, want_expected, want_actual) in cases {
            let mut p = empty_program();
            p.invariants = vec![invariant(
                "bad_compare",
                comparator(term(date("2026-01-01")), term(dec("100"))),
            )];
            let errs = check_program(&p);
            assert_eq!(
                errs.len(),
                1,
                "{want_op}: expected one operand error; got {errs:?}"
            );
            match &errs[0] {
                ValidationError::OperandKindMismatch {
                    operator,
                    expected,
                    actual,
                    ..
                } => {
                    assert_eq!(*operator, want_op);
                    assert_eq!(*expected, want_expected);
                    assert_eq!(*actual, want_actual);
                }
                other => panic!("expected OperandKindMismatch, got {other:?}"),
            }
        }
    }

    #[test]
    fn abs_of_a_subject_flags_abs_kind() {
        // abs is defined on signed numeric kinds; a subject has no
        // magnitude.
        let mut p = empty_program();
        p.invariants = vec![invariant(
            "bad_abs",
            le(abs(term(subj("not_a_number"))), term(dec("100"))),
        )];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::AbsKind {
                    kind: PredicateArgKind::Subject,
                    ..
                }
            )),
            "expected AbsKind on a subject, got {errs:?}"
        );
    }

    #[test]
    fn abs_of_a_decimal_is_accepted() {
        let mut p = empty_program();
        p.invariants = vec![invariant(
            "ok_abs",
            le(abs(term(dec("10"))), term(dec("100"))),
        )];
        assert!(
            check_program(&p).is_empty(),
            "abs of a decimal should type-check"
        );
    }

    #[test]
    fn abs_refines_the_variable_it_wraps() {
        // `x` is used in a Subject slot and inside `abs(x) <= 10`. The
        // refinement reaches the variable through abs and pins it to
        // Decimal, conflicting with the Subject use - the conflict only
        // arises if abs's operand is refined (without it, x stays the
        // Subject the claim made it, and nothing pins it to Decimal).
        let mut p = empty_program();
        p.predicates = vec![pdecl("S", &[("v", PredicateArgKind::Subject)])];
        p.invariants = vec![invariant(
            "abs_refine",
            and(vec![
                claim("S", vec![var("x")]),
                le(abs(term(var("x"))), term(dec("10"))),
            ]),
        )];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::VariableKindConflict { variable, .. } if variable == "x"
            )),
            "abs(x) should refine x to Decimal and conflict with the Subject use: {errs:?}"
        );
    }

    #[test]
    fn add_with_subject_literal_operand_flags_no_arith_rule() {
        // Arithmetic on a subject literal is the unambiguous bug. With
        // the time kinds in the matrix, the report names both operand
        // kinds rather than assuming Decimal was intended.
        let mut p = empty_program();
        p.invariants = vec![invariant(
            "bad_add",
            le(
                add(term(dec("10")), term(subj("not_a_number"))),
                term(dec("100")),
            ),
        )];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::NoArithRule {
                    operator: "+",
                    left: PredicateArgKind::Decimal,
                    right: PredicateArgKind::Subject,
                    ..
                }
            )),
            "expected NoArithRule on `+`, got {errs:?}"
        );
    }

    #[test]
    fn comparator_refines_variable_for_subsequent_uses() {
        // `require A(x) and x <= 100` should refine `x` from
        // unconstrained (via A's Any slot) to Decimal. A later use
        // of `x` in a Subject slot must conflict.
        let mut p = empty_program();
        p.predicates = vec![
            pdecl("A", &[("v", PredicateArgKind::Any)]),
            pdecl("B", &[("v", PredicateArgKind::Subject)]),
        ];
        p.invariants = vec![invariant(
            "refine_via_le",
            and(vec![
                claim("A", vec![var("x")]),
                le(term(var("x")), term(dec("100"))),
                claim("B", vec![var("x")]),
            ]),
        )];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::VariableKindConflict {
                    variable: v,
                    previous: PredicateArgKind::Decimal,
                    new: PredicateArgKind::Subject,
                    ..
                } if v == "x"
            )),
            "expected x to refine to Decimal via Le then conflict on Subject use; got {errs:?}"
        );
    }

    #[test]
    fn strict_equality_flags_distinct_operand_kinds() {
        // Eq and Neq are strict: Decimal vs Subject must surface as a
        // kind mismatch, not be silently coerced.
        for (name, body, operator) in [
            ("bad_eq", eq(term(dec("100")), term(subj("S"))), "="),
            ("bad_neq", neq(dec("100"), subj("S")), "!="),
        ] {
            let mut p = empty_program();
            p.invariants = vec![invariant(name, body)];
            let errs = check_program(&p);
            assert_eq!(errs.len(), 1, "{operator}: {errs:?}");
            assert!(
                matches!(
                    errs[0],
                    ValidationError::EqualityKindMismatch { operator: op, .. } if op == operator
                ),
                "{operator}: got {errs:?}"
            );
        }
    }

    #[test]
    fn eq_refines_variable_to_concrete_kind_for_subsequent_uses() {
        // `x == 100` against an otherwise unconstrained `x` should
        // pin `x` to Decimal; a later Subject-slot use conflicts.
        let mut p = empty_program();
        p.predicates = vec![pdecl("B", &[("v", PredicateArgKind::Subject)])];
        p.invariants = vec![invariant(
            "refine_via_eq",
            and(vec![
                eq(term(var("x")), term(dec("100"))),
                claim("B", vec![var("x")]),
            ]),
        )];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::VariableKindConflict {
                    previous: PredicateArgKind::Decimal,
                    new: PredicateArgKind::Subject,
                    ..
                }
            )),
            "expected refinement via Eq then conflict; got {errs:?}"
        );
    }

    #[test]
    fn let_binds_name_at_inferred_value_kind() {
        // `let y = x - 1` where `x` was already Decimal binds `y`
        // as Decimal; using `y` in a Subject slot must conflict.
        let mut p = empty_program();
        p.predicates = vec![
            pdecl("A", &[("v", PredicateArgKind::Decimal)]),
            pdecl("S", &[("id", PredicateArgKind::Subject)]),
        ];
        p.transformations = vec![transformation(
            "t",
            vec![],
            vec![
                bind_one(claim("A", vec![var("x")])),
                let_("y", sub(term(var("x")), term(dec("1")))),
                assert_("S", vec![var("y")]),
            ],
        )];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::VariableKindConflict {
                    variable: v,
                    previous: PredicateArgKind::Decimal,
                    new: PredicateArgKind::Subject,
                    ..
                } if v == "y"
            )),
            "expected y to inherit Decimal from let-expression; got {errs:?}"
        );
    }

    #[test]
    fn comparator_clean_when_operands_match_expected_kind() {
        // The happy path: amount <= limit, both bound from
        // Decimal slots. No errors expected.
        let mut p = empty_program();
        p.predicates = vec![
            pdecl("A", &[("v", PredicateArgKind::Decimal)]),
            pdecl("L", &[("v", PredicateArgKind::Decimal)]),
        ];
        p.invariants = vec![invariant(
            "ok",
            and(vec![
                claim("A", vec![var("amount")]),
                claim("L", vec![var("limit")]),
                le(term(var("amount")), term(var("limit"))),
            ]),
        )];
        let errs = check_program(&p);
        assert!(errs.is_empty(), "happy path should pass; got {errs:?}");
    }

    // ============================================================
    // Sum + ValueOf
    // ============================================================

    #[test]
    fn sum_with_body_refined_value_term_passes() {
        // The canonical aggregation shape: `sum(amount | P(_, amount))`
        // where P's value slot is Decimal. Sum's body refines
        // `amount` to Decimal; the value term resolves to Decimal;
        // Sum is happy.
        let mut p = empty_program();
        p.predicates = vec![pdecl(
            "Payment",
            &[
                ("policy", PredicateArgKind::Subject),
                ("amount", PredicateArgKind::Decimal),
            ],
        )];
        p.invariants = vec![invariant(
            "ok_sum",
            le(
                sum(
                    var("amount"),
                    claim("Payment", vec![wildcard(), var("amount")]),
                ),
                term(dec("1000")),
            ),
        )];
        let errs = check_program(&p);
        assert!(errs.is_empty(), "well-typed Sum should pass; got {errs:?}");
    }

    #[test]
    fn sum_with_subject_value_term_flags_operand_mismatch() {
        // `sum(p | Payment(p, _))` - the value term is a Subject
        // (refined from P's first slot), but Sum demands Decimal.
        let mut p = empty_program();
        p.predicates = vec![pdecl(
            "Payment",
            &[
                ("policy", PredicateArgKind::Subject),
                ("amount", PredicateArgKind::Decimal),
            ],
        )];
        p.invariants = vec![invariant(
            "bad_sum",
            le(
                sum(var("p"), claim("Payment", vec![var("p"), wildcard()])),
                term(dec("1000")),
            ),
        )];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::OperandKindMismatch {
                    operator: "sum",
                    expected: PredicateArgKind::Decimal,
                    actual: PredicateArgKind::Subject,
                    ..
                }
            )),
            "expected sum's value term to flag Subject vs Decimal; got {errs:?}"
        );
    }

    #[test]
    fn sum_with_date_literal_value_term_flags_operand_mismatch() {
        // A literal in the value position that is not Decimal.
        let mut p = empty_program();
        p.predicates = vec![pdecl("X", &[("v", PredicateArgKind::Subject)])];
        p.invariants = vec![invariant(
            "bad_sum_lit",
            le(
                sum(date("2026-01-01"), claim("X", vec![var("x")])),
                term(dec("100")),
            ),
        )];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::OperandKindMismatch {
                    operator: "sum",
                    expected: PredicateArgKind::Decimal,
                    actual: PredicateArgKind::Date,
                    ..
                }
            )),
            "expected sum's date literal to flag; got {errs:?}"
        );
    }

    #[test]
    fn sum_body_bindings_do_not_leak_to_surrounding_env() {
        // `bind_one Q(x); require x <= sum(amount | P(_, amount))`
        // - the outer x is Decimal (Q's slot). The Sum's body
        // binds an inner `amount` at Decimal; after the Sum, the
        // outer env should still see `amount` as unconstrained.
        let mut p = empty_program();
        p.predicates = vec![
            pdecl("Q", &[("v", PredicateArgKind::Decimal)]),
            pdecl(
                "P",
                &[
                    ("policy", PredicateArgKind::Subject),
                    ("amount", PredicateArgKind::Decimal),
                ],
            ),
            pdecl("S", &[("id", PredicateArgKind::Subject)]),
        ];
        p.transformations = vec![transformation(
            "t",
            vec![],
            vec![
                bind_one(claim("Q", vec![var("x")])),
                require(le(
                    term(var("x")),
                    sum(var("amount"), claim("P", vec![wildcard(), var("amount")])),
                )),
                // The Sum bound `amount` only inside its body. At
                // this assert `amount` is unbound again, so it must
                // flag UnboundVariable - which is precisely the
                // non-leak property: the sum binding did not escape.
                assert_("S", vec![var("amount")]),
            ],
        )];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::UnboundVariable { variable: v, .. } if v == "amount"
            )),
            "Sum's `amount` binding must not leak to the later assert; got {errs:?}"
        );
    }

    #[test]
    fn value_of_resolves_to_wildcard_slot_kind() {
        // `value Policy(p, _)` resolves to Policy's second slot
        // (Decimal); against `<= 100` (Decimal) there is no kind
        // error. `p` is a transformation parameter so it is bound
        // for the lookup key (an invariant would leave it unbound).
        let mut p = empty_program();
        p.predicates = vec![pdecl(
            "Policy",
            &[
                ("policy", PredicateArgKind::Subject),
                ("limit", PredicateArgKind::Decimal),
            ],
        )];
        p.transformations = vec![transformation(
            "t",
            vec!["p".into()],
            vec![require(le(
                value_of("Policy", vec![var("p"), wildcard()]),
                term(dec("100")),
            ))],
        )];
        let errs = check_program(&p);
        assert!(
            errs.is_empty(),
            "ValueOf at decimal slot should be Decimal; got {errs:?}"
        );
    }

    #[test]
    fn value_of_with_subject_slot_in_comparator_flags_operand_mismatch() {
        // `value Owner(p, _) <= 100` - wildcard is Owner's
        // Subject slot; Le's LHS is Subject, not Decimal.
        let mut p = empty_program();
        p.predicates = vec![pdecl(
            "Owner",
            &[
                ("policy", PredicateArgKind::Subject),
                ("owner", PredicateArgKind::Subject),
            ],
        )];
        p.invariants = vec![invariant(
            "bad_value_of",
            le(
                value_of("Owner", vec![var("p"), wildcard()]),
                term(dec("100")),
            ),
        )];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::OperandKindMismatch {
                    operator: "<=",
                    expected: PredicateArgKind::Decimal,
                    actual: PredicateArgKind::Subject,
                    ..
                }
            )),
            "expected Le LHS to flag Subject vs Decimal; got {errs:?}"
        );
    }

    // ============================================================
    // Derived-claim output args vs declared kinds
    // ============================================================

    use crate::ir::{DerivedClaim, DerivedValue};

    #[test]
    fn derived_claim_key_var_with_wrong_kind_flags_predicate_arg_kind_mismatch() {
        // Out predicate Row(account: Subject, ...); `over P(account)`
        // where P binds account at Decimal. Output position 0
        // expects Subject; actual is Decimal.
        let mut p = empty_program();
        p.predicates = vec![
            pdecl(
                "Row",
                &[
                    ("account", PredicateArgKind::Subject),
                    ("balance", PredicateArgKind::Decimal),
                ],
            ),
            pdecl("P", &[("v", PredicateArgKind::Decimal)]),
        ];
        p.derived_claims = vec![DerivedClaim {
            predicate: "Row".into(),
            keys: vec!["account".into()],
            values: vec![DerivedValue {
                name: "balance".into(),
                expr: term(dec("0")),
            }],
            domain: claim("P", vec![var("account")]),
        }];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::ArgKindMismatch {
                    vocabulary: VocabularyKind::Predicate,
                    name: pn,
                    position: 0,
                    expected: PredicateArgKind::Subject,
                    actual: PredicateArgKind::Decimal,
                    ..
                } if pn == "Row"
            )),
            "derived key vs declared kind mismatch must flag; got {errs:?}"
        );
    }

    #[test]
    fn derived_claim_value_expr_with_wrong_kind_flags_predicate_arg_kind_mismatch() {
        // Out predicate Row(account: Subject, count: Subject);
        // value expr returns Decimal. Position 1 mismatch.
        let mut p = empty_program();
        p.predicates = vec![
            pdecl(
                "Row",
                &[
                    ("account", PredicateArgKind::Subject),
                    ("count", PredicateArgKind::Subject),
                ],
            ),
            pdecl(
                "P",
                &[
                    ("acct", PredicateArgKind::Subject),
                    ("amt", PredicateArgKind::Decimal),
                ],
            ),
        ];
        p.derived_claims = vec![DerivedClaim {
            predicate: "Row".into(),
            keys: vec!["account".into()],
            values: vec![DerivedValue {
                name: "count".into(),
                expr: sum(var("amt"), claim("P", vec![var("account"), var("amt")])),
            }],
            domain: claim("P", vec![var("account"), wildcard()]),
        }];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::ArgKindMismatch {
                    vocabulary: VocabularyKind::Predicate,
                    name: pn,
                    position: 1,
                    expected: PredicateArgKind::Subject,
                    actual: PredicateArgKind::Decimal,
                    ..
                } if pn == "Row"
            )),
            "derived value vs declared kind mismatch must flag; got {errs:?}"
        );
    }

    #[test]
    fn derived_claim_clean_when_keys_and_values_match_declared_kinds() {
        // Mirror of the TrialBalanceRow shape in the ledger example.
        let mut p = empty_program();
        p.predicates = vec![
            pdecl(
                "Row",
                &[
                    ("account", PredicateArgKind::Subject),
                    ("balance", PredicateArgKind::Decimal),
                ],
            ),
            pdecl(
                "Line",
                &[
                    ("account", PredicateArgKind::Subject),
                    ("amount", PredicateArgKind::Decimal),
                ],
            ),
        ];
        p.derived_claims = vec![DerivedClaim {
            predicate: "Row".into(),
            keys: vec!["account".into()],
            values: vec![DerivedValue {
                name: "balance".into(),
                expr: sum(var("amt"), claim("Line", vec![var("account"), var("amt")])),
            }],
            domain: claim("Line", vec![var("account"), wildcard()]),
        }];
        let errs = check_program(&p);
        assert!(
            errs.is_empty(),
            "well-typed derived claim should pass; got {errs:?}"
        );
    }

    // ============================================================
    // For collection + In non-Collection literal
    // ============================================================

    #[test]
    fn for_with_non_collection_variable_flags_operand_mismatch() {
        // `bind_one Q(x); for x in ...` - x is bound at Decimal
        // (Q's slot). The `for` collection slot demands Collection.
        let mut p = empty_program();
        p.predicates = vec![pdecl("Q", &[("v", PredicateArgKind::Decimal)])];
        p.transformations = vec![transformation(
            "t",
            vec![],
            vec![
                bind_one(claim("Q", vec![var("x")])),
                for_("e", term(var("x")), vec![]),
            ],
        )];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::VariableKindConflict {
                    variable: v,
                    previous: PredicateArgKind::Decimal,
                    new: PredicateArgKind::Collection,
                    ..
                } if v == "x"
            )),
            "for on a Decimal variable must flag conflict; got {errs:?}"
        );
    }

    #[test]
    fn for_collection_variable_refines_to_collection() {
        // `for e in xs: assert P(xs)` where P expects Decimal
        // should conflict on `xs` - the for refined xs to
        // Collection, then assert tries Decimal.
        let mut p = empty_program();
        p.predicates = vec![pdecl("P", &[("v", PredicateArgKind::Decimal)])];
        p.transformations = vec![transformation(
            "t",
            vec!["xs".into()],
            vec![
                for_("e", term(var("xs")), vec![]),
                assert_("P", vec![var("xs")]),
            ],
        )];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::VariableKindConflict {
                    variable: v,
                    previous: PredicateArgKind::Collection,
                    new: PredicateArgKind::Decimal,
                    ..
                } if v == "xs"
            )),
            "for must refine xs to Collection; got {errs:?}"
        );
    }

    #[test]
    fn in_with_non_collection_literal_flags_operand_mismatch() {
        // `x in 100` - the collection side is a decimal literal,
        // which runtime would reject as "In expects a collection".
        let mut p = empty_program();
        p.invariants = vec![invariant(
            "in_lit",
            Prop::In(Term::Var("x".into()), dec("100")),
        )];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::OperandKindMismatch {
                    operator: "in",
                    expected: PredicateArgKind::Collection,
                    actual: PredicateArgKind::Decimal,
                    ..
                }
            )),
            "non-collection literal in `in` must flag; got {errs:?}"
        );
    }

    // ============================================================
    // Intent emit arg-kind checking
    // ============================================================

    use crate::IntentDecl;

    fn intent(name: &str, args: &[(&str, PredicateArgKind)]) -> IntentDecl {
        IntentDecl {
            name: name.into(),
            args: args
                .iter()
                .map(|(n, k)| ArgDecl {
                    name: n.to_string(),
                    kind: k.clone(),
                })
                .collect(),
        }
    }

    #[test]
    fn emit_with_literal_in_wrong_kind_slot_flags_arg_kind_mismatch() {
        // `emit X(100)` against `intent X(id: Subject)` - decimal
        // literal in a Subject slot. Same shape of error as the
        // predicate-side `Assert` case, but tagged Intent.
        let mut p = empty_program();
        p.intents = vec![intent("Notify", &[("id", PredicateArgKind::Subject)])];
        p.transformations = vec![transformation(
            "t",
            vec![],
            vec![emit("Notify", vec![dec("100")])],
        )];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::ArgKindMismatch {
                    vocabulary: VocabularyKind::Intent,
                    name,
                    position: 0,
                    expected: PredicateArgKind::Subject,
                    actual: PredicateArgKind::Decimal,
                    ..
                } if name == "Notify"
            )),
            "expected Intent ArgKindMismatch; got {errs:?}"
        );
    }

    #[test]
    fn emit_variable_observed_against_declared_intent_arg_kind() {
        // `bind_one P(x); emit Notify(x)` where P binds x:Decimal
        // and Notify expects x:Subject. The conflict surfaces via
        // VariableKindConflict (variable already had Decimal kind).
        let mut p = empty_program();
        p.predicates = vec![pdecl("P", &[("v", PredicateArgKind::Decimal)])];
        p.intents = vec![intent("Notify", &[("v", PredicateArgKind::Subject)])];
        p.transformations = vec![transformation(
            "t",
            vec![],
            vec![
                bind_one(claim("P", vec![var("x")])),
                emit("Notify", vec![var("x")]),
            ],
        )];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::VariableKindConflict {
                    variable: v,
                    previous: PredicateArgKind::Decimal,
                    new: PredicateArgKind::Subject,
                    ..
                } if v == "x"
            )),
            "expected VariableKindConflict on x; got {errs:?}"
        );
    }

    #[test]
    fn emit_with_arg_kinds_matching_declared_intent_is_clean() {
        // Happy path: emit args agree with intent decl.
        let mut p = empty_program();
        p.predicates = vec![pdecl("P", &[("v", PredicateArgKind::Subject)])];
        p.intents = vec![intent(
            "Notify",
            &[
                ("subject", PredicateArgKind::Subject),
                ("count", PredicateArgKind::Decimal),
            ],
        )];
        p.transformations = vec![transformation(
            "t",
            vec![],
            vec![
                bind_one(claim("P", vec![var("x")])),
                emit("Notify", vec![var("x"), dec("5")]),
            ],
        )];
        let errs = check_program(&p);
        assert!(errs.is_empty(), "well-typed emit should pass; got {errs:?}");
    }

    // ============================================================
    // Or branch independence
    // ============================================================
    //
    // `Or` evaluates each branch against the same base context
    // and concatenates the results. The check mirrors this:
    // each branch sees the env at the call site; refinements
    // inside one branch are not visible to other branches and do
    // not leak out of the `Or`.

    #[test]
    fn or_branches_with_disjoint_kind_constraints_do_not_conflict() {
        // `A(x) or B(x)` with A:Decimal, B:Subject - each branch
        // observes x at its own kind independently. Conjunctive
        // logic would flag a conflict; disjunctive logic must not.
        let mut p = empty_program();
        p.predicates = vec![
            pdecl("A", &[("v", PredicateArgKind::Decimal)]),
            pdecl("B", &[("v", PredicateArgKind::Subject)]),
        ];
        p.invariants = vec![invariant(
            "or_disjoint",
            or(vec![claim("A", vec![var("x")]), claim("B", vec![var("x")])]),
        )];
        let errs = check_program(&p);
        assert!(
            errs.is_empty(),
            "Or branches must check independently; got {errs:?}"
        );
    }

    #[test]
    fn or_branch_refinements_do_not_leak_after_or() {
        // `(A(x) or B(x)) and C(x)` - A refines x to Decimal in
        // one branch, B to Subject in the other; the trailing
        // `C(x)` (Subject) must NOT conflict because Or did not
        // export either branch's refinement. (Conservative v0:
        // no per-variable intersection across branches.)
        let mut p = empty_program();
        p.predicates = vec![
            pdecl("A", &[("v", PredicateArgKind::Decimal)]),
            pdecl("B", &[("v", PredicateArgKind::Subject)]),
            pdecl("C", &[("v", PredicateArgKind::Subject)]),
        ];
        p.invariants = vec![invariant(
            "or_no_leak",
            and(vec![
                or(vec![claim("A", vec![var("x")]), claim("B", vec![var("x")])]),
                claim("C", vec![var("x")]),
            ]),
        )];
        let errs = check_program(&p);
        assert!(
            errs.is_empty(),
            "Or branch refinements must not leak; got {errs:?}"
        );
    }

    #[test]
    fn or_still_walks_branches_for_in_branch_kind_errors() {
        // A literal-vs-slot mismatch inside a branch must still
        // surface even though branches are independent of each
        // other. Pin the regression: independence is per-variable,
        // not per-error-emission.
        let mut p = empty_program();
        p.predicates = vec![pdecl("A", &[("v", PredicateArgKind::Subject)])];
        p.invariants = vec![invariant(
            "or_inner_error",
            or(vec![claim("A", vec![dec("100")])]),
        )];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::ArgKindMismatch {
                    vocabulary: VocabularyKind::Predicate,
                    expected: PredicateArgKind::Subject,
                    actual: PredicateArgKind::Decimal,
                    ..
                }
            )),
            "in-branch literal mismatch must still surface; got {errs:?}"
        );
    }

    // ============================================================
    // Quantifier bindings unify with outer (no shadowing)
    // ============================================================
    //
    // The runtime evaluator (`find_matches`) does not shadow
    // quantifier bindings - `unify_args` treats existing bindings
    // as constraints. An outer `x` reused as a forall / exists /
    // sum binding constrains the source/body rather than being
    // shadowed; a kind mismatch between the outer and inner uses
    // is what the runtime would surface as a unification failure,
    // so the check flags it as `VariableKindConflict`.

    #[test]
    fn forall_with_kind_conflicting_outer_variable_flags_conflict() {
        let mut p = empty_program();
        p.predicates = vec![
            pdecl("S", &[("v", PredicateArgKind::Subject)]),
            pdecl("P", &[("v", PredicateArgKind::Decimal)]),
            pdecl("C", &[("v", PredicateArgKind::Decimal)]),
        ];
        p.invariants = vec![invariant(
            "forall_unify",
            and(vec![
                claim("S", vec![var("x")]),
                forall("x", claim("P", vec![var("x")]), claim("C", vec![var("x")])),
            ]),
        )];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::VariableKindConflict {
                    variable: v,
                    previous: PredicateArgKind::Subject,
                    new: PredicateArgKind::Decimal,
                    ..
                } if v == "x"
            )),
            "outer x:Subject must conflict with forall source P(x:Decimal); got {errs:?}"
        );
    }

    #[test]
    fn exists_with_kind_conflicting_outer_variable_flags_conflict() {
        let mut p = empty_program();
        p.predicates = vec![
            pdecl("S", &[("v", PredicateArgKind::Subject)]),
            pdecl("D", &[("v", PredicateArgKind::Decimal)]),
        ];
        p.invariants = vec![invariant(
            "exists_unify",
            and(vec![
                claim("S", vec![var("x")]),
                exists("x", claim("D", vec![var("x")])),
            ]),
        )];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::VariableKindConflict {
                    variable: v,
                    previous: PredicateArgKind::Subject,
                    new: PredicateArgKind::Decimal,
                    ..
                } if v == "x"
            )),
            "outer x:Subject must conflict with exists body D(x:Decimal); got {errs:?}"
        );
    }

    #[test]
    fn value_of_default_kind_mismatch_flags_operand_mismatch() {
        // ValueOf's default must match the wildcard slot's kind.
        // Here the slot is Decimal but the default is a Subject -
        // the runtime would return either, so the caller cannot
        // safely consume the result.
        let mut p = empty_program();
        p.predicates = vec![pdecl(
            "Policy",
            &[
                ("policy", PredicateArgKind::Subject),
                ("limit", PredicateArgKind::Decimal),
            ],
        )];
        p.invariants = vec![invariant(
            "bad_default",
            le(
                value_of_with_default(
                    "Policy",
                    vec![var("p"), wildcard()],
                    term(subj("UNLIMITED")),
                ),
                term(dec("100")),
            ),
        )];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::OperandKindMismatch {
                    operator: "value default",
                    expected: PredicateArgKind::Decimal,
                    actual: PredicateArgKind::Subject,
                    ..
                }
            )),
            "expected value-default mismatch; got {errs:?}"
        );
    }
}
