//! Programme-level validation. Owns the [`ValidationError`] vocabulary
//! and orchestrates three contributions into one error list: a
//! nesting-depth guard (run first, so the recursive walks it protects
//! cannot overflow on the input that would trip it), a name-level
//! duplicate-declaration pass, and the single static-check traversal in
//! [`crate::check`] (declarations and arity for both predicate and
//! intent vocabularies, kind/type compatibility, binding flow, and
//! actor context).
//!
//! Called via [`crate::Program::validate`]. Strict mode: undeclared
//! predicates and intents are errors, not passthrough. The validator
//! collects every error rather than failing on the first; a migration
//! that adds declarations should see the full work list at once.

use crate::ir::{DefinitionName, PredicateArgKind, Program, Prop, Stmt, ValueExpr};
use std::collections::HashMap;

/// Proof-of-validity handle: a reference to a [`Program`] that has
/// been run through [`Program::validate`] and survived. Constructed
/// via [`Program::validated`] (which fails if validation reports
/// errors); the only way to obtain one is to go through that gate.
///
/// Why a separate type instead of a documented contract
/// ([`Program::validate`] alone): the analysis surface
/// ([`crate::transformation_param_kinds`],
/// [`crate::transformation_arg_schema`]) is only meaningful over a
/// validated programme, since the kind inference these accessors
/// depend on observes kinds the runtime would itself refuse to
/// admit if validation has not passed. Taking `&ValidatedProgram`
/// rather than `&Program` makes the precondition load-bearing at
/// the type level: a caller cannot accidentally analyse an
/// unvalidated programme, and the analysis layer no longer needs to
/// defensively re-validate. The defensive re-validation also meant
/// every CLI invocation paid the validation cost twice; the
/// newtype removes that.
///
/// `Copy` because it wraps a single reference - passing it around
/// has the same cost as passing `&Program`.
#[derive(Debug, Clone, Copy)]
pub struct ValidatedProgram<'a>(&'a Program);

impl<'a> ValidatedProgram<'a> {
    /// Borrow the underlying programme. Used by callers that want
    /// to read non-analysis fields (predicate declarations,
    /// invariants, transformation bodies) without re-deriving the
    /// validation guarantee.
    pub fn as_program(&self) -> &'a Program {
        self.0
    }

    /// Used internally by [`Program::validated`] to assemble the
    /// handle after successful validation. Pub(crate) because the
    /// only path callers should reach this through is the
    /// `validated()` gate.
    pub(crate) fn from_validated(program: &'a Program) -> Self {
        Self(program)
    }
}

/// Which declared vocabulary a validation error refers to. Predicates
/// and intents share four diagnostic shapes (undeclared reference,
/// arity mismatch, duplicate declaration, arg-kind mismatch); the
/// `vocabulary` field disambiguates them in the rendered message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VocabularyKind {
    Predicate,
    Intent,
    Definition,
}

impl std::fmt::Display for VocabularyKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VocabularyKind::Predicate => write!(f, "predicate"),
            VocabularyKind::Intent => write!(f, "intent"),
            VocabularyKind::Definition => write!(f, "definition"),
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
    Definition { name: String },
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
    /// An arithmetic operator was applied to a pair of known kinds for
    /// which no rule exists (e.g. adding two timestamps, or multiplying
    /// durations). The rule matrix is deliberately small: decimals
    /// support every operator; instants shift by durations and
    /// difference into durations; durations add, subtract, and cap.
    NoArithRule {
        operator: &'static str,
        left: PredicateArgKind,
        right: PredicateArgKind,
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
    /// `actor` was referenced in an invariant or derived-claim
    /// body, where no proposing transition is in scope. The kernel
    /// raises `EvalError::UnboundActor` for this at evaluation
    /// time; the check surfaces it earlier. `actor` resolves only
    /// inside transformation bodies - authority checks belong in a
    /// `require`, not an invariant.
    ActorNotAvailable { context: ValidationContext },
    /// A body in this context nests deeper than the validator's fixed
    /// maximum depth. The recursive evaluator and check walk descend
    /// one stack frame per nesting level, so a pathologically deep
    /// expression or `for`-statement chain would exhaust the stack
    /// during `propose`. Validation rejects it first, which is why
    /// untrusted IR must be validated before it is proposed.
    NestingTooDeep { context: ValidationContext },
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
    /// A definition shares a name with a predicate. The two vocabularies
    /// share the claim-shaped reference namespace in body position - a
    /// reference `name(args)` resolves to exactly one of them - so a
    /// collision would let adding a definition silently change what
    /// existing text means.
    DefinitionNameCollision { name: String },
    /// Definitions reference each other in a cycle. A definition is a
    /// named proposition expanded at evaluation; a cycle would never
    /// terminate. `names` carries one cycle's members in sorted order.
    DefinitionCycle { names: Vec<String> },
    /// A `Prop::Claim` references a name that is declared as a
    /// definition. The parser resolves claim-shaped references against
    /// the definitions table; hand-built IR must construct
    /// `Prop::Defined` (via `ir_builder::defined`) or run
    /// [`crate::resolve_defined_calls`] before validating.
    UnresolvedDefinitionCall {
        name: String,
        context: ValidationContext,
    },
    /// A definition parameter does not appear in the definition body in
    /// any binding-capable position, so a call could never give it a
    /// value and projection would be partial.
    ParameterNotBound {
        definition: String,
        parameter: String,
    },
    /// `pre(...)` was used inside a definition body. Definitions are
    /// context-free so a call means the same thing in a gate as in an
    /// invariant; a body that read pre-state would break that. Wrap the
    /// *call* in `pre(...)` instead - the context swap applies to the
    /// body's evaluation.
    PreNotAvailable { context: ValidationContext },
}

