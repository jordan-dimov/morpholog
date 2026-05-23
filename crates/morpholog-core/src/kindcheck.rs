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
use crate::validate::{ValidationContext, ValidationError};

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

    /// Run `body` with `name` shadowed in scope. The binding's
    /// pre-`body` value (if any) is restored on exit regardless of
    /// what the body did. Refinements to *other* variables made
    /// inside `body` leak through normally - mirrors the runtime
    /// semantics where a quantifier binding (`forall x in ...`,
    /// `exists x: ...`, `sum(_ | x ...)`) introduces a fresh `x`
    /// scoped to the body while still seeing the outer context.
    pub(crate) fn with_shadow<R>(&mut self, name: &str, body: impl FnOnce(&mut Self) -> R) -> R {
        let saved = self.bindings.remove(name);
        let result = body(self);
        match saved {
            Some(prev) => {
                self.bindings.insert(name.to_string(), prev);
            }
            None => {
                self.bindings.remove(name);
            }
        }
        result
    }
}

/// Run the kind checker over the whole programme. Returns the
/// full list of detected mismatches; an empty `Vec` means the
/// programme is kind-consistent. Traversal order matches the
/// structural pass (invariants, then transformations, then
/// derived claims) so merged diagnostics come out in a
/// predictable shape.
pub(crate) fn kindcheck_program(program: &Program) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    let predicate_decls: HashMap<&str, &PredicateDecl> = program
        .predicates
        .iter()
        .map(|d| (d.name.as_str(), d))
        .collect();

    for inv in &program.invariants {
        let mut env = KindEnv::new();
        let ctx = ValidationContext::Invariant {
            name: inv.name.clone(),
        };
        walk_predicate_expr(&inv.body, &mut env, &predicate_decls, &ctx, &mut errors);
    }

    for transformation in &program.transformations {
        let ctx = ValidationContext::Transformation {
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
            walk_stmt(stmt, &mut env, &predicate_decls, &ctx, &mut errors);
        }
    }

    for derived in &program.derived_claims {
        let mut env = KindEnv::new();
        let ctx = ValidationContext::DerivedClaim {
            predicate: derived.predicate.clone(),
        };
        walk_predicate_expr(
            &derived.domain,
            &mut env,
            &predicate_decls,
            &ctx,
            &mut errors,
        );
        // Each `value <name> = <expr>` is value-producing; infer
        // under the env built by `domain`, surfacing in-expression
        // kind errors (operand mismatches, variable conflicts).
        for value in &derived.values {
            infer_value_expr(&value.expr, &mut env, &predicate_decls, &ctx, &mut errors);
        }
    }

    errors
}

