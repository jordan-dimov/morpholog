//! Programme-level structural validation: every predicate referenced in
//! any transformation body, invariant body, or derived-claim shape
//! must be declared in [`Program::predicates`], every intent referenced
//! in any `emit` statement must be declared in [`Program::intents`], and
//! every reference must match the declared arity.
//!
//! Called via [`crate::Program::validate`]. Strict mode: undeclared
//! predicates and intents are errors, not passthrough. The validator
//! collects every error rather than failing on the first; a migration
//! that adds declarations should see the full work list at once.

use crate::ir::{Expr, PredicateArgKind, Program, Stmt};
use std::collections::HashMap;

/// Which declared vocabulary a validation error refers to. Predicates
/// and intents share four diagnostic shapes (undeclared reference,
/// arity mismatch, duplicate declaration, arg-kind mismatch); the
/// `vocabulary` field disambiguates them in the rendered message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VocabularyKind {
    Predicate,
    Intent,
}

impl std::fmt::Display for VocabularyKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VocabularyKind::Predicate => write!(f, "predicate"),
            VocabularyKind::Intent => write!(f, "intent"),
        }
    }
}

/// Where in a programme a validation error was found. Reported alongside
/// every [`ValidationError`] so migrations can find the right call site
/// without trawling the whole programme.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationContext {
    Invariant { name: String },
    Transformation { name: String },
    DerivedClaim { predicate: String },
}

/// A single failure surfaced by [`Program::validate`]. The validator
/// collects every error rather than failing fast, so a programme
/// migration that adds declarations sees the full work list rather
/// than fixing one site, re-running, and discovering the next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// A predicate or intent referenced somewhere in the programme
    /// is not declared. Strict mode: every reference must have a
    /// declaration.
    Undeclared {
        vocabulary: VocabularyKind,
        name: String,
        context: ValidationContext,
    },
    /// A predicate or intent reference passes a different number of
    /// arguments than the declaration calls for.
    ArityMismatch {
        vocabulary: VocabularyKind,
        name: String,
        expected: usize,
        actual: usize,
        context: ValidationContext,
    },
    /// Two declarations in the same vocabulary share a name. Even
    /// if both agree on arity, the duplicate is a modelling bug.
    DuplicateDecl {
        vocabulary: VocabularyKind,
        name: String,
    },
    /// A predicate-call or intent-emit argument does not match the
    /// kind declared for that position. Surfaces things like
    /// `Policy(amount, 100)` where the first position is declared
    /// `Subject`, or a date literal flowing into a `Decimal` slot.
    ArgKindMismatch {
        vocabulary: VocabularyKind,
        name: String,
        position: usize,
        expected: PredicateArgKind,
        actual: PredicateArgKind,
        context: ValidationContext,
    },
    /// An operator (comparator, arithmetic, `sum`, `for`, `in`,
    /// `value default`) received an operand of the wrong kind.
    /// `Le(date, decimal)`, `Add(subject, decimal)`,
    /// `For` over a Decimal value - the kernel raises these as
    /// `EvalError::TypeMismatch` at runtime; this validator
    /// surfaces them at authoring time.
    OperandKindMismatch {
        operator: &'static str,
        expected: PredicateArgKind,
        actual: PredicateArgKind,
        context: ValidationContext,
    },
    /// An equality (`==` or `!=`) had two operands of distinct,
    /// incompatible kinds. Symmetric by nature: there is no
    /// "expected" side - both kinds are equally constrained by the
    /// other. `Subject == Decimal` is a kind error, not a silent
    /// coercion to false.
    EqualityKindMismatch {
        operator: &'static str,
        left: PredicateArgKind,
        right: PredicateArgKind,
        context: ValidationContext,
    },
    /// A variable was bound at one kind and then used at a different
    /// kind that is not compatible with the first. `amount` bound
    /// from a `Decimal` slot and then used in a `Subject` slot is
    /// the canonical case.
    VariableKindConflict {
        variable: String,
        previous: PredicateArgKind,
        new: PredicateArgKind,
        context: ValidationContext,
    },
    /// An expression that the kind-checker treats as value-producing
    /// (operand of arithmetic, comparator right-hand side, `Sum`'s
    /// target term) appeared as a predicate-shaped expression - one
    /// that produces binding witnesses rather than a value. The
    /// runtime would surface this as `EvalError::NotValue`; kind-
    /// check surfaces it earlier.
    ExpectedValueExpression {
        context: ValidationContext,
        expression: String,
    },
}

