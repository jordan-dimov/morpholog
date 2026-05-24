//! Kind/type compatibility check. Walks every expression in every
//! invariant, transformation, and derived claim; emits the kind
//! errors the runtime would otherwise raise as
//! `EvalError::TypeMismatch`. Predicate declarations carry the
//! expected kind per arg position; comparators, arithmetic, and
//! aggregators have fixed expected kinds; variables are inferred
//! and refined.
//!
//! `Any` is unconstrained, not a kind-eraser: a variable seen
//! first through an `Any` slot stays open and refines to a
//! specific kind on its next concrete use.
//!
//! Diagnostics ship without source spans in v0; the IR drops
//! parser spans on lowering.

use std::collections::HashMap;

use crate::ir::{Expr, PredicateArgKind, PredicateDecl, Program, Stmt, Term, Value};
use crate::validate::{ValidationContext, ValidationError, VocabularyKind};

/// Inferred kind of a value during static analysis. Distinct from
/// [`PredicateArgKind`] (which is the *declared* kind on a predicate
/// position) because variables can be observed-but-not-yet-pinned -
/// the `UnknownOrAny` state. A variable seen only through an `Any`
/// slot stays unconstrained and refines to a specific kind when
/// later observed in a specific slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
                if kinds_compatible(prev, new) {
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
fn kinds_compatible(a: PredicateArgKind, b: PredicateArgKind) -> bool {
    a == PredicateArgKind::Any || b == PredicateArgKind::Any || a == b
}

/// Scope-local map from variable name to inferred kind. Mutable
/// during expression and statement walks; passed by `&mut` through
/// the recursive checker. Distinct kind environments live per
/// invariant body, per derived-claim body, per transformation
/// (extended statement-by-statement following the runtime quartet
/// doctrine).
#[derive(Debug, Default, Clone)]
pub(crate) struct KindEnv {
    bindings: HashMap<String, InferredKind>,
}

impl KindEnv {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Look up a variable's current inferred kind. Returns
    /// `UnknownOrAny` for variables never observed before - that
    /// matches how an unconstrained slot would treat them.
    pub(crate) fn lookup(&self, name: &str) -> InferredKind {
        self.bindings
            .get(name)
            .copied()
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
        name: &str,
        observed: InferredKind,
    ) -> Result<(), (PredicateArgKind, PredicateArgKind)> {
        let existing = self.lookup(name);
        let refined = existing.refine(observed)?;
        self.bindings.insert(name.to_string(), refined);
        Ok(())
    }
}

/// The static-check visitor. Holds the programme's declared
/// vocabularies, the current `ValidationContext`, and the
/// accumulating error list. The per-walk kind environment is
/// passed separately (not held on the struct) because it is
/// cloned at scope boundaries - `require`, `sum`, `for`, and
/// `or`-branches each walk a clone whose refinements do not leak
/// back.
///
/// Shaped so a bound-variable environment can join the per-walk
/// state later without disturbing this struct: today the only
/// per-scope state threaded through the methods is the `KindEnv`;
/// unbound-variable detection will pair it with a bound-set in a
/// `Scope` when that behaviour lands.
struct CheckCtx<'a> {
    predicates: HashMap<&'a str, &'a PredicateDecl>,
    intents: HashMap<&'a str, &'a crate::IntentDecl>,
    context: ValidationContext,
    errors: Vec<ValidationError>,
}

