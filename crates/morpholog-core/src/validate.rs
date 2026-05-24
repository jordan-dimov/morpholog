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

use crate::ir::{PredicateArgKind, Program};
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
    /// The mirror of `ExpectedValueExpression`: a value-producing
    /// expression (a bare term, `+`, `-`, `sum`, `value`) appeared
    /// where a predicate-shaped expression was required - a `require`
    /// body, an invariant, a quantifier body, a comparator that
    /// matches state. The runtime surfaces this as
    /// `EvalError::NotPredicate`; the check surfaces it earlier.
    ExpectedPredicateExpression {
        context: ValidationContext,
        expression: String,
    },
    /// `actor` was referenced in an invariant or derived-claim
    /// body, where no proposing transition is in scope. The kernel
    /// raises `EvalError::UnboundActor` for this at evaluation
    /// time; the check surfaces it earlier. `actor` resolves only
    /// inside transformation bodies - authority checks belong in a
    /// `require`, not an invariant.
    ActorNotAvailable { context: ValidationContext },
    /// A variable was used in a position that demands a bound value
    /// (an `admit`/`retract`/`emit` argument, a comparator or
    /// arithmetic operand, a `value` lookup key, a `sum` target)
    /// without anything having bound it first. The binding rules
    /// follow the runtime: parameters, `bind`, `let`, `for`, and
    /// claim matches inside a `require`/invariant bind names;
    /// `require` does not export its matches to later statements.
    /// The kernel raises `EvalError::UnboundVariable` for this at
    /// evaluation time.
    UnboundVariable {
        variable: String,
        context: ValidationContext,
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
            ValidationError::ExpectedPredicateExpression {
                context,
                expression,
            } => write!(
                f,
                "expected a predicate-shaped expression but found value-producing \
                 `{expression}` in {context}"
            ),
            ValidationError::ActorNotAvailable { context } => write!(
                f,
                "`actor` is not available in {context}; it resolves only inside \
                 transformation bodies, so authority checks belong in a `require`"
            ),
            ValidationError::UnboundVariable { variable, context } => write!(
                f,
                "variable `{variable}` is used in {context} but nothing binds it; \
                 a `require` match does not export its bindings to later statements"
            ),
        }
    }
}

impl std::error::Error for ValidationError {}

/// Strict programme validation. Two contributions merge into one
/// `Vec<ValidationError>`: the name-level duplicate-declaration
/// check below, and the single-traversal static check in
/// [`crate::check::check_program`] (declared references, arity,
/// kind compatibility). Called via [`Program::validate`].
///
/// Duplicate detection stays here because it is not a tree walk -
/// it compares declaration names, not references. Everything that
/// *is* a tree walk (declared/arity/kind at each reference) lives
/// in the one `check` visitor, so a faulty programme sees the full
/// work list from a single pass over its bodies.
pub(crate) fn validate_program(p: &Program) -> Result<(), Vec<ValidationError>> {
    let mut errors = collect_duplicate_decl_errors(p);
    errors.extend(crate::check::check_program(p));
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Name-level duplicate-declaration check across both vocabularies.
/// Not a tree walk: it compares declaration names, so it has no
/// place in the reference-visiting `check` pass.
fn collect_duplicate_decl_errors(p: &Program) -> Vec<ValidationError> {
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

    errors
}