/// Walk a predicate-shaped expression and emit any claim-arg or
/// variable-conflict errors. Threads bindings through `env` for
/// composition (`And`, `Or`, `Implies`, `Pre`, etc.); `Forall`,
/// `Exists`, and `Sum` shadow their binding via `with_shadow`.
/// Comparator and arithmetic operands delegate to
/// `check_operand_kind`, which infers via `infer_value_expr`.
fn walk_predicate_expr(
    expr: &Expr,
    env: &mut KindEnv,
    predicate_decls: &HashMap<&str, &PredicateDecl>,
    ctx: &ValidationContext,
    errors: &mut Vec<ValidationError>,
) {
    match expr {
        Expr::Claim { predicate, args } => {
            check_claim_args(predicate, args, env, predicate_decls, ctx, errors);
        }
        Expr::And(items) | Expr::Or(items) => {
            for item in items {
                walk_predicate_expr(item, env, predicate_decls, ctx, errors);
            }
        }
        Expr::Not(inner) | Expr::Pre(inner) => {
            walk_predicate_expr(inner, env, predicate_decls, ctx, errors);
        }
        Expr::Implies { left, right } => {
            walk_predicate_expr(left, env, predicate_decls, ctx, errors);
            walk_predicate_expr(right, env, predicate_decls, ctx, errors);
        }
        Expr::Exists { binding, body } => {
            env.with_shadow(binding, |env| {
                walk_predicate_expr(body, env, predicate_decls, ctx, errors);
            });
        }
        Expr::Forall {
            binding,
            source,
            body,
        } => {
            // The binding is defined by `source` and consumed by
            // `body` - both run under the shadow so the loop-local
            // x cannot collide with an outer x of a different
            // kind. (The runtime's forall introduces a fresh x
            // before evaluating source's matches.)
            env.with_shadow(binding, |env| {
                walk_predicate_expr(source, env, predicate_decls, ctx, errors);
                walk_predicate_expr(body, env, predicate_decls, ctx, errors);
            });
        }
        Expr::Le(left, right) => {
            check_operand_kind(
                left,
                PredicateArgKind::Decimal,
                "<=",
                env,
                predicate_decls,
                ctx,
                errors,
            );
            check_operand_kind(
                right,
                PredicateArgKind::Decimal,
                "<=",
                env,
                predicate_decls,
                ctx,
                errors,
            );
        }
        Expr::DateLe(left, right) => {
            check_operand_kind(
                left,
                PredicateArgKind::Date,
                "on_or_before",
                env,
                predicate_decls,
                ctx,
                errors,
            );
            check_operand_kind(
                right,
                PredicateArgKind::Date,
                "on_or_before",
                env,
                predicate_decls,
                ctx,
                errors,
            );
        }
        Expr::Eq(left, right) => {
            check_equality_operands(left, right, "==", env, predicate_decls, ctx, errors)
        }
        Expr::Neq(left, right) => {
            check_equality_terms(left, right, "!=", env, ctx, errors);
        }
        Expr::In(_element, collection) => {
            // The collection side must be Collection-kinded; refining
            // a variable there is the only correlation we can do.
            // Element vs collection-element-kind correlation lands
            // when a worked example forces it.
            if let Term::Var(name) = collection {
                observe_or_report(
                    env,
                    name,
                    InferredKind::Known(PredicateArgKind::Collection),
                    ctx,
                    errors,
                );
            }
        }
        // Value-shaped expressions cannot appear at a predicate
        // position; the runtime raises NotPredicate. A symmetric
        // static check would need an ExpectedPredicateExpression
        // variant; deferred until a worked example surfaces the
        // case (Layer 2 unbound-variable detection is a more
        // natural home for it).
        Expr::Term(_)
        | Expr::Add(_, _)
        | Expr::Sub(_, _)
        | Expr::Sum { .. }
        | Expr::ValueOf { .. } => {}
    }
}

/// Walk a statement, threading kind information through the env
/// according to the runtime quartet doctrine. The semantics must
/// match what `propose.rs` does at runtime:
///
/// - `Require` checks against a cloned env: refinements observed
///   inside do NOT export to later statements (mirrors the runtime
///   yes/no gate).
/// - `BindOne` walks against the live env; refinements flow forward.
/// - `Let { name, value }` infers `value`'s kind and observes
///   `name` at that kind.
/// - `LetNewSubject { name }` observes `name` at `Subject`.
/// - `Assert` and `Retract` check args against declared kinds,
///   same as `Expr::Claim`. Assert does not export bindings.
/// - `For { binding, body }` runs the body under a scoped env
///   clone; loop-introduced bindings do not leak.
/// - `Emit` is a no-op until `IntentDecl` lands.
fn walk_stmt(
    stmt: &Stmt,
    env: &mut KindEnv,
    predicate_decls: &HashMap<&str, &PredicateDecl>,
    ctx: &ValidationContext,
    errors: &mut Vec<ValidationError>,
) {
    match stmt {
        Stmt::Require(expr) => {
            // Require does NOT export bindings. Walk against a
            // clone so any refinements inside the require do not
            // leak forward - this mirrors how the runtime treats
            // require as a yes/no gate that returns the original
            // binding context unchanged on success.
            let mut scoped = env.clone();
            walk_predicate_expr(expr, &mut scoped, predicate_decls, ctx, errors);
        }
        Stmt::BindOne(expr) => {
            // BindOne extends the binding context with the matched
            // binding set. Refinements observed here flow forward.
            walk_predicate_expr(expr, env, predicate_decls, ctx, errors);
        }
        Stmt::Let { name, value } => {
            // Infer the value-expression's kind and bind `name`
            // at it. Variable refinements observed while walking
            // `value` flow forward via the live env.
            let value_kind = infer_value_expr(value, env, predicate_decls, ctx, errors);
            observe_or_report(env, name, value_kind, ctx, errors);
        }
        Stmt::LetNewSubject { name } => {
            // `new Subject()` mints a fresh subject id; the
            // bound name is unambiguously Subject-kinded.
            observe_or_report(
                env,
                name,
                InferredKind::Known(PredicateArgKind::Subject),
                ctx,
                errors,
            );
        }
        Stmt::Assert(claim) => {
            check_claim_args(
                &claim.predicate,
                &claim.args,
                env,
                predicate_decls,
                ctx,
                errors,
            );
        }
        Stmt::Retract { predicate, args } => {
            check_claim_args(predicate, args, env, predicate_decls, ctx, errors);
        }
        Stmt::For {
            binding,
            collection: _,
            body,
        } => {
            // Body runs under a scoped env clone so loop-introduced
            // bindings do not leak across iterations or beyond the
            // loop. The iteration variable's element kind is
            // unknown without collection-element typing; observed
            // at UnknownOrAny so body uses refine on demand.
            let mut scoped = env.clone();
            let _ = scoped.observe(binding, InferredKind::UnknownOrAny);
            for inner in body {
                walk_stmt(inner, &mut scoped, predicate_decls, ctx, errors);
            }
        }
        Stmt::Emit(_intent) => {
            // Intent emission has no declared vocabulary today;
            // `IntentDecl` would gate this. Until then, nothing
            // to check at the kind layer.
        }
    }
}