/// Run the static checks over the whole programme. Returns the
/// full list of detected problems; an empty `Vec` means the
/// programme passes. Traversal order is invariants, then
/// transformations, then derived claims, so merged diagnostics
/// come out in a predictable shape.
pub(crate) fn check_program(program: &Program) -> Vec<ValidationError> {
    let mut cx = CheckCtx {
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
        // Reassigned per top-level item below; this placeholder is
        // never the context of an emitted error.
        context: ValidationContext::Invariant {
            name: String::new(),
        },
        errors: Vec::new(),
    };

    for inv in &program.invariants {
        cx.context = ValidationContext::Invariant {
            name: inv.name.clone(),
        };
        if expr_mentions_actor(&inv.body) {
            let context = cx.context.clone();
            cx.errors
                .push(ValidationError::ActorNotAvailable { context });
        }
        let mut env = KindEnv::new();
        cx.walk_predicate_expr(&inv.body, &mut env);
    }

    for transformation in &program.transformations {
        cx.context = ValidationContext::Transformation {
            name: transformation.name.clone(),
        };
        let mut env = KindEnv::new();
        // Parameters arrive untyped; observe each at UnknownOrAny
        // so the statement walk can refine them on use. The first
        // observation against UnknownOrAny never conflicts.
        for param in &transformation.parameters {
            let _ = env.observe(param, InferredKind::UnknownOrAny);
        }
        for stmt in &transformation.body {
            cx.walk_stmt(stmt, &mut env);
        }
    }

    for derived in &program.derived_claims {
        cx.context = ValidationContext::DerivedClaim {
            predicate: derived.predicate.clone(),
        };
        if expr_mentions_actor(&derived.domain)
            || derived.values.iter().any(|v| expr_mentions_actor(&v.expr))
        {
            let context = cx.context.clone();
            cx.errors
                .push(ValidationError::ActorNotAvailable { context });
        }
        let mut env = KindEnv::new();
        cx.walk_predicate_expr(&derived.domain, &mut env);
        // Each `value <name> = <expr>` is value-producing; infer
        // under the env built by `domain`. Keep the inferred kind
        // for the output-arg check below.
        let value_kinds: Vec<InferredKind> = derived
            .values
            .iter()
            .map(|v| cx.infer_value_expr(&v.expr, &mut env))
            .collect();

        // Output args check: the runtime emits claims of the form
        // `predicate(key_0, ..., key_K-1, value_0, ..., value_V-1)`.
        // The output predicate must be declared, its arity must
        // equal keys+values, and each position must match the
        // declared kind.
        let Some(decl) = cx.predicates.get(derived.predicate.as_str()).copied() else {
            let context = cx.context.clone();
            cx.errors.push(ValidationError::Undeclared {
                vocabulary: VocabularyKind::Predicate,
                name: derived.predicate.clone(),
                context,
            });
            continue;
        };
        let output_arity = derived.keys.len() + derived.values.len();
        if decl.args.len() != output_arity {
            let context = cx.context.clone();
            cx.errors.push(ValidationError::ArityMismatch {
                vocabulary: VocabularyKind::Predicate,
                name: derived.predicate.clone(),
                expected: decl.args.len(),
                actual: output_arity,
                context,
            });
        }
        let n = output_arity.min(decl.args.len());
        for position in 0..n {
            let actual = if position < derived.keys.len() {
                env.lookup(&derived.keys[position])
            } else {
                value_kinds[position - derived.keys.len()]
            };
            let expected = decl.args[position].kind;
            if let InferredKind::Known(actual_kind) = actual
                && !kinds_compatible(expected, actual_kind)
            {
                let context = cx.context.clone();
                cx.errors.push(ValidationError::ArgKindMismatch {
                    vocabulary: VocabularyKind::Predicate,
                    name: derived.predicate.clone(),
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
    /// Walk a predicate-shaped expression. Threads bindings
    /// through `env` for composition (`And`, `Implies`, `Pre`,
    /// etc.); `Or` branches walk a clone so a refinement in one
    /// branch does not constrain another. Comparator and
    /// arithmetic operands delegate to `check_operand_kind`.
    fn walk_predicate_expr(&mut self, expr: &Expr, env: &mut KindEnv) {
        match expr {
            Expr::Claim { predicate, args } => {
                self.check_predicate_ref(predicate, args, env);
            }
            Expr::And(items) => {
                // Conjuncts thread bindings forward: each branch
                // sees refinements made by earlier conjuncts.
                for item in items {
                    self.walk_predicate_expr(item, env);
                }
            }
            Expr::Or(items) => {
                // Disjuncts evaluate against the same base context
                // (mirrors `find_disjunction`). Each branch gets a
                // fresh clone so a refinement in one branch does
                // not conflict with another; branch-local
                // refinements do not leak out.
                for item in items {
                    let mut branch = env.clone();
                    self.walk_predicate_expr(item, &mut branch);
                }
            }
            Expr::Not(inner) | Expr::Pre(inner) => {
                self.walk_predicate_expr(inner, env);
            }
            Expr::Implies { left, right } => {
                self.walk_predicate_expr(left, env);
                self.walk_predicate_expr(right, env);
            }
            Expr::Exists { binding: _, body } => {
                // No shadowing: `find_matches` for Exists walks the
                // body against the same bindings it was called
                // with, so an outer variable of the same name as
                // `binding` acts as a unification constraint, not a
                // shadow. Refinements flow through.
                self.walk_predicate_expr(body, env);
            }
            Expr::Forall {
                binding: _,
                source,
                body,
            } => {
                // No shadowing, same reasoning as Exists:
                // `unify_args` treats an existing binding as a
                // constraint, so an outer variable of the same name
                // constrains the loop rather than being shadowed.
                self.walk_predicate_expr(source, env);
                self.walk_predicate_expr(body, env);
            }
            Expr::Le(left, right) => {
                self.check_operand_kind(left, PredicateArgKind::Decimal, "<=", env);
                self.check_operand_kind(right, PredicateArgKind::Decimal, "<=", env);
            }
            Expr::DateLe(left, right) => {
                self.check_operand_kind(left, PredicateArgKind::Date, "on_or_before", env);
                self.check_operand_kind(right, PredicateArgKind::Date, "on_or_before", env);
            }
            Expr::Eq(left, right) => {
                self.check_equality_operands(left, right, "==", env);
            }
            Expr::Neq(left, right) => {
                self.check_equality_terms(left, right, "!=", env);
            }
            Expr::In(_element, collection) => {
                // The collection side must be Collection-kinded.
                // Variables refine; literals and `actor` carry a
                // concrete kind the runtime would reject as "In
                // expects a collection" - surface that statically.
                match collection {
                    Term::Var(name) => {
                        self.observe_or_report(
                            env,
                            name,
                            InferredKind::Known(PredicateArgKind::Collection),
                        );
                    }
                    Term::Wildcard => {}
                    other => {
                        if let InferredKind::Known(actual) = term_kind(other)
                            && !kinds_compatible(PredicateArgKind::Collection, actual)
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
            // Value-shaped expressions at a predicate position are
            // not visited here; the runtime raises NotPredicate.
            // A symmetric static check (ExpectedPredicateExpression)
            // is a later layer.
            Expr::Term(_)
            | Expr::Add(_, _)
            | Expr::Sub(_, _)
            | Expr::Sum { .. }
            | Expr::ValueOf { .. } => {}
        }
    }

    /// Walk a statement, threading kind information through `env`
    /// per the runtime require/bind/let/for quartet:
    ///
    /// - `Require` walks a clone (refinements do not export).
    /// - `BindOne` walks the live env (refinements flow forward).
    /// - `Let` infers the value's kind and observes `name` at it.
    /// - `LetNewSubject` observes `name` at `Subject`.
    /// - `Assert`/`Retract` check args against declared kinds.
    /// - `For` checks the collection is Collection-kinded; the body
    ///   runs under a scoped env clone.
    /// - `Emit` checks args against the declared intent.
    fn walk_stmt(&mut self, stmt: &Stmt, env: &mut KindEnv) {
        match stmt {
            Stmt::Require(expr) => {
                let mut scoped = env.clone();
                self.walk_predicate_expr(expr, &mut scoped);
            }
            Stmt::BindOne(expr) => {
                self.walk_predicate_expr(expr, env);
            }
            Stmt::Let { name, value } => {
                let value_kind = self.infer_value_expr(value, env);
                self.observe_or_report(env, name, value_kind);
            }
            Stmt::LetNewSubject { name } => {
                self.observe_or_report(env, name, InferredKind::Known(PredicateArgKind::Subject));
            }
            Stmt::Assert(claim) => {
                self.check_predicate_ref(&claim.predicate, &claim.args, env);
            }
            Stmt::Retract { predicate, args } => {
                self.check_predicate_ref(predicate, args, env);
            }
            Stmt::For {
                binding,
                collection,
                body,
            } => {
                self.check_operand_kind(collection, PredicateArgKind::Collection, "for", env);
                // Body runs under a scoped env clone so loop-
                // introduced bindings do not leak across iterations
                // or beyond the loop.
                let mut scoped = env.clone();
                let _ = scoped.observe(binding, InferredKind::UnknownOrAny);
                for inner in body {
                    self.walk_stmt(inner, &mut scoped);
                }
            }
            Stmt::Emit(intent) => {
                self.check_intent_ref(&intent.name, &intent.args, env);
            }
        }
    }

    /// Check that a value-shaped operand evaluates to the expected
    /// kind. A bare variable observes directly (a conflict surfaces
    /// as `VariableKindConflict`, naming the variable); anything
    /// else infers its kind and emits `OperandKindMismatch` on
    /// disagreement (naming the operator and kinds).
    fn check_operand_kind(
        &mut self,
        operand: &Expr,
        expected: PredicateArgKind,
        operator: &'static str,
        env: &mut KindEnv,
    ) {
        if let Expr::Term(Term::Var(name)) = operand {
            self.observe_or_report(env, name, InferredKind::Known(expected));
            return;
        }
        let inferred = self.infer_value_expr(operand, env);
        if let InferredKind::Known(actual) = inferred
            && !kinds_compatible(expected, actual)
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

    /// Strict equality between two value operands. If both produce
    /// a `Known` kind they must be compatible; when one is a bare
    /// variable and the other contributes a concrete kind, the
    /// variable refines to it. `Subject == Decimal` is a kind
    /// error, never a coercion. Backs both `Eq` (Expr operands)
    /// and `Neq` (Term operands).
    fn check_equality(
        &mut self,
        left: EqualityOperand<'_>,
        right: EqualityOperand<'_>,
        operator: &'static str,
        env: &mut KindEnv,
    ) {
        let combined = match (left.0, right.0) {
            (InferredKind::Known(l), InferredKind::Known(r)) => {
                if !kinds_compatible(l, r) {
                    let context = self.context.clone();
                    self.errors.push(ValidationError::EqualityKindMismatch {
                        operator,
                        left: l,
                        right: r,
                        context,
                    });
                    None
                } else {
                    Some(InferredKind::Known(more_specific(l, r)))
                }
            }
            (k @ InferredKind::Known(_), InferredKind::UnknownOrAny)
            | (InferredKind::UnknownOrAny, k @ InferredKind::Known(_)) => Some(k),
            (InferredKind::UnknownOrAny, InferredKind::UnknownOrAny) => None,
        };
        if let Some(refined) = combined {
            for name in [left.1, right.1].into_iter().flatten() {
                self.observe_or_report(env, name, refined);
            }
        }
    }

    fn check_equality_operands(
        &mut self,
        left: &Expr,
        right: &Expr,
        operator: &'static str,
        env: &mut KindEnv,
    ) {
        let left_op = (self.infer_value_expr(left, env), expr_var_name(left));
        let right_op = (self.infer_value_expr(right, env), expr_var_name(right));
        self.check_equality(left_op, right_op, operator, env);
    }

    fn check_equality_terms(
        &mut self,
        left: &Term,
        right: &Term,
        operator: &'static str,
        env: &mut KindEnv,
    ) {
        self.check_equality(
            (resolved_term_kind(left, env), term_var_name(left)),
            (resolved_term_kind(right, env), term_var_name(right)),
            operator,
            env,
        );
    }

    /// Infer the kind of a value-producing expression. Variables
    /// look up via `env`; literals carry their kind; `Add`/`Sub`
    /// recursively check Decimal operands and return Decimal;
    /// `Sum` returns Decimal after a body-first walk under a
    /// cloned env; `ValueOf` returns its wildcard slot's declared
    /// kind. A predicate-shaped expression at a value position
    /// surfaces as `ExpectedValueExpression`.
    fn infer_value_expr(&mut self, expr: &Expr, env: &mut KindEnv) -> InferredKind {
        match expr {
            Expr::Term(term) => resolved_term_kind(term, env),
            Expr::Add(left, right) | Expr::Sub(left, right) => {
                let operator = if matches!(expr, Expr::Add(_, _)) {
                    "+"
                } else {
                    "-"
                };
                self.check_operand_kind(left, PredicateArgKind::Decimal, operator, env);
                self.check_operand_kind(right, PredicateArgKind::Decimal, operator, env);
                InferredKind::Known(PredicateArgKind::Decimal)
            }
            Expr::Sum {
                value,
                binding: _,
                body,
            } => {
                // Body-first inference on a cloned env so body-bound
                // names (the iteration binding, plus any others) do
                // not leak into the surrounding expression. Outer
                // variables stay visible via the clone. Sum's result
                // is always Decimal.
                let mut scoped = env.clone();
                self.walk_predicate_expr(body, &mut scoped);
                let resolved = resolved_term_kind(value, &scoped);
                if let InferredKind::Known(actual) = resolved
                    && !kinds_compatible(PredicateArgKind::Decimal, actual)
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
            Expr::ValueOf {
                predicate,
                args,
                default,
            } => {
                self.check_predicate_ref(predicate, args, env);
                let result_kind = value_of_result_kind(predicate, args, &self.predicates);
                if let Some(default_expr) = default {
                    let default_kind = self.infer_value_expr(default_expr, env);
                    // The runtime returns either the looked-up value
                    // or the default, so a kind mismatch between them
                    // is the same class of error as a comparator
                    // mismatch.
                    if let (InferredKind::Known(expected), InferredKind::Known(actual)) =
                        (result_kind, default_kind)
                        && !kinds_compatible(expected, actual)
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
            // Predicate-shaped expression at a value position. The
            // runtime raises `NotValue`; surface it earlier.
            Expr::Claim { .. }
            | Expr::Implies { .. }
            | Expr::Exists { .. }
            | Expr::Forall { .. }
            | Expr::And(_)
            | Expr::Or(_)
            | Expr::Not(_)
            | Expr::Pre(_)
            | Expr::Eq(_, _)
            | Expr::Le(_, _)
            | Expr::DateLe(_, _)
            | Expr::Neq(_, _)
            | Expr::In(_, _) => {
                let context = self.context.clone();
                self.errors.push(ValidationError::ExpectedValueExpression {
                    context,
                    expression: short_expr_shape(expr),
                });
                InferredKind::UnknownOrAny
            }
        }
    }

    /// Check a predicate reference end to end: declared, right
    /// arity, then arg kinds. An undeclared predicate emits
    /// `Undeclared` and stops; a wrong arity emits `ArityMismatch`
    /// but still kind-checks the positions both sides share.
    fn check_predicate_ref(&mut self, predicate: &str, args: &[Term], env: &mut KindEnv) {
        self.check_reference(VocabularyKind::Predicate, predicate, args, env);
    }

    /// Same, against the intent vocabulary; powers `Stmt::Emit`.
    fn check_intent_ref(&mut self, intent: &str, args: &[Term], env: &mut KindEnv) {
        self.check_reference(VocabularyKind::Intent, intent, args, env);
    }

    /// Shared declared + arity + arg-kind check for a reference in
    /// either vocabulary. The `.copied()` detaches the declaration
    /// from the borrow of `self`, so the subsequent `&mut self`
    /// arg-kind walk is free of a borrow conflict.
    fn check_reference(
        &mut self,
        vocabulary: VocabularyKind,
        name: &str,
        args: &[Term],
        env: &mut KindEnv,
    ) {
        let decl_args = match vocabulary {
            VocabularyKind::Predicate => self.predicates.get(name).copied().map(|d| &d.args),
            VocabularyKind::Intent => self.intents.get(name).copied().map(|d| &d.args),
        };
        let Some(decl_args) = decl_args else {
            let context = self.context.clone();
            self.errors.push(ValidationError::Undeclared {
                vocabulary,
                name: name.to_string(),
                context,
            });
            return;
        };
        if decl_args.len() != args.len() {
            let context = self.context.clone();
            self.errors.push(ValidationError::ArityMismatch {
                vocabulary,
                name: name.to_string(),
                expected: decl_args.len(),
                actual: args.len(),
                context,
            });
        }
        self.check_args(vocabulary, name, args, decl_args, env);
    }

    /// Generic arg-list kind check. A literal contributes its kind;
    /// a variable is observed (refining the env); `Wildcard` is
    /// skipped; `Actor` contributes `Subject`. Walks only
    /// `min(args, decl)`; the structural pass owns arity.
    fn check_args(
        &mut self,
        vocabulary: VocabularyKind,
        name: &str,
        args: &[Term],
        decl_args: &[crate::ArgDecl],
        env: &mut KindEnv,
    ) {
        let n = args.len().min(decl_args.len());
        for (position, (arg, decl_arg)) in args
            .iter()
            .take(n)
            .zip(decl_args.iter().take(n))
            .enumerate()
        {
            self.check_one_arg(vocabulary, name, position, arg, decl_arg.kind, env);
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
        env: &mut KindEnv,
    ) {
        let actual = term_kind(arg);
        if let Term::Var(var_name) = arg {
            if let Err((previous, new)) = env.observe(var_name, InferredKind::Known(expected)) {
                let context = self.context.clone();
                self.errors.push(ValidationError::VariableKindConflict {
                    variable: var_name.clone(),
                    previous,
                    new,
                    context,
                });
            }
            // VariableKindConflict is the right diagnostic when the
            // variable already held an incompatible kind.
        } else if let InferredKind::Known(actual_kind) = actual
            && !kinds_compatible(expected, actual_kind)
        {
            let context = self.context.clone();
            self.errors.push(ValidationError::ArgKindMismatch {
                vocabulary,
                name: name.to_string(),
                position,
                expected,
                actual: actual_kind,
                context,
            });
        }
    }

    /// Observe `name` at `kind`; on a refinement conflict, push a
    /// `VariableKindConflict` carrying both kinds.
    fn observe_or_report(&mut self, env: &mut KindEnv, name: &str, kind: InferredKind) {
        if let Err((previous, new)) = env.observe(name, kind) {
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
type EqualityOperand<'a> = (InferredKind, Option<&'a str>);

fn expr_var_name(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Term(Term::Var(name)) => Some(name.as_str()),
        _ => None,
    }
}

fn term_var_name(term: &Term) -> Option<&str> {
    match term {
        Term::Var(name) => Some(name.as_str()),
        _ => None,
    }
}

/// Resolve a `Term`'s kind through the env: variables look up
/// their current inferred kind; literals and `actor` return their
/// inherent kind. Wildcard stays UnknownOrAny.
fn resolved_term_kind(term: &Term, env: &KindEnv) -> InferredKind {
    match term {
        Term::Var(name) => env.lookup(name),
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
        .map(|a| InferredKind::Known(a.kind))
        .unwrap_or(InferredKind::UnknownOrAny)
}

/// Whether an expression references `Term::Actor` anywhere in its
/// tree. Used to flag `actor` in invariant and derived-claim
/// bodies, where the runtime raises `EvalError::UnboundActor`
/// because no proposing transition is in scope.
///
/// A standalone exhaustive walk rather than a hook in the kind
/// visitor: `actor` can sit in any term position (claim arg,
/// comparator operand, `sum` target, `in` operand), and a
/// dedicated scan with no `_` arm guarantees a future `Expr`
/// variant cannot let an `actor` slip through unnoticed.
fn expr_mentions_actor(expr: &Expr) -> bool {
    fn is_actor(t: &Term) -> bool {
        matches!(t, Term::Actor)
    }
    match expr {
        Expr::Term(t) => is_actor(t),
        Expr::Claim { args, .. } => args.iter().any(is_actor),
        Expr::ValueOf { args, default, .. } => {
            args.iter().any(is_actor) || default.as_ref().is_some_and(|d| expr_mentions_actor(d))
        }
        Expr::Neq(a, b) | Expr::In(a, b) => is_actor(a) || is_actor(b),
        Expr::Sum { value, body, .. } => is_actor(value) || expr_mentions_actor(body),
        Expr::And(items) | Expr::Or(items) => items.iter().any(expr_mentions_actor),
        Expr::Not(e) | Expr::Pre(e) | Expr::Exists { body: e, .. } => expr_mentions_actor(e),
        Expr::Implies { left, right }
        | Expr::Eq(left, right)
        | Expr::Le(left, right)
        | Expr::DateLe(left, right)
        | Expr::Add(left, right)
        | Expr::Sub(left, right) => expr_mentions_actor(left) || expr_mentions_actor(right),
        Expr::Forall { source, body, .. } => {
            expr_mentions_actor(source) || expr_mentions_actor(body)
        }
    }
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
    }
}

/// Short structural label for an expression used in
/// `ExpectedValueExpression`. Not a full pretty-print; just the
/// outermost constructor.
fn short_expr_shape(expr: &Expr) -> String {
    match expr {
        Expr::Claim { predicate, .. } => format!("claim {predicate}(...)"),
        Expr::Implies { .. } => "_ implies _".to_string(),
        Expr::Exists { .. } => "exists _: _".to_string(),
        Expr::Forall { .. } => "forall _ in _: _".to_string(),
        Expr::And(_) => "_ and _".to_string(),
        Expr::Or(_) => "_ or _".to_string(),
        Expr::Not(_) => "not _".to_string(),
        Expr::Pre(_) => "pre(_)".to_string(),
        Expr::Eq(_, _) => "_ == _".to_string(),
        Expr::Le(_, _) => "_ <= _".to_string(),
        Expr::DateLe(_, _) => "_ on_or_before _".to_string(),
        Expr::Neq(_, _) => "_ != _".to_string(),
        Expr::In(_, _) => "_ in _".to_string(),
        Expr::Term(_) => "term".to_string(),
        Expr::Add(_, _) => "_ + _".to_string(),
        Expr::Sub(_, _) => "_ - _".to_string(),
        Expr::Sum { .. } => "sum(...)".to_string(),
        Expr::ValueOf { predicate, .. } => format!("value {predicate}(...)"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn refine_unknown_to_known_yields_known() {
        let refined = InferredKind::UnknownOrAny
            .refine(InferredKind::Known(PredicateArgKind::Decimal))
            .expect("compatible");
        assert_eq!(refined, InferredKind::Known(PredicateArgKind::Decimal));
    }

    #[test]
    fn refine_known_then_unknown_keeps_known() {
        let refined = InferredKind::Known(PredicateArgKind::Decimal)
            .refine(InferredKind::UnknownOrAny)
            .expect("compatible");
        assert_eq!(refined, InferredKind::Known(PredicateArgKind::Decimal));
    }

    #[test]
    fn refine_any_then_decimal_yields_decimal() {
        // The `Any`-declared slot was the first observation; a
        // later specific use refines the variable to that specific
        // kind rather than leaving it permissive.
        let refined = InferredKind::Known(PredicateArgKind::Any)
            .refine(InferredKind::Known(PredicateArgKind::Decimal))
            .expect("compatible");
        assert_eq!(refined, InferredKind::Known(PredicateArgKind::Decimal));
    }

    #[test]
    fn refine_decimal_then_any_keeps_decimal() {
        let refined = InferredKind::Known(PredicateArgKind::Decimal)
            .refine(InferredKind::Known(PredicateArgKind::Any))
            .expect("compatible");
        assert_eq!(refined, InferredKind::Known(PredicateArgKind::Decimal));
    }

    #[test]
    fn refine_decimal_then_subject_conflicts() {
        let err = InferredKind::Known(PredicateArgKind::Decimal)
            .refine(InferredKind::Known(PredicateArgKind::Subject))
            .expect_err("conflict");
        assert_eq!(err, (PredicateArgKind::Decimal, PredicateArgKind::Subject));
    }

    #[test]
    fn kindenv_observe_then_lookup_returns_refined_kind() {
        let mut env = KindEnv::new();
        env.observe("amount", InferredKind::Known(PredicateArgKind::Decimal))
            .expect("first observation always succeeds against UnknownOrAny");
        assert_eq!(
            env.lookup("amount"),
            InferredKind::Known(PredicateArgKind::Decimal)
        );
    }

    #[test]
    fn kindenv_observe_refines_through_any() {
        let mut env = KindEnv::new();
        env.observe("x", InferredKind::Known(PredicateArgKind::Any))
            .unwrap();
        env.observe("x", InferredKind::Known(PredicateArgKind::Decimal))
            .unwrap();
        assert_eq!(
            env.lookup("x"),
            InferredKind::Known(PredicateArgKind::Decimal)
        );
    }

    #[test]
    fn kindenv_observe_reports_conflict_with_previous_kinds() {
        let mut env = KindEnv::new();
        env.observe("x", InferredKind::Known(PredicateArgKind::Decimal))
            .unwrap();
        let err = env
            .observe("x", InferredKind::Known(PredicateArgKind::Subject))
            .expect_err("conflict");
        assert_eq!(err, (PredicateArgKind::Decimal, PredicateArgKind::Subject));
    }

    // ============================================================
    // check_program: claim arg checking + statement flow
    // ============================================================

    use crate::dsl::*;
    use crate::ir::{ArgDecl, Intent, Invariant, Program, Transformation};

    /// Build a `PredicateDecl` shorthand for tests.
    fn pdecl(name: &str, args: &[(&str, PredicateArgKind)]) -> crate::ir::PredicateDecl {
        crate::ir::PredicateDecl {
            name: name.to_string(),
            args: args
                .iter()
                .map(|(n, k)| ArgDecl {
                    name: n.to_string(),
                    kind: *k,
                })
                .collect(),
        }
    }

    fn empty_program() -> Program {
        Program {
            name: "test".to_string(),
            predicates: vec![],
            intents: vec![],
            invariants: vec![],
            transformations: vec![],
            derived_claims: vec![],
        }
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
        p.invariants = vec![Invariant {
            name: "any_policy_has_positive_limit".to_string(),
            version: 1,
            body: claim("Policy", vec![var("p"), var("l")]),
        }];
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
        p.invariants = vec![Invariant {
            name: "bad".to_string(),
            version: 1,
            body: claim("Policy", vec![dec("123")]),
        }];
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
        p.invariants = vec![Invariant {
            name: "refine".to_string(),
            version: 1,
            body: and(vec![claim("A", vec![var("x")]), claim("B", vec![var("x")])]),
        }];
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
        p.invariants = vec![Invariant {
            name: "conflict".to_string(),
            version: 1,
            body: and(vec![claim("A", vec![var("x")]), claim("B", vec![var("x")])]),
        }];
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
        p.invariants = vec![Invariant {
            name: "refines_through_any".to_string(),
            version: 1,
            body: and(vec![claim("A", vec![var("x")]), claim("B", vec![var("x")])]),
        }];
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
        // mismatch, not the actor-context error Layer 3 would add
        // in an invariant.
        let mut p = empty_program();
        p.predicates = vec![pdecl("Limit", &[("amount", PredicateArgKind::Decimal)])];
        p.transformations = vec![Transformation {
            name: "t".to_string(),
            parameters: vec![],
            body: vec![assert_("Limit", vec![actor()])],
        }];
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

    // ----- Layer 3: actor-in-wrong-context -----

    #[test]
    fn actor_in_invariant_body_flags_actor_not_available() {
        // `actor` in an invariant body has no proposing transition
        // in scope; the kernel raises UnboundActor at runtime, the
        // check flags it statically. The predicate slot is Subject
        // so no kind error muddies the result.
        let mut p = empty_program();
        p.predicates = vec![pdecl("Approver", &[("who", PredicateArgKind::Subject)])];
        p.invariants = vec![Invariant {
            name: "mentions_actor".to_string(),
            version: 1,
            body: claim("Approver", vec![actor()]),
        }];
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
            predicate: "Row".to_string(),
            keys: vec!["k".to_string()],
            values: vec![crate::ir::DerivedValue {
                name: "v".to_string(),
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

    #[test]
    fn actor_in_transformation_body_is_allowed() {
        // `actor` resolves inside transformation bodies - no
        // ActorNotAvailable. Slot is Subject so no kind error either.
        let mut p = empty_program();
        p.predicates = vec![pdecl("Approver", &[("who", PredicateArgKind::Subject)])];
        p.transformations = vec![Transformation {
            name: "t".to_string(),
            parameters: vec![],
            body: vec![assert_("Approver", vec![actor()])],
        }];
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
        p.transformations = vec![Transformation {
            name: "t".to_string(),
            parameters: vec![],
            body: vec![
                bind_one(claim("A", vec![var("x")])),
                assert_("B", vec![var("x")]),
            ],
        }];
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
        p.transformations = vec![Transformation {
            name: "t".to_string(),
            parameters: vec![],
            body: vec![
                bind_one(claim("A", vec![var("x")])),
                assert_("B", vec![var("x")]),
            ],
        }];
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
        // require A(x) sees x as Decimal but should NOT export it.
        // Then assert B(x) (Subject) should not conflict because x
        // is fresh again at the assert.
        let mut p = empty_program();
        p.predicates = vec![
            pdecl("A", &[("v", PredicateArgKind::Decimal)]),
            pdecl("B", &[("v", PredicateArgKind::Subject)]),
        ];
        p.transformations = vec![Transformation {
            name: "t".to_string(),
            parameters: vec![],
            body: vec![
                require(claim("A", vec![var("x")])),
                assert_("B", vec![var("x")]),
            ],
        }];
        let errs = check_program(&p);
        assert!(
            errs.is_empty(),
            "require must not export bindings; got {errs:?}"
        );
    }

    #[test]
    fn let_new_subject_binds_name_as_subject() {
        // let_new_subject names a fresh subject; using it in a
        // Decimal slot must flag.
        let mut p = empty_program();
        p.predicates = vec![pdecl("Amt", &[("v", PredicateArgKind::Decimal)])];
        p.transformations = vec![Transformation {
            name: "t".to_string(),
            parameters: vec![],
            body: vec![let_new_subject("fresh"), assert_("Amt", vec![var("fresh")])],
        }];
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
        p.transformations = vec![Transformation {
            name: "t".to_string(),
            parameters: vec![],
            body: vec![retract("P", vec![dec("99")])],
        }];
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

    #[test]
    fn le_with_date_literal_left_operand_flags_operand_mismatch() {
        // `<=` is the decimal comparator. A date literal on either
        // side is the canonical "wrong comparator" mistake the
        // kernel surfaces as TypeMismatch at runtime.
        let mut p = empty_program();
        p.invariants = vec![Invariant {
            name: "bad_le".to_string(),
            version: 1,
            body: le(term(date("2026-01-01")), term(dec("100"))),
        }];
        let errs = check_program(&p);
        assert_eq!(errs.len(), 1, "expected one operand error; got {errs:?}");
        match &errs[0] {
            ValidationError::OperandKindMismatch {
                operator,
                expected,
                actual,
                ..
            } => {
                assert_eq!(*operator, "<=");
                assert_eq!(*expected, PredicateArgKind::Decimal);
                assert_eq!(*actual, PredicateArgKind::Date);
            }
            other => panic!("expected OperandKindMismatch, got {other:?}"),
        }
    }

    #[test]
    fn date_le_with_decimal_literal_flags_operand_mismatch() {
        // `on_or_before` is the date comparator; decimal here is wrong.
        let mut p = empty_program();
        p.invariants = vec![Invariant {
            name: "bad_date_le".to_string(),
            version: 1,
            body: date_le(term(date("2026-01-01")), term(dec("100"))),
        }];
        let errs = check_program(&p);
        assert_eq!(errs.len(), 1);
        match &errs[0] {
            ValidationError::OperandKindMismatch {
                operator,
                expected,
                actual,
                ..
            } => {
                assert_eq!(*operator, "on_or_before");
                assert_eq!(*expected, PredicateArgKind::Date);
                assert_eq!(*actual, PredicateArgKind::Decimal);
            }
            other => panic!("expected OperandKindMismatch, got {other:?}"),
        }
    }

    #[test]
    fn add_with_subject_literal_operand_flags_operand_mismatch() {
        // Arithmetic on a subject literal is the unambiguous bug.
        let mut p = empty_program();
        p.invariants = vec![Invariant {
            name: "bad_add".to_string(),
            version: 1,
            body: le(
                add(term(dec("10")), term(subj("not_a_number"))),
                term(dec("100")),
            ),
        }];
        let errs = check_program(&p);
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::OperandKindMismatch {
                    operator: "+",
                    expected: PredicateArgKind::Decimal,
                    actual: PredicateArgKind::Subject,
                    ..
                }
            )),
            "expected OperandKindMismatch on `+`, got {errs:?}"
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
        p.invariants = vec![Invariant {
            name: "refine_via_le".to_string(),
            version: 1,
            body: and(vec![
                claim("A", vec![var("x")]),
                le(term(var("x")), term(dec("100"))),
                claim("B", vec![var("x")]),
            ]),
        }];
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
    fn eq_with_distinct_known_kinds_flags_operand_mismatch() {
        // Eq is strict: Decimal == Subject must surface as a kind
        // mismatch, not be silently coerced.
        let mut p = empty_program();
        p.invariants = vec![Invariant {
            name: "bad_eq".to_string(),
            version: 1,
            body: eq(term(dec("100")), term(subj("S"))),
        }];
        let errs = check_program(&p);
        assert_eq!(errs.len(), 1);
        assert!(matches!(
            errs[0],
            ValidationError::EqualityKindMismatch { operator: "==", .. }
        ));
    }

    #[test]
    fn neq_with_distinct_known_kinds_flags_operand_mismatch() {
        // Neq's operands are Terms; strict-equality rules still apply.
        let mut p = empty_program();
        p.invariants = vec![Invariant {
            name: "bad_neq".to_string(),
            version: 1,
            body: neq(dec("100"), subj("S")),
        }];
        let errs = check_program(&p);
        assert_eq!(errs.len(), 1);
        assert!(matches!(
            errs[0],
            ValidationError::EqualityKindMismatch { operator: "!=", .. }
        ));
    }

    #[test]
    fn eq_refines_variable_to_concrete_kind_for_subsequent_uses() {
        // `x == 100` against an otherwise unconstrained `x` should
        // pin `x` to Decimal; a later Subject-slot use conflicts.
        let mut p = empty_program();
        p.predicates = vec![pdecl("B", &[("v", PredicateArgKind::Subject)])];
        p.invariants = vec![Invariant {
            name: "refine_via_eq".to_string(),
            version: 1,
            body: and(vec![
                eq(term(var("x")), term(dec("100"))),
                claim("B", vec![var("x")]),
            ]),
        }];
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
        p.transformations = vec![Transformation {
            name: "t".to_string(),
            parameters: vec![],
            body: vec![
                bind_one(claim("A", vec![var("x")])),
                let_("y", sub(term(var("x")), term(dec("1")))),
                assert_("S", vec![var("y")]),
            ],
        }];
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
        p.invariants = vec![Invariant {
            name: "ok".to_string(),
            version: 1,
            body: and(vec![
                claim("A", vec![var("amount")]),
                claim("L", vec![var("limit")]),
                le(term(var("amount")), term(var("limit"))),
            ]),
        }];
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
        p.invariants = vec![Invariant {
            name: "ok_sum".to_string(),
            version: 1,
            body: le(
                sum(
                    var("amount"),
                    "amount",
                    claim("Payment", vec![wildcard(), var("amount")]),
                ),
                term(dec("1000")),
            ),
        }];
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
        p.invariants = vec![Invariant {
            name: "bad_sum".to_string(),
            version: 1,
            body: le(
                sum(var("p"), "p", claim("Payment", vec![var("p"), wildcard()])),
                term(dec("1000")),
            ),
        }];
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
        p.invariants = vec![Invariant {
            name: "bad_sum_lit".to_string(),
            version: 1,
            body: le(
                sum(date("2026-01-01"), "x", claim("X", vec![var("x")])),
                term(dec("100")),
            ),
        }];
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
        p.transformations = vec![Transformation {
            name: "t".to_string(),
            parameters: vec![],
            body: vec![
                bind_one(claim("Q", vec![var("x")])),
                require(le(
                    term(var("x")),
                    sum(
                        var("amount"),
                        "amount",
                        claim("P", vec![wildcard(), var("amount")]),
                    ),
                )),
                // If Sum had leaked `amount` as Decimal into the
                // outer env, this assert would flag a conflict.
                // It must NOT - amount is fresh again here.
                assert_("S", vec![var("amount")]),
            ],
        }];
        let errs = check_program(&p);
        assert!(
            errs.is_empty(),
            "Sum's body bindings must not leak; got {errs:?}"
        );
    }

    #[test]
    fn value_of_resolves_to_wildcard_slot_kind() {
        // `value Policy(p, _) <= 100` - the wildcard marks
        // Policy's second slot (Decimal); Le's RHS is Decimal;
        // no error.
        let mut p = empty_program();
        p.predicates = vec![pdecl(
            "Policy",
            &[
                ("policy", PredicateArgKind::Subject),
                ("limit", PredicateArgKind::Decimal),
            ],
        )];
        p.invariants = vec![Invariant {
            name: "ok_value_of".to_string(),
            version: 1,
            body: le(
                value_of("Policy", vec![var("p"), wildcard()]),
                term(dec("100")),
            ),
        }];
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
        p.invariants = vec![Invariant {
            name: "bad_value_of".to_string(),
            version: 1,
            body: le(
                value_of("Owner", vec![var("p"), wildcard()]),
                term(dec("100")),
            ),
        }];
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
            predicate: "Row".to_string(),
            keys: vec!["account".to_string()],
            values: vec![DerivedValue {
                name: "balance".to_string(),
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
            predicate: "Row".to_string(),
            keys: vec!["account".to_string()],
            values: vec![DerivedValue {
                name: "count".to_string(),
                expr: sum(
                    var("amt"),
                    "amt",
                    claim("P", vec![var("account"), var("amt")]),
                ),
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
            predicate: "Row".to_string(),
            keys: vec!["account".to_string()],
            values: vec![DerivedValue {
                name: "balance".to_string(),
                expr: sum(
                    var("amt"),
                    "amt",
                    claim("Line", vec![var("account"), var("amt")]),
                ),
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
        p.transformations = vec![Transformation {
            name: "t".to_string(),
            parameters: vec![],
            body: vec![
                bind_one(claim("Q", vec![var("x")])),
                for_("e", term(var("x")), vec![]),
            ],
        }];
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
        p.transformations = vec![Transformation {
            name: "t".to_string(),
            parameters: vec!["xs".to_string()],
            body: vec![
                for_("e", term(var("xs")), vec![]),
                assert_("P", vec![var("xs")]),
            ],
        }];
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
        p.invariants = vec![Invariant {
            name: "in_lit".to_string(),
            version: 1,
            body: Expr::In(Term::Var("x".to_string()), dec("100")),
        }];
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
            name: name.to_string(),
            args: args
                .iter()
                .map(|(n, k)| ArgDecl {
                    name: n.to_string(),
                    kind: *k,
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
        p.transformations = vec![Transformation {
            name: "t".to_string(),
            parameters: vec![],
            body: vec![Stmt::Emit(Intent {
                name: "Notify".to_string(),
                args: vec![dec("100")],
            })],
        }];
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
        p.transformations = vec![Transformation {
            name: "t".to_string(),
            parameters: vec![],
            body: vec![
                bind_one(claim("P", vec![var("x")])),
                Stmt::Emit(Intent {
                    name: "Notify".to_string(),
                    args: vec![var("x")],
                }),
            ],
        }];
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
        p.transformations = vec![Transformation {
            name: "t".to_string(),
            parameters: vec![],
            body: vec![
                bind_one(claim("P", vec![var("x")])),
                Stmt::Emit(Intent {
                    name: "Notify".to_string(),
                    args: vec![var("x"), dec("5")],
                }),
            ],
        }];
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
        p.invariants = vec![Invariant {
            name: "or_disjoint".to_string(),
            version: 1,
            body: or(vec![claim("A", vec![var("x")]), claim("B", vec![var("x")])]),
        }];
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
        p.invariants = vec![Invariant {
            name: "or_no_leak".to_string(),
            version: 1,
            body: and(vec![
                or(vec![claim("A", vec![var("x")]), claim("B", vec![var("x")])]),
                claim("C", vec![var("x")]),
            ]),
        }];
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
        p.invariants = vec![Invariant {
            name: "or_inner_error".to_string(),
            version: 1,
            body: or(vec![claim("A", vec![dec("100")])]),
        }];
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
        p.invariants = vec![Invariant {
            name: "forall_unify".to_string(),
            version: 1,
            body: and(vec![
                claim("S", vec![var("x")]),
                forall("x", claim("P", vec![var("x")]), claim("C", vec![var("x")])),
            ]),
        }];
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
        p.invariants = vec![Invariant {
            name: "exists_unify".to_string(),
            version: 1,
            body: and(vec![
                claim("S", vec![var("x")]),
                exists("x", claim("D", vec![var("x")])),
            ]),
        }];
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
        p.invariants = vec![Invariant {
            name: "bad_default".to_string(),
            version: 1,
            body: le(
                value_of_with_default(
                    "Policy",
                    vec![var("p"), wildcard()],
                    term(subj("UNLIMITED")),
                ),
                term(dec("100")),
            ),
        }];
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