impl std::fmt::Display for ValidationContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationContext::Invariant { name } => write!(f, "invariant `{name}`"),
            ValidationContext::Transformation { name } => write!(f, "transformation `{name}`"),
            ValidationContext::DerivedClaim { predicate } => {
                write!(f, "derived claim `{predicate}`")
            }
            ValidationContext::Definition { name } => write!(f, "definition `{name}`"),
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
                "{vocabulary} `{name}` arg #{position} expects {expected} but \
                 received {actual} in {context}"
            ),
            ValidationError::OperandKindMismatch {
                operator,
                expected,
                actual,
                context,
            } => write!(
                f,
                "{operator} expects {expected} operand(s) but received {actual} in {context}"
            ),
            ValidationError::NoArithRule {
                operator,
                left,
                right,
                context,
            } => write!(
                f,
                "no arithmetic rule for {left} {operator} {right} in {context}"
            ),
            ValidationError::EqualityKindMismatch {
                operator,
                left,
                right,
                context,
            } => write!(
                f,
                "{operator} operands must have the same kind; got {left} vs {right} in {context}"
            ),
            ValidationError::VariableKindConflict {
                variable,
                previous,
                new,
                context,
            } => write!(
                f,
                "variable `{variable}` was first constrained to {previous} but later \
                 used as {new} in {context}"
            ),
            ValidationError::ActorNotAvailable { context } => write!(
                f,
                "`actor` is not available in {context}; it resolves only inside \
                 transformation bodies, so authority checks belong in a `require`"
            ),
            ValidationError::NestingTooDeep { context } => write!(
                f,
                "nesting in {context} exceeds the maximum depth of {MAX_EXPR_DEPTH}"
            ),
            ValidationError::UnboundVariable { variable, context } => write!(
                f,
                "variable `{variable}` is used in {context} but nothing binds it; \
                 a `require` match does not export its bindings to later statements"
            ),
            ValidationError::DefinitionNameCollision { name } => write!(
                f,
                "definition `{name}` collides with predicate `{name}`; the two share \
                 the reference namespace in rule bodies, so a reference could mean \
                 either - rename one"
            ),
            ValidationError::DefinitionCycle { names } => write!(
                f,
                "definitions reference each other in a cycle ({}); a definition \
                 must expand to claims and conditions, never back to itself",
                names.join(", ")
            ),
            ValidationError::UnresolvedDefinitionCall { name, context } => write!(
                f,
                "`{name}` is a definition but is referenced as a claim in {context}; \
                 construct the call as `Prop::Defined` (the parser does this; \
                 hand-built IR uses `ir_builder::defined` or `resolve_defined_calls`)"
            ),
            ValidationError::ParameterNotBound {
                definition,
                parameter,
            } => write!(
                f,
                "parameter `{parameter}` of definition `{definition}` is not \
                 constrained by the definition body; remove it or reference it \
                 in a condition"
            ),
            ValidationError::PreNotAvailable { context } => write!(
                f,
                "pre(...) is used in {context}, but definitions are context-free \
                 and carry no pre-state; wrap the call in pre(...) at the use \
                 site instead"
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
    // Depth guard runs first and short-circuits. The duplicate pass is
    // harmless, but `check::check_program` recurses over every body, so
    // a body deep enough to overflow that walk has to be rejected
    // before it runs. A programme this malformed gets the depth errors
    // alone, not a fuller work list - there is nothing useful to add.
    // The definition reference graph must be acyclic before anything
    // walks through calls: the depth budget charges a call its callee's
    // expanded depth, and evaluation would recurse forever on a cycle.
    // A cyclic programme gets the cycle (and duplicate) errors alone -
    // nothing else is well-defined until the graph is sound.
    let order = match crate::definitions::definition_topo_order(&p.definitions) {
        Ok(order) => order,
        Err(names) => {
            let mut errors = collect_duplicate_decl_errors(p);
            errors.push(ValidationError::DefinitionCycle { names });
            return Err(errors);
        }
    };

    // Expanded depth per definition, callees before callers, so a chain
    // of definitions cannot multiply nesting past the budget while each
    // body looks shallow. A body that itself exceeds the budget errors
    // here and gets no entry; the short-circuit below keeps later walks
    // off it.
    let mut definition_depths: HashMap<DefinitionName, usize> = HashMap::new();
    let mut depth_errors = Vec::new();
    for i in order {
        let def = &p.definitions[i];
        match prop_depth_capped(&def.body, MAX_EXPR_DEPTH, &definition_depths) {
            Some(d) => {
                definition_depths.insert(def.name.clone(), d);
            }
            None => depth_errors.push(ValidationError::NestingTooDeep {
                context: ValidationContext::Definition {
                    name: def.name.to_string(),
                },
            }),
        }
    }
    depth_errors.extend(collect_depth_errors(p, &definition_depths));
    if !depth_errors.is_empty() {
        return Err(depth_errors);
    }
    let mut errors = collect_duplicate_decl_errors(p);
    errors.extend(crate::check::check_program(p));
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Exact nesting depth of `prop` (at least 1), with definition calls
/// charged at their callee's expanded depth, or `None` once the depth
/// exceeds `budget`. Bails the instant the budget runs out, so its own
/// recursion is bounded by the budget it enforces.
fn prop_depth_capped(
    prop: &Prop,
    budget: usize,
    depths: &HashMap<DefinitionName, usize>,
) -> Option<usize> {
    let inner = budget.checked_sub(1)?;
    let below = match prop {
        Prop::Claim { .. } | Prop::In(_, _) => 0,
        // A call expands to its callee's body. An unknown name charges
        // nothing here - the dangling reference is the check pass's
        // error, not a depth question.
        Prop::Defined { name, .. } => depths.get(name).copied().unwrap_or(0),
        Prop::And(items) | Prop::Or(items) => items.iter().try_fold(0usize, |acc, p| {
            Some(acc.max(prop_depth_capped(p, inner, depths)?))
        })?,
        Prop::Not(p) | Prop::Pre(p) | Prop::Exists { body: p, .. } => {
            prop_depth_capped(p, inner, depths)?
        }
        Prop::Implies { left, right } => {
            prop_depth_capped(left, inner, depths)?.max(prop_depth_capped(right, inner, depths)?)
        }
        Prop::Xor(left, right) => {
            prop_depth_capped(&crate::eval::lower_xor(left, right), inner, depths)?
        }
        Prop::Eq(left, right) | Prop::Neq(left, right) | Prop::Compare { left, right, .. } => {
            value_depth_capped(left, inner, depths)?.max(value_depth_capped(right, inner, depths)?)
        }
        Prop::Forall { source, body, .. } => {
            prop_depth_capped(source, inner, depths)?.max(prop_depth_capped(body, inner, depths)?)
        }
    };
    let total = below + 1;
    (total <= budget).then_some(total)
}

/// Value-sort companion to [`prop_depth_capped`].
fn value_depth_capped(
    expr: &ValueExpr,
    budget: usize,
    depths: &HashMap<DefinitionName, usize>,
) -> Option<usize> {
    let inner = budget.checked_sub(1)?;
    let below = match expr {
        ValueExpr::Term(_) => 0,
        ValueExpr::Arith { left, right, .. } => {
            value_depth_capped(left, inner, depths)?.max(value_depth_capped(right, inner, depths)?)
        }
        ValueExpr::Sum { body, .. } => prop_depth_capped(body, inner, depths)?,
        ValueExpr::ValueOf { default, .. } => match default.as_deref() {
            Some(d) => value_depth_capped(d, inner, depths)?,
            None => 0,
        },
    };
    let total = below + 1;
    (total <= budget).then_some(total)
}

/// Maximum expression / nested-statement depth accepted by
/// [`Program::validate`]. The recursive evaluator (`find_matches`,
/// `eval_value`) and the recursive check walk both descend one stack
/// frame per nesting level; a pathologically deep body would exhaust
/// the stack before any invariant ran. Validation rejects it first, so
/// `propose` never recurses on untrusted IR that would overflow.
/// Generous for hand-authored programmes, far below a default stack's
/// frame budget.
pub(crate) const MAX_EXPR_DEPTH: usize = 256;

/// Collect a [`ValidationError::NestingTooDeep`] for every body that
/// nests past [`MAX_EXPR_DEPTH`]: invariant bodies, transformation
/// statement bodies (expressions and nested `for`s), and derived-claim
/// domains and value expressions.
fn collect_depth_errors(
    p: &Program,
    depths: &HashMap<DefinitionName, usize>,
) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    for inv in &p.invariants {
        if prop_exceeds_depth(&inv.body, MAX_EXPR_DEPTH, depths) {
            errors.push(ValidationError::NestingTooDeep {
                context: ValidationContext::Invariant {
                    name: inv.name.to_string(),
                },
            });
        }
    }
    for t in &p.transformations {
        if t.body
            .iter()
            .any(|s| stmt_exceeds_depth(s, MAX_EXPR_DEPTH, depths))
        {
            errors.push(ValidationError::NestingTooDeep {
                context: ValidationContext::Transformation {
                    name: t.name.to_string(),
                },
            });
        }
    }
    for d in &p.derived_claims {
        let too_deep = prop_exceeds_depth(&d.domain, MAX_EXPR_DEPTH, depths)
            || d.values
                .iter()
                .any(|v| value_exceeds_depth(&v.expr, MAX_EXPR_DEPTH, depths));
        if too_deep {
            errors.push(ValidationError::NestingTooDeep {
                context: ValidationContext::DerivedClaim {
                    predicate: d.predicate.to_string(),
                },
            });
        }
    }
    errors
}