impl std::fmt::Display for ValidationContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationContext::Invariant { name } => write!(f, "invariant `{name}`"),
            ValidationContext::Transformation { name } => write!(f, "transformation `{name}`"),
            ValidationContext::DerivedClaim { predicate } => {
                write!(f, "derived claim `{predicate}`")
            }
        }
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationError::Undeclared {
                vocabulary,
                name,
                context,
            } => write!(
                f,
                "undeclared {vocabulary} `{name}` referenced in {context}"
            ),
            ValidationError::ArityMismatch {
                vocabulary,
                name,
                expected,
                actual,
                context,
            } => write!(
                f,
                "{vocabulary} `{name}` declared with arity {expected} \
                 but referenced with {actual} args in {context}"
            ),
            ValidationError::DuplicateDecl { vocabulary, name } => {
                write!(f, "duplicate {vocabulary} declaration for `{name}`")
            }
            ValidationError::ArgKindMismatch {
                vocabulary,
                name,
                position,
                expected,
                actual,
                context,
            } => write!(
                f,
                "{vocabulary} `{name}` arg #{position} expects {expected:?} but \
                 received {actual:?} in {context}"
            ),
            ValidationError::OperandKindMismatch {
                operator,
                expected,
                actual,
                context,
            } => write!(
                f,
                "{operator} expects {expected:?} operand(s) but received {actual:?} in {context}"
            ),
            ValidationError::EqualityKindMismatch {
                operator,
                left,
                right,
                context,
            } => write!(
                f,
                "{operator} operands must have the same kind; got {left:?} vs {right:?} in {context}"
            ),
            ValidationError::VariableKindConflict {
                variable,
                previous,
                new,
                context,
            } => write!(
                f,
                "variable `{variable}` was first constrained to {previous:?} but later \
                 used as {new:?} in {context}"
            ),
            ValidationError::ExpectedValueExpression {
                context,
                expression,
            } => write!(
                f,
                "expected a value-producing expression but found predicate-shaped \
                 `{expression}` in {context}"
            ),
        }
    }
}

impl std::error::Error for ValidationError {}

/// Strict programme validation. Merges the structural pass below
/// with [`crate::kindcheck::kindcheck_program`] into a single
/// `Vec<ValidationError>`. Called via [`Program::validate`].
///
/// Both layers contribute to the same error list; a faulty
/// programme sees the full work list rather than fixing one layer
/// and re-running to discover the next. The kind checker is
/// defensive against arity-mismatched sites (walks `min(args, decl)`)
/// so an arity error and a kind error in the same expression both
/// surface in one run.
pub(crate) fn validate_program(p: &Program) -> Result<(), Vec<ValidationError>> {
    let mut errors = collect_structural_errors(p);
    errors.extend(crate::kindcheck::kindcheck_program(p));
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// The original arity-and-declaration pass: undeclared predicate
/// references, arity mismatches, duplicate declarations.
fn collect_structural_errors(p: &Program) -> Vec<ValidationError> {
    let mut errors = Vec::new();

    // 1. Duplicate predicate declarations. Counts must be collected
    //    via HashMap for O(1) lookups, but the error emission order
    //    must be deterministic (HashMap iteration is randomised, and
    //    the workspace-wide validation test's panic output would
    //    otherwise vary run-to-run). Collect duplicates into a Vec,
    //    sort by name, then emit.
    let mut seen = HashMap::<&str, usize>::new();
    for decl in &p.predicates {
        *seen.entry(decl.name.as_str()).or_insert(0) += 1;
    }
    let mut duplicates: Vec<&str> = seen
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(name, _)| *name)
        .collect();
    duplicates.sort();
    for name in duplicates {
        errors.push(ValidationError::DuplicateDecl {
            vocabulary: VocabularyKind::Predicate,
            name: name.to_string(),
        });
    }

    // Same duplicate check for intents - separate namespace.
    let mut seen_intents = HashMap::<&str, usize>::new();
    for decl in &p.intents {
        *seen_intents.entry(decl.name.as_str()).or_insert(0) += 1;
    }
    let mut dup_intents: Vec<&str> = seen_intents
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(name, _)| *name)
        .collect();
    dup_intents.sort();
    for name in dup_intents {
        errors.push(ValidationError::DuplicateDecl {
            vocabulary: VocabularyKind::Intent,
            name: name.to_string(),
        });
    }

    // Build name -> arity lookups for both vocabularies. Predicate
    // and intent namespaces are separate; a name appearing in both
    // is unusual but lawful.
    let arities: HashMap<&str, usize> = p
        .predicates
        .iter()
        .map(|d| (d.name.as_str(), d.args.len()))
        .collect();
    let intent_arities: HashMap<&str, usize> = p
        .intents
        .iter()
        .map(|d| (d.name.as_str(), d.args.len()))
        .collect();

    // 2. Walk each invariant body.
    for inv in &p.invariants {
        let ctx = ValidationContext::Invariant {
            name: inv.name.clone(),
        };
        validate_expr(&inv.body, &arities, &ctx, &mut errors);
    }

    // 3. Walk each transformation body.
    for t in &p.transformations {
        let ctx = ValidationContext::Transformation {
            name: t.name.clone(),
        };
        for stmt in &t.body {
            validate_stmt(stmt, &arities, &intent_arities, &ctx, &mut errors);
        }
    }

    // 4. Walk each derived claim: output predicate declared, output
    //    arity matches keys + values count, domain references validated.
    for d in &p.derived_claims {
        let ctx = ValidationContext::DerivedClaim {
            predicate: d.predicate.clone(),
        };
        match arities.get(d.predicate.as_str()) {
            None => errors.push(ValidationError::Undeclared {
                vocabulary: VocabularyKind::Predicate,
                name: d.predicate.clone(),
                context: ctx.clone(),
            }),
            Some(&decl_arity) => {
                let actual_arity = d.keys.len() + d.values.len();
                if decl_arity != actual_arity {
                    errors.push(ValidationError::ArityMismatch {
                        vocabulary: VocabularyKind::Predicate,
                        name: d.predicate.clone(),
                        expected: decl_arity,
                        actual: actual_arity,
                        context: ctx.clone(),
                    });
                }
            }
        }
        validate_expr(&d.domain, &arities, &ctx, &mut errors);
        for v in &d.values {
            validate_expr(&v.expr, &arities, &ctx, &mut errors);
        }
    }

    errors
}