/// Check that a value-shaped operand evaluates to a value of the
/// expected kind. Used by `Le` (Decimal), `DateLe` (Date), and
/// arithmetic (`Add`/`Sub`, both Decimal).
///
/// Two paths to avoid double-emission: a bare variable observes
/// directly (any conflict surfaces as `VariableKindConflict`,
/// which names the variable); anything else infers its kind and
/// emits `OperandKindMismatch` on disagreement (which names the
/// operator and the kinds).
fn check_operand_kind(
    operand: &Expr,
    expected: PredicateArgKind,
    operator: &'static str,
    env: &mut KindEnv,
    predicate_decls: &HashMap<&str, &PredicateDecl>,
    ctx: &ValidationContext,
    errors: &mut Vec<ValidationError>,
) {
    if let Expr::Term(Term::Var(name)) = operand {
        observe_or_report(env, name, InferredKind::Known(expected), ctx, errors);
        return;
    }
    let inferred = infer_value_expr(operand, env, predicate_decls, ctx, errors);
    if let InferredKind::Known(actual) = inferred
        && !kinds_compatible(expected, actual)
    {
        errors.push(ValidationError::OperandKindMismatch {
            operator,
            expected,
            actual,
            context: ctx.clone(),
        });
    }
}

/// One side of an equality check: the inferred kind, plus the
/// variable name if the operand was a bare variable (so a refined
/// kind can be written back to the env). Constructed by
/// `expr_operand` for `Eq` and `term_operand` for `Neq`.
type EqualityOperand<'a> = (InferredKind, Option<&'a str>);