/// True if `prop` nests deeper than `budget` levels. Spends one unit
/// of budget per level and bails the instant it runs out, so its own
/// recursion is bounded by `budget` - it cannot overflow on the very
/// input it exists to reject. Crosses into [`value_exceeds_depth`] for
/// comparator operands, since the sorts are mutually recursive.
fn prop_exceeds_depth(prop: &Prop, budget: usize, depths: &HashMap<DefinitionName, usize>) -> bool {
    let Some(budget) = budget.checked_sub(1) else {
        return true;
    };
    match prop {
        Prop::Claim { .. } | Prop::In(_, _) => false,
        // A call expands to its callee's body, so it charges the
        // callee's expanded depth (computed callees-first in
        // `validate_program`); an unknown name charges nothing, the
        // dangling reference being the check pass's error.
        Prop::Defined { name, .. } => depths.get(name).copied().unwrap_or(0) > budget,
        Prop::And(items) | Prop::Or(items) => {
            items.iter().any(|p| prop_exceeds_depth(p, budget, depths))
        }
        Prop::Not(inner) | Prop::Pre(inner) | Prop::Exists { body: inner, .. } => {
            prop_exceeds_depth(inner, budget, depths)
        }
        Prop::Implies { left, right } => {
            prop_exceeds_depth(left, budget, depths) || prop_exceeds_depth(right, budget, depths)
        }
        // Xor is evaluated by lowering to `(a or b) and not (a and b)`,
        // which nests deeper than the one binary node. Measure that
        // lowered shape - the same definition eval uses - so a deep xor
        // chain cannot pass the depth guard and then overflow eval.
        Prop::Xor(left, right) => {
            prop_exceeds_depth(&crate::eval::lower_xor(left, right), budget, depths)
        }
        Prop::Eq(left, right) | Prop::Neq(left, right) | Prop::Compare { left, right, .. } => {
            value_exceeds_depth(left, budget, depths) || value_exceeds_depth(right, budget, depths)
        }
        Prop::Forall { source, body, .. } => {
            prop_exceeds_depth(source, budget, depths) || prop_exceeds_depth(body, budget, depths)
        }
    }
}