/// Walk a statement and collect arity/declaration errors.
pub(crate) fn validate_stmt(
    stmt: &Stmt,
    arities: &HashMap<&str, usize>,
    intent_arities: &HashMap<&str, usize>,
    ctx: &ValidationContext,
    errors: &mut Vec<ValidationError>,
) {
    match stmt {
        Stmt::Require(e) | Stmt::BindOne(e) => validate_expr(e, arities, ctx, errors),
        Stmt::Let { value, .. } => validate_expr(value, arities, ctx, errors),
        Stmt::LetNewSubject { .. } => {}
        Stmt::Assert(c) => {
            check_declared(
                VocabularyKind::Predicate,
                &c.predicate,
                c.args.len(),
                arities,
                ctx,
                errors,
            );
        }
        Stmt::Retract { predicate, args } => {
            check_declared(
                VocabularyKind::Predicate,
                predicate,
                args.len(),
                arities,
                ctx,
                errors,
            );
        }
        Stmt::For {
            collection, body, ..
        } => {
            validate_expr(collection, arities, ctx, errors);
            for inner in body {
                validate_stmt(inner, arities, intent_arities, ctx, errors);
            }
        }
        Stmt::Emit(intent) => {
            check_declared(
                VocabularyKind::Intent,
                &intent.name,
                intent.args.len(),
                intent_arities,
                ctx,
                errors,
            );
        }
    }
}

/// Walk an expression and collect arity/declaration errors.
pub(crate) fn validate_expr(
    expr: &Expr,
    arities: &HashMap<&str, usize>,
    ctx: &ValidationContext,
    errors: &mut Vec<ValidationError>,
) {
    match expr {
        Expr::Claim { predicate, args } => {
            check_declared(
                VocabularyKind::Predicate,
                predicate,
                args.len(),
                arities,
                ctx,
                errors,
            );
        }
        Expr::ValueOf {
            predicate,
            args,
            default,
        } => {
            check_declared(
                VocabularyKind::Predicate,
                predicate,
                args.len(),
                arities,
                ctx,
                errors,
            );
            if let Some(d) = default {
                validate_expr(d, arities, ctx, errors);
            }
        }
        Expr::Implies { left, right } => {
            validate_expr(left, arities, ctx, errors);
            validate_expr(right, arities, ctx, errors);
        }
        Expr::And(exprs) | Expr::Or(exprs) => {
            for e in exprs {
                validate_expr(e, arities, ctx, errors);
            }
        }
        Expr::Not(e) | Expr::Exists { body: e, .. } | Expr::Pre(e) => {
            validate_expr(e, arities, ctx, errors);
        }
        Expr::Eq(l, r)
        | Expr::Le(l, r)
        | Expr::DateLe(l, r)
        | Expr::Sub(l, r)
        | Expr::Add(l, r) => {
            validate_expr(l, arities, ctx, errors);
            validate_expr(r, arities, ctx, errors);
        }
        Expr::Sum { body, .. } => {
            validate_expr(body, arities, ctx, errors);
        }
        Expr::Forall { source, body, .. } => {
            validate_expr(source, arities, ctx, errors);
            validate_expr(body, arities, ctx, errors);
        }
        Expr::Neq(_, _) | Expr::Term(_) | Expr::In(_, _) => {
            // No predicate references; operate on Terms only.
        }
    }
}

/// Helper: emit the right error variant for a vocabulary call/emit site.
pub(crate) fn check_declared(
    vocabulary: VocabularyKind,
    name: &str,
    actual: usize,
    arities: &HashMap<&str, usize>,
    ctx: &ValidationContext,
    errors: &mut Vec<ValidationError>,
) {
    match arities.get(name) {
        None => errors.push(ValidationError::Undeclared {
            vocabulary,
            name: name.to_string(),
            context: ctx.clone(),
        }),
        Some(&expected) if expected != actual => {
            errors.push(ValidationError::ArityMismatch {
                vocabulary,
                name: name.to_string(),
                expected,
                actual,
                context: ctx.clone(),
            });
        }
        Some(_) => {}
    }
}