/// Strict equality between two value operands. If both sides
/// produce a `Known` kind they must be compatible; when one is a
/// bare variable and the other contributes a concrete kind, the
/// variable refines to that kind. `Subject == Decimal` is a kind
/// error, never a silent coercion. Backs both `Eq` (Expr
/// operands) and `Neq` (Term operands).
fn check_equality(
    left: EqualityOperand<'_>,
    right: EqualityOperand<'_>,
    operator: &'static str,
    env: &mut KindEnv,
    ctx: &ValidationContext,
    errors: &mut Vec<ValidationError>,
) {
    let combined = match (left.0, right.0) {
        (InferredKind::Known(l), InferredKind::Known(r)) => {
            if !kinds_compatible(l, r) {
                errors.push(ValidationError::OperandKindMismatch {
                    operator,
                    expected: l,
                    actual: r,
                    context: ctx.clone(),
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
            observe_or_report(env, name, refined, ctx, errors);
        }
    }
}

fn check_equality_operands(
    left: &Expr,
    right: &Expr,
    operator: &'static str,
    env: &mut KindEnv,
    predicate_decls: &HashMap<&str, &PredicateDecl>,
    ctx: &ValidationContext,
    errors: &mut Vec<ValidationError>,
) {
    let left_op = (
        infer_value_expr(left, env, predicate_decls, ctx, errors),
        expr_var_name(left),
    );
    let right_op = (
        infer_value_expr(right, env, predicate_decls, ctx, errors),
        expr_var_name(right),
    );
    check_equality(left_op, right_op, operator, env, ctx, errors);
}

fn check_equality_terms(
    left: &Term,
    right: &Term,
    operator: &'static str,
    env: &mut KindEnv,
    ctx: &ValidationContext,
    errors: &mut Vec<ValidationError>,
) {
    check_equality(
        (resolved_term_kind(left, env), term_var_name(left)),
        (resolved_term_kind(right, env), term_var_name(right)),
        operator,
        env,
        ctx,
        errors,
    );
}

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

/// Infer the kind of a value-producing expression. Variables look
/// up via `env`; literals carry their kind directly; `Add`/`Sub`
/// recursively check Decimal operands and return Decimal; `Sum`
/// returns Decimal after a body-first walk under a shadowed
/// binding; `ValueOf` returns the declared kind of its wildcard
/// slot. A predicate-shaped expression at a value position
/// surfaces as `ExpectedValueExpression`.
fn infer_value_expr(
    expr: &Expr,
    env: &mut KindEnv,
    predicate_decls: &HashMap<&str, &PredicateDecl>,
    ctx: &ValidationContext,
    errors: &mut Vec<ValidationError>,
) -> InferredKind {
    match expr {
        Expr::Term(term) => resolved_term_kind(term, env),
        Expr::Add(left, right) | Expr::Sub(left, right) => {
            let operator = if matches!(expr, Expr::Add(_, _)) {
                "+"
            } else {
                "-"
            };
            check_operand_kind(
                left,
                PredicateArgKind::Decimal,
                operator,
                env,
                predicate_decls,
                ctx,
                errors,
            );
            check_operand_kind(
                right,
                PredicateArgKind::Decimal,
                operator,
                env,
                predicate_decls,
                ctx,
                errors,
            );
            InferredKind::Known(PredicateArgKind::Decimal)
        }
        Expr::Sum {
            value,
            binding,
            body,
        } => {
            // Body-first inference under a shadowed binding: walk
            // body so any kind observed there for `binding` (or
            // other body-bound variables) refines, then check
            // `value` (a Term) against the refined env. Sum's
            // outer result is always Decimal regardless of body.
            //
            // Shadow scopes the iteration variable only; refining
            // an outer variable used inside the body still leaks
            // through, mirroring the runtime where body sees the
            // same outer state.
            env.with_shadow(binding, |env| {
                walk_predicate_expr(body, env, predicate_decls, ctx, errors);
                let resolved = resolved_term_kind(value, env);
                if let InferredKind::Known(actual) = resolved
                    && !kinds_compatible(PredicateArgKind::Decimal, actual)
                {
                    errors.push(ValidationError::OperandKindMismatch {
                        operator: "sum",
                        expected: PredicateArgKind::Decimal,
                        actual,
                        context: ctx.clone(),
                    });
                }
            });
            InferredKind::Known(PredicateArgKind::Decimal)
        }
        Expr::ValueOf {
            predicate,
            args,
            default,
        } => {
            // Walk args as a predicate-call so kind mismatches in
            // the lookup pattern surface (subject literal in
            // decimal slot, variable conflict, etc.).
            check_claim_args(predicate, args, env, predicate_decls, ctx, errors);
            let result_kind = value_of_result_kind(predicate, args, predicate_decls);
            if let Some(default_expr) = default {
                let default_kind =
                    infer_value_expr(default_expr, env, predicate_decls, ctx, errors);
                // The runtime returns either the looked-up value
                // or the default, so a kind mismatch between them
                // is the same class of error as a comparator
                // mismatch - one branch would produce a kind the
                // caller cannot consume.
                if let (InferredKind::Known(expected), InferredKind::Known(actual)) =
                    (result_kind, default_kind)
                    && !kinds_compatible(expected, actual)
                {
                    errors.push(ValidationError::OperandKindMismatch {
                        operator: "value default",
                        expected,
                        actual,
                        context: ctx.clone(),
                    });
                }
            }
            result_kind
        }
        // Predicate-shaped expression appearing in a value-
        // demanding position. Runtime raises `NotValue`; surface
        // it earlier. The shape string is short and structural -
        // a full pretty-print would need IR-aware formatting that
        // does not yet exist outside `morpholog_core::format`.
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
            errors.push(ValidationError::ExpectedValueExpression {
                context: ctx.clone(),
                expression: short_expr_shape(expr),
            });
            InferredKind::UnknownOrAny
        }
    }
}

/// Resolve a `Term`'s kind through the env: variables look up
/// their current inferred kind; literals and `actor` return
/// their inherent kind. Wildcard stays UnknownOrAny.
fn resolved_term_kind(term: &Term, env: &KindEnv) -> InferredKind {
    match term {
        Term::Var(name) => env.lookup(name),
        other => term_kind(other),
    }
}

/// Prefer the more specific of two compatible kinds. `Any` loses
/// to a concrete kind; otherwise the kinds are equal and either
/// is fine.
fn more_specific(a: PredicateArgKind, b: PredicateArgKind) -> PredicateArgKind {
    if matches!(a, PredicateArgKind::Any) {
        b
    } else {
        a
    }
}

/// Look up the kind of the value position in a `ValueOf` lookup.
/// The wildcard position(s) in `args` mark the value slot(s);
/// returns the first wildcard's declared kind, or UnknownOrAny
/// when the predicate is undeclared or has no wildcard.
fn value_of_result_kind(
    predicate: &str,
    args: &[Term],
    predicate_decls: &HashMap<&str, &PredicateDecl>,
) -> InferredKind {
    let Some(decl) = predicate_decls.get(predicate) else {
        return InferredKind::UnknownOrAny;
    };
    args.iter()
        .position(|a| matches!(a, Term::Wildcard))
        .and_then(|p| decl.args.get(p))
        .map(|a| InferredKind::Known(a.kind))
        .unwrap_or(InferredKind::UnknownOrAny)
}

/// Short structural label for an expression used in
/// `ExpectedValueExpression`. Not a full pretty-print; just the
/// outermost constructor so the diagnostic identifies the shape
/// without committing to the formatter's exact output.
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

/// Check a predicate-call's arg list against the declared kinds.
/// For each position: a literal contributes its kind directly; a
/// variable is observed (refining the env); `Term::Wildcard` is
/// skipped (matches anything, binds nothing); `Term::Actor`
/// contributes `Subject` (its inherent kind; whether `actor` is
/// reachable in this context is a separate Layer-3 concern).
///
/// An undeclared predicate is *not* an error here - the existing
/// arity-and-declaration pass surfaces that earlier with
/// `UndeclaredPredicate`. We skip silently if the predicate is
/// unknown.
fn check_claim_args(
    predicate: &str,
    args: &[Term],
    env: &mut KindEnv,
    predicate_decls: &HashMap<&str, &PredicateDecl>,
    ctx: &ValidationContext,
    errors: &mut Vec<ValidationError>,
) {
    let Some(decl) = predicate_decls.get(predicate) else {
        return;
    };
    // Arity mismatch is also out of scope for the kind layer; the
    // existing pass emits it. Walk only as many positions as both
    // sides have.
    let n = args.len().min(decl.args.len());
    for (position, (arg, decl_arg)) in args
        .iter()
        .take(n)
        .zip(decl.args.iter().take(n))
        .enumerate()
    {
        check_one_claim_arg(predicate, position, arg, decl_arg.kind, env, ctx, errors);
    }
}

fn check_one_claim_arg(
    predicate: &str,
    position: usize,
    arg: &Term,
    expected: PredicateArgKind,
    env: &mut KindEnv,
    ctx: &ValidationContext,
    errors: &mut Vec<ValidationError>,
) {
    let actual = term_kind(arg);
    // For variables, observe (refining the env). For literals and
    // actor, just check compatibility - they carry their kind
    // directly and cannot themselves be refined.
    if let Term::Var(name) = arg {
        if let Err((previous, new)) = env.observe(name, InferredKind::Known(expected)) {
            errors.push(ValidationError::VariableKindConflict {
                variable: name.clone(),
                previous,
                new,
                context: ctx.clone(),
            });
        }
        // Variable-vs-declaration conflict (e.g. var was already
        // refined to Decimal and the current slot expects Subject)
        // already comes out as VariableKindConflict above; nothing
        // more to emit here.
    } else if let InferredKind::Known(actual_kind) = actual
        && !kinds_compatible(expected, actual_kind)
    {
        errors.push(ValidationError::PredicateArgKindMismatch {
            predicate: predicate.to_string(),
            position,
            expected,
            actual: actual_kind,
            context: ctx.clone(),
        });
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

/// Observe `name` at `kind`; on a refinement conflict, push a
/// `VariableKindConflict` carrying both kinds.
fn observe_or_report(
    env: &mut KindEnv,
    name: &str,
    kind: InferredKind,
    ctx: &ValidationContext,
    errors: &mut Vec<ValidationError>,
) {
    if let Err((previous, new)) = env.observe(name, kind) {
        errors.push(ValidationError::VariableKindConflict {
            variable: name.to_string(),
            previous,
            new,
            context: ctx.clone(),
        });
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
    // kindcheck_program: claim arg checking + statement flow
    // ============================================================

    use crate::dsl::*;
    use crate::ir::{Invariant, PredicateArgDecl, Program, Transformation};

    /// Build a `PredicateDecl` shorthand for tests.
    fn pdecl(name: &str, args: &[(&str, PredicateArgKind)]) -> crate::ir::PredicateDecl {
        crate::ir::PredicateDecl {
            name: name.to_string(),
            args: args
                .iter()
                .map(|(n, k)| PredicateArgDecl {
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
        let errs = kindcheck_program(&p);
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
        let errs = kindcheck_program(&p);
        assert_eq!(errs.len(), 1, "expected one kind error; got {errs:?}");
        match &errs[0] {
            ValidationError::PredicateArgKindMismatch {
                predicate,
                position,
                expected,
                actual,
                ..
            } => {
                assert_eq!(predicate, "Policy");
                assert_eq!(*position, 0);
                assert_eq!(*expected, PredicateArgKind::Subject);
                assert_eq!(*actual, PredicateArgKind::Decimal);
            }
            other => panic!("expected PredicateArgKindMismatch, got {other:?}"),
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
        let errs = kindcheck_program(&p);
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
        let errs = kindcheck_program(&p);
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
        let errs = kindcheck_program(&p);
        assert!(
            errs.is_empty(),
            "Any-then-Decimal should refine; got {errs:?}"
        );
    }

    #[test]
    fn actor_term_carries_subject_kind() {
        // `actor` flowing into a Decimal slot is a kind mismatch.
        let mut p = empty_program();
        p.predicates = vec![pdecl("Limit", &[("amount", PredicateArgKind::Decimal)])];
        p.invariants = vec![Invariant {
            name: "actor_in_decimal_slot".to_string(),
            version: 1,
            body: claim("Limit", vec![actor()]),
        }];
        let errs = kindcheck_program(&p);
        assert_eq!(
            errs.len(),
            1,
            "actor-in-decimal-slot must flag; got {errs:?}"
        );
        match &errs[0] {
            ValidationError::PredicateArgKindMismatch {
                expected, actual, ..
            } => {
                assert_eq!(*expected, PredicateArgKind::Decimal);
                assert_eq!(*actual, PredicateArgKind::Subject);
            }
            other => panic!("expected PredicateArgKindMismatch, got {other:?}"),
        }
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
        let errs = kindcheck_program(&p);
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
        let errs = kindcheck_program(&p);
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
        let errs = kindcheck_program(&p);
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
        let errs = kindcheck_program(&p);
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
        let errs = kindcheck_program(&p);
        assert_eq!(errs.len(), 1);
        assert!(matches!(
            errs[0],
            ValidationError::PredicateArgKindMismatch { .. }
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
        let errs = kindcheck_program(&p);
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
        let errs = kindcheck_program(&p);
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
        let errs = kindcheck_program(&p);
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
        let errs = kindcheck_program(&p);
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
        let errs = kindcheck_program(&p);
        assert_eq!(errs.len(), 1);
        assert!(matches!(
            errs[0],
            ValidationError::OperandKindMismatch { operator: "==", .. }
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
        let errs = kindcheck_program(&p);
        assert_eq!(errs.len(), 1);
        assert!(matches!(
            errs[0],
            ValidationError::OperandKindMismatch { operator: "!=", .. }
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
        let errs = kindcheck_program(&p);
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
        let errs = kindcheck_program(&p);
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
        let errs = kindcheck_program(&p);
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
        let errs = kindcheck_program(&p);
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
        let errs = kindcheck_program(&p);
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
        let errs = kindcheck_program(&p);
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
        let errs = kindcheck_program(&p);
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
        let errs = kindcheck_program(&p);
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
        let errs = kindcheck_program(&p);
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
    // Binding scoping for quantifiers (Forall, Exists, Sum)
    // ============================================================

    #[test]
    fn forall_binding_shadows_outer_variable_of_the_same_name() {
        // Outer `x` is Subject (via S(x)). The `forall x in P(x): C(x)`
        // re-uses `x` as a loop-local binding over P (Decimal). After
        // the forall, the outer `x` must still be Subject - using
        // it in a Subject slot again must NOT conflict.
        let mut p = empty_program();
        p.predicates = vec![
            pdecl("S", &[("v", PredicateArgKind::Subject)]),
            pdecl("P", &[("v", PredicateArgKind::Decimal)]),
            pdecl("C", &[("v", PredicateArgKind::Decimal)]),
        ];
        p.invariants = vec![Invariant {
            name: "forall_shadowing".to_string(),
            version: 1,
            body: and(vec![
                claim("S", vec![var("x")]),
                forall("x", claim("P", vec![var("x")]), claim("C", vec![var("x")])),
                claim("S", vec![var("x")]),
            ]),
        }];
        let errs = kindcheck_program(&p);
        assert!(
            errs.is_empty(),
            "forall binding must shadow outer x; got {errs:?}"
        );
    }

    #[test]
    fn exists_binding_shadows_outer_variable_of_the_same_name() {
        // Same pattern as forall but with exists.
        let mut p = empty_program();
        p.predicates = vec![
            pdecl("S", &[("v", PredicateArgKind::Subject)]),
            pdecl("D", &[("v", PredicateArgKind::Decimal)]),
        ];
        p.invariants = vec![Invariant {
            name: "exists_shadowing".to_string(),
            version: 1,
            body: and(vec![
                claim("S", vec![var("x")]),
                exists("x", claim("D", vec![var("x")])),
                claim("S", vec![var("x")]),
            ]),
        }];
        let errs = kindcheck_program(&p);
        assert!(
            errs.is_empty(),
            "exists binding must shadow outer x; got {errs:?}"
        );
    }

    #[test]
    fn sum_binding_shadows_outer_variable_of_the_same_name() {
        // The Sum scoping test we already have proves body
        // bindings don't leak, but didn't pin shadowing of an
        // outer variable of the same name. Pin it.
        let mut p = empty_program();
        p.predicates = vec![
            pdecl("S", &[("v", PredicateArgKind::Subject)]),
            pdecl("P", &[("v", PredicateArgKind::Decimal)]),
        ];
        p.invariants = vec![Invariant {
            name: "sum_shadowing".to_string(),
            version: 1,
            body: and(vec![
                claim("S", vec![var("amount")]),
                le(
                    sum(var("amount"), "amount", claim("P", vec![var("amount")])),
                    term(dec("100")),
                ),
                claim("S", vec![var("amount")]),
            ]),
        }];
        let errs = kindcheck_program(&p);
        assert!(
            errs.is_empty(),
            "sum binding must shadow outer amount; got {errs:?}"
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
        let errs = kindcheck_program(&p);
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