/// True if `expr` nests deeper than `budget` levels. The value-sort
/// companion to [`prop_exceeds_depth`]; the two recurse into each other
/// (`Sum`'s body is a `Prop`).
fn value_exceeds_depth(
    expr: &ValueExpr,
    budget: usize,
    depths: &HashMap<DefinitionName, usize>,
) -> bool {
    let Some(budget) = budget.checked_sub(1) else {
        return true;
    };
    match expr {
        ValueExpr::Term(_) => false,
        ValueExpr::Arith { left, right, .. } => {
            value_exceeds_depth(left, budget, depths) || value_exceeds_depth(right, budget, depths)
        }
        ValueExpr::Sum { body, .. } => prop_exceeds_depth(body, budget, depths),
        ValueExpr::ValueOf { default, .. } => default
            .as_deref()
            .is_some_and(|d| value_exceeds_depth(d, budget, depths)),
    }
}

/// True if `stmt` nests deeper than `budget` levels, counting both its
/// expression bodies and nested `for` statements. Same bailing
/// discipline as [`prop_exceeds_depth`].
fn stmt_exceeds_depth(stmt: &Stmt, budget: usize, depths: &HashMap<DefinitionName, usize>) -> bool {
    let Some(budget) = budget.checked_sub(1) else {
        return true;
    };
    match stmt {
        Stmt::Require(p) | Stmt::BindOne(p) => prop_exceeds_depth(p, budget, depths),
        Stmt::Let { value, .. } => value_exceeds_depth(value, budget, depths),
        Stmt::Assert(_) | Stmt::Retract { .. } | Stmt::Emit(_) | Stmt::LetNewSubject { .. } => {
            false
        }
        Stmt::For {
            collection, body, ..
        } => {
            value_exceeds_depth(collection, budget, depths)
                || body.iter().any(|s| stmt_exceeds_depth(s, budget, depths))
        }
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
    duplicates.sort_unstable();
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
    dup_intents.sort_unstable();
    for name in dup_intents {
        errors.push(ValidationError::DuplicateDecl {
            vocabulary: VocabularyKind::Intent,
            name: name.to_string(),
        });
    }

    // Same duplicate check for definitions.
    let mut seen_definitions = HashMap::<&str, usize>::new();
    for decl in &p.definitions {
        *seen_definitions.entry(decl.name.as_str()).or_insert(0) += 1;
    }
    let mut dup_definitions: Vec<&str> = seen_definitions
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(name, _)| *name)
        .collect();
    dup_definitions.sort_unstable();
    for name in dup_definitions {
        errors.push(ValidationError::DuplicateDecl {
            vocabulary: VocabularyKind::Definition,
            name: name.to_string(),
        });
    }

    // Definitions and predicates share the claim-shaped reference
    // namespace (`name(args)` in a body resolves to exactly one of
    // them), so a name in both vocabularies is a collision, not two
    // independent declarations.
    let mut collisions: Vec<&str> = p
        .definitions
        .iter()
        .filter(|d| p.predicates.iter().any(|pr| pr.name.as_str() == d.name))
        .map(|d| d.name.as_str())
        .collect();
    collisions.sort_unstable();
    collisions.dedup();
    for name in collisions {
        errors.push(ValidationError::DefinitionNameCollision {
            name: name.to_string(),
        });
    }

    errors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir_builder::*;

    fn empty_program() -> Program {
        program("t").build()
    }

    #[test]
    fn expression_nested_past_the_limit_is_rejected() {
        // A `not not not ... A()` chain deeper than the limit.
        // Building and dropping it is heap work, not recursion; only
        // the bailing depth check walks it, so the test itself cannot
        // overflow on the input it is asserting gets rejected.
        let mut body = claim("A", vec![]);
        for _ in 0..(MAX_EXPR_DEPTH + 50) {
            body = not(body);
        }
        let mut p = empty_program();
        p.invariants = vec![invariant("deep", body)];
        let errs = p
            .validate()
            .expect_err("over-deep invariant must be rejected");
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::NestingTooDeep {
                    context: ValidationContext::Invariant { name }
                } if name == "deep"
            )),
            "expected NestingTooDeep for the invariant; got {errs:?}"
        );
    }

    #[test]
    fn nested_for_statements_past_the_limit_are_rejected() {
        // `for z in c: for z in c: ...` - statement nesting is the
        // other recursion dimension the guard covers.
        let mut inner = vec![assert_("A", vec![var("z")])];
        for _ in 0..(MAX_EXPR_DEPTH + 50) {
            inner = vec![for_("z", term(var("c")), inner)];
        }
        let mut p = empty_program();
        p.transformations = vec![transformation("deep", params(&["c"]), inner)];
        let errs = p
            .validate()
            .expect_err("over-deep for-nesting must be rejected");
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::NestingTooDeep {
                    context: ValidationContext::Transformation { name }
                } if name == "deep"
            )),
            "expected NestingTooDeep for the transformation; got {errs:?}"
        );
    }

    #[test]
    fn shallow_nesting_passes_the_depth_guard() {
        // A handful of levels: the guard must leave it alone (the rest
        // of validation passes too, so this also pins that the guard
        // adds no spurious error to a clean programme).
        let mut p = empty_program();
        p.predicates = vec![predicate("A").build()];
        p.invariants = vec![invariant("shallow", not(not(not(claim("A", vec![])))))];
        assert!(
            p.validate().is_ok(),
            "shallow nesting must validate cleanly"
        );
    }
}
