//! Programme-level validation. Owns the [`ValidationError`] vocabulary
//! and merges several passes into one error list: a definition-cycle
//! check and a nesting-depth guard (both first, so later recursive walks
//! cannot loop or overflow), name-level declaration and discipline
//! checks, and the static-check traversal in [`crate::check`].
//!
//! Called via [`crate::Program::validate`]. Strict: undeclared
//! predicates and intents are errors. Every error is collected, so an
//! author fixing a programme sees the whole list at once.

use crate::ir::{DefinitionName, PredicateArgKind, Program, Prop, Stmt, ValueExpr};
use std::collections::HashMap;

/// Proof-of-validity handle: a reference to a [`Program`] that passed
/// [`Program::validate`]. The only way to obtain one is
/// [`Program::validated`], which fails if validation reports errors.
///
/// The analysis accessors ([`crate::transformation_param_kinds`],
/// [`crate::transformation_arg_schema`]) only make sense over a valid
/// programme. Taking this type instead of `&Program` puts that
/// precondition in the signature, so they need not re-validate.
///
/// `Copy` because it wraps a single reference.
#[derive(Debug, Clone, Copy)]
pub struct ValidatedProgram<'a>(&'a Program);

impl<'a> ValidatedProgram<'a> {
    /// Borrow the underlying programme.
    pub fn as_program(&self) -> &'a Program {
        self.0
    }

    /// Wraps a programme that has just passed validation. Crate-private
    /// so [`Program::validated`] stays the only way in.
    pub(crate) fn from_validated(program: &'a Program) -> Self {
        Self(program)
    }
}

/// Which declared vocabulary a validation error refers to. Several
/// errors (undeclared, arity, duplicate, arg kind) apply to more than
/// one vocabulary; their `vocabulary` field names which in the message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VocabularyKind {
    Predicate,
    Intent,
    Definition,
    Derived,
}

impl std::fmt::Display for VocabularyKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VocabularyKind::Predicate => write!(f, "predicate"),
            VocabularyKind::Intent => write!(f, "intent"),
            VocabularyKind::Definition => write!(f, "definition"),
            VocabularyKind::Derived => write!(f, "derived claim"),
        }
    }
}

/// Where in a programme a validation error was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationContext {
    Invariant {
        name: String,
    },
    Transformation {
        name: String,
        /// 0-based index of the top-level body statement, when known;
        /// `None` for transformation-level findings. A finding inside a
        /// `for` carries the `for`'s own index. Surface tooling maps it
        /// to a source span.
        statement: Option<usize>,
    },
    DerivedClaim {
        predicate: String,
    },
    Definition {
        name: String,
    },
}

/// The unbound-variable remedy talks about `require`/`bind`/`let`, which
/// exist only in transformation bodies, so other contexts get no hint.
fn unbound_variable_hint(context: &ValidationContext) -> &'static str {
    match context {
        ValidationContext::Transformation { .. } => {
            "; a `require` match does not export its bindings to later statements - \
             `bind` looks up a claim and exports its fields, `let` binds a computed value"
        }
        ValidationContext::Invariant { .. }
        | ValidationContext::DerivedClaim { .. }
        | ValidationContext::Definition { .. } => "",
    }
}

/// The optional "; if these are Date operands, use `on_or_before`" tail
/// of a [`ValidationError::OperandKindMismatch`]. Worded as "if" because
/// the other operand may be wrong too.
fn suggestion_suffix(suggestion: &Option<&'static str>, actual: &PredicateArgKind) -> String {
    match suggestion {
        Some(token) => format!("; if these are {actual} operands, use `{token}`"),
        None => String::new(),
    }
}

/// A single failure surfaced by [`Program::validate`], which collects
/// every error rather than stopping at the first.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    /// A reference names something that is not declared.
    #[error("undeclared {vocabulary} `{name}` referenced in {context}")]
    Undeclared {
        vocabulary: VocabularyKind,
        name: String,
        context: ValidationContext,
    },
    /// A reference passes a different number of arguments than the
    /// declaration calls for.
    #[error(
        "{vocabulary} `{name}` declared with arity {expected} but referenced with {actual} args in {context}"
    )]
    ArityMismatch {
        vocabulary: VocabularyKind,
        name: String,
        expected: usize,
        actual: usize,
        context: ValidationContext,
    },
    /// Two declarations in the same vocabulary share a name. Even
    /// if both agree on arity, the duplicate is a modelling bug.
    #[error("duplicate {vocabulary} declaration for `{name}`")]
    DuplicateDecl {
        vocabulary: VocabularyKind,
        name: String,
    },
    /// An argument does not match the kind declared for its position,
    /// e.g. a date literal in a `Decimal` slot.
    #[error(
        "{vocabulary} `{name}` arg #{position} expects {expected} but received {actual} in {context}"
    )]
    ArgKindMismatch {
        vocabulary: VocabularyKind,
        name: String,
        position: usize,
        expected: PredicateArgKind,
        actual: PredicateArgKind,
        context: ValidationContext,
    },
    /// A predicate carries more than one `effective by` clause. There is
    /// one selector per predicate, so a second clause would be silently
    /// skipped. A predicate with two time axes needs to be split in two.
    #[error(
        "`{predicate}` carries more than one `effective by` clause: a predicate has \
         one in-force selector, so a second clause would be silently ignored"
    )]
    MultipleEffectiveClauses { predicate: String },

    /// `effective by (..) on (f)` named `f` as both a key and the date.
    /// Each version would then be its own group, so nothing could
    /// supersede anything.
    #[error(
        "`{predicate}` is effective-dated by `{field}`, which is also one of its \
         keys: a version cannot be grouped by the date that orders it"
    )]
    EffectiveDateIsAKey { predicate: String, field: String },

    /// `effective by (..) on (f)` where `f` is not a Date or Timestamp.
    #[error(
        "`{predicate}` is effective-dated by `{field}`, which is declared {actual}: \
         effective dating needs a Date or Timestamp field"
    )]
    EffectiveDateNotATime {
        predicate: String,
        field: String,
        actual: PredicateArgKind,
    },

    /// A predicate declaration carries a discipline and a `derived`
    /// declaration computes it.
    ///
    /// Disciplines are promises about governed state. A derived output is
    /// computed on demand and replaced wholesale on refresh, so it can keep
    /// none of them. Reported at the declaration, where the author wrote
    /// the clause, rather than at a generated rule nobody typed.
    #[error(
        "`{predicate}` is computed by a derived claim, so it cannot carry a \
         discipline: disciplines promise how governed state behaves, and a \
         derived claim is a read model replaced wholesale on refresh."
    )]
    DisciplineOnDerived { predicate: String },

    /// An invariant declared `total over P` for a `P` the programme does
    /// not declare.
    ///
    /// Refused rather than ignored: the declaration tells the vacuity lints
    /// this rule is the backstop for `P`, so a typo would silently withdraw
    /// the guarantee it appears to make.
    #[error(
        "invariant `{invariant}` declares `total over {predicate}`, but no \
         predicate `{predicate}` is declared. The totality declaration names \
         the predicate whose coverage this rule guarantees, so an unknown name \
         withdraws the guarantee silently."
    )]
    UnknownTotalityTarget {
        invariant: String,
        predicate: String,
    },

    /// Two refusing statements in one transformation answer to the same
    /// name.
    ///
    /// A rule name identifies which statement refused, so a duplicate makes
    /// the refusal ambiguous. Checked per transformation, not programme-wide:
    /// two transformations may carry the same gate verbatim.
    #[error(
        "{context} reuses the rule name `{name}`. A rule name identifies the statement \
         that refused, so a duplicate makes a refusal ambiguous - rename one, or leave \
         the less interesting one unnamed."
    )]
    DuplicateRuleName {
        context: ValidationContext,
        name: String,
    },

    /// A rule named a predicate that a `derived` declaration computes.
    ///
    /// Rules evaluate against admitted claims. A derived claim is a read
    /// model computed on demand, never admitted, so no rule can see it.
    ///
    /// Refused outright rather than treated as a rule that matches nothing:
    /// rows admitted under that name by an older version of the programme
    /// may still exist, and the name would then have two sources.
    #[error(
        "`{predicate}` is a derived claim and {context} names it: a derived \
         claim is computed from admitted claims and refreshed out of band, \
         so a rule can neither match one nor admit one. Name the claims it \
         is computed from, or make the figure a claim of its own."
    )]
    DerivedInRule {
        predicate: String,
        context: ValidationContext,
    },
    /// `max`/`min` ranged over a kind with no order, such as a subject or a
    /// collection. Refused rather than given an arbitrary order. The
    /// message lists the kinds that do order, because the check is an
    /// allow-list.
    #[error(
        "{op} needs an ordered kind but received {actual} in {context}; \
         only decimals, dates, timestamps, durations and quantities have an order"
    )]
    UnorderedExtremum {
        op: &'static str,
        actual: PredicateArgKind,
        context: ValidationContext,
    },
    /// A wildcard stood where a value must be produced, such as an
    /// arithmetic operand or a sum target. `_` never carries a value, so
    /// evaluation would raise `EvalError::TypeMismatch`.
    #[error(
        "`_` is not a value in {context}: a wildcard marks an unread claim-pattern \
         position; name the variable this expression reads"
    )]
    WildcardAsValue { context: ValidationContext },
    /// A `value` lookup's extraction index does not point at a wildcard
    /// in its own argument list. Only hand-built IR can reach this; the
    /// parser cannot produce it.
    #[error(
        "value lookup on `{predicate}` extracts position {extract}, which is not a \
         wildcard hole in its argument list, in {context}"
    )]
    InvalidValueExtraction {
        predicate: String,
        extract: usize,
        context: ValidationContext,
    },
    /// A sum's target reads as a duration or quantity, but its empty case
    /// would be a bare-decimal zero, which fails at runtime the first time
    /// the sum is empty. The fix is to bind the summed value inside the
    /// sum's body, or pair the target with an operand of the right kind.
    #[error(
        "the empty case of this sum cannot be typed in {context}: the target reads as \
         {target}, but the empty sum would evaluate to {seed}; bind the summed value \
         in the sum's own body, or give the target an operand of the expected kind"
    )]
    EmptySumUntyped {
        target: PredicateArgKind,
        seed: PredicateArgKind,
        context: ValidationContext,
    },
    /// An operator (comparator, arithmetic, `sum`, `for`, `in`,
    /// `value default`) received an operand of the wrong kind, e.g.
    /// `Le(date, decimal)`. Caught here instead of as
    /// `EvalError::TypeMismatch` at runtime.
    #[error(
        "{operator} expects {expected} operand(s) but received {actual}{} in {context}",
        suggestion_suffix(suggestion, actual)
    )]
    OperandKindMismatch {
        operator: &'static str,
        expected: PredicateArgKind,
        actual: PredicateArgKind,
        /// The comparator that DOES order the received kind, when one
        /// exists - `on_or_before` for a Date operand under `<=`.
        suggestion: Option<&'static str>,
        context: ValidationContext,
    },
    /// An arithmetic operator was applied to a pair of kinds with no
    /// rule, e.g. adding two timestamps.
    #[error("no arithmetic rule for {left} {operator} {right} in {context}")]
    NoArithRule {
        operator: &'static str,
        left: PredicateArgKind,
        right: PredicateArgKind,
        context: ValidationContext,
    },
    /// `min`/`max` applied to a kind with no order, such as a subject or
    /// a calendar span.
    #[error(
        "{builtin} is defined on ordered values - decimals, quantities, durations, dates, \
         and timestamps - not {kind} in {context}"
    )]
    BuiltinKind {
        builtin: &'static str,
        kind: PredicateArgKind,
        context: ValidationContext,
    },
    /// `abs(...)` was applied to a value that has no magnitude. Defined
    /// on the signed numeric kinds - decimals, quantities, and durations.
    #[error("abs is defined on decimals, quantities, and durations, not {kind} in {context}")]
    AbsKind {
        kind: PredicateArgKind,
        context: ValidationContext,
    },
    /// A `round(...)` whose quantum is a literal zero or negative decimal.
    /// A non-positive quantum from a variable is caught at runtime by
    /// `EvalError::RoundQuantumNotPositive`.
    #[error("round quantum must be a positive decimal, got {quantum} in {context}")]
    RoundQuantumNotPositive {
        quantum: String,
        context: ValidationContext,
    },
    /// A predicate or intent declaration names `CalendarSpan` as an
    /// argument kind. A span only shifts dates inside arithmetic; it is
    /// never a stored value. The parser cannot declare one, so only
    /// hand-built IR reaches this.
    #[error(
        "argument `{argument}` of `{declaration}` declares CalendarSpan, an expression-only kind that no claim or intent can carry"
    )]
    CalendarSpanNotDeclarable {
        declaration: String,
        argument: String,
    },
    /// A calendar span reached a place only stored values may occupy: a
    /// claim or intent argument (even an `Any` one), a derived output
    /// value, or a transformation parameter. The runtime refuses these
    /// too; this catches them at `check` time.
    #[error(
        "a calendar span cannot leave expression position: {place} in {context}; a span shifts a date inside arithmetic and is never itself a governed value"
    )]
    CalendarSpanEscapesExpression {
        place: String,
        context: ValidationContext,
    },
    /// A builtin called with the wrong number of arguments. Only
    /// hand-built IR reaches this; the parser fixes the count.
    #[error("{builtin} takes {expected} argument(s), got {found} in {context}")]
    BuiltinArity {
        builtin: &'static str,
        expected: usize,
        found: usize,
        context: ValidationContext,
    },
    /// A period builtin (`period_index`, `period_start_of`) was given a
    /// literal zero-length span, so there are no periods to index. A span
    /// from a variable is caught at evaluation instead.
    #[error("{builtin} needs a positive span; got {span} in {context}")]
    PeriodSpanNotPositive {
        builtin: &'static str,
        span: String,
        context: ValidationContext,
    },
    /// A `period_start_of` whose index is a literal fraction. Period
    /// indexes are whole numbers. A computed index is caught at
    /// evaluation instead.
    #[error("period_start_of needs a whole-number index; got {index} in {context}")]
    PeriodIndexNotWhole {
        index: String,
        context: ValidationContext,
    },
    /// A conditional's two branches have incompatible kinds, e.g.
    /// `if(p, #meter, 100)`.
    #[error(
        "the branches of `if` must have the same kind; `then` is {then_kind}, `otherwise` is {otherwise_kind}, in {context}"
    )]
    CondBranchKindMismatch {
        then_kind: PredicateArgKind,
        otherwise_kind: PredicateArgKind,
        context: ValidationContext,
    },
    /// An equality (`==` or `!=`) had operands of incompatible kinds,
    /// e.g. `Subject == Decimal`. A kind error, not a silent false.
    /// Symmetric, so neither side is "expected".
    #[error("{operator} operands must have the same kind; got {left} vs {right} in {context}")]
    EqualityKindMismatch {
        operator: &'static str,
        left: PredicateArgKind,
        right: PredicateArgKind,
        context: ValidationContext,
    },
    /// A variable was bound at one kind and later used at an incompatible
    /// one, e.g. bound from a `Decimal` slot, then used in a `Subject` slot.
    #[error(
        "variable `{variable}` was first constrained to {previous} but later used as {new} in {context}"
    )]
    VariableKindConflict {
        variable: String,
        previous: PredicateArgKind,
        new: PredicateArgKind,
        context: ValidationContext,
    },
    /// `actor` was referenced outside a transformation body, where no
    /// proposer is in scope (evaluation would raise
    /// `EvalError::UnboundActor`). Authority checks belong in a `require`.
    #[error(
        "`actor` is not available in {context}; it resolves only inside transformation bodies, so authority checks belong in a `require`"
    )]
    ActorNotAvailable { context: ValidationContext },
    /// A body nests deeper than the fixed maximum depth. Evaluation
    /// recurses once per level, so a very deep body would overflow the
    /// stack during `propose`. This is why untrusted IR must be validated
    /// before it is proposed.
    #[error("nesting in {context} exceeds the maximum depth of {}", MAX_EXPR_DEPTH)]
    NestingTooDeep { context: ValidationContext },
    /// A variable was used where a bound value is needed (an argument
    /// to `admit`/`retract`/`emit`, an operand, a lookup key, a `sum`
    /// target) before anything bound it. Names are bound by parameters,
    /// `bind`, `let`, `for`, and claim matches; `require` does not export
    /// its matches to later statements. Evaluation would raise
    /// `EvalError::UnboundVariable`.
    #[error(
        "variable `{variable}` is used in {context} but nothing binds it{}",
        unbound_variable_hint(context)
    )]
    UnboundVariable {
        variable: String,
        context: ValidationContext,
    },
    /// A derived claim's value expression names a variable its domain
    /// binds but its head does not carry. Values are computed once per
    /// distinct head key, so a non-key variable has no single value there
    /// (evaluation would raise `EvalError::UnboundVariable`).
    #[error(
        "variable `{variable}` is bound by the domain of {context} but is not a head \
         key, so it is not available while computing values; add `{variable}` to the \
         head if the value depends on it, or, if it only positions a lookup, drop it \
         and name the extracted field (`field: _`), eliding unused coordinates with `..`"
    )]
    DerivedValueNotAKey {
        variable: String,
        context: ValidationContext,
    },
    /// A definition shares a name with a predicate. In a rule body,
    /// `name(args)` must resolve to exactly one of them; otherwise adding
    /// a definition could silently change what existing text means.
    #[error(
        "definition `{name}` collides with predicate `{name}`; the two share the reference namespace in rule bodies, so a reference could mean either - rename one"
    )]
    DefinitionNameCollision { name: String },
    /// Definitions reference each other in a cycle, so expanding them
    /// would never terminate. `names` holds one cycle's members, sorted.
    #[error(
        "definitions reference each other in a cycle ({}); a definition must expand to claims and conditions, never back to itself",
        .names.join(", ")
    )]
    DefinitionCycle { names: Vec<String> },
    /// A predicate declared `partial` that some invariant also declares
    /// `total over`.
    ///
    /// One says coverage gaps are intended, the other that there are none.
    /// Refused rather than resolved by precedence, since either choice
    /// would silently override something the author wrote.
    #[error(
        "`{predicate}` is declared `partial`, but invariant `{invariant}` declares \
         `total over {predicate}`. Those contradict: one says coverage gaps are \
         intended, the other that a rule guarantees none. Drop whichever is not true."
    )]
    PartialContradictsTotality {
        predicate: String,
        invariant: String,
    },

    /// A reference names a definition where a predicate is required: an
    /// `admit` / `retract` / `value` target, or a hand-built `Prop::Claim`
    /// that skipped resolution. A definition is a condition only. Hand-built
    /// IR builds calls with `ir_builder::defined` or runs
    /// [`crate::resolve_defined_calls`] before validating.
    #[error(
        "`{name}` names a definition where a predicate is required, in {context}; a definition is a condition - callable in rule bodies, never an `admit`/`retract`/`emit` target or a `value` lookup (hand-built body calls use `ir_builder::defined` or `resolve_defined_calls`)"
    )]
    UnresolvedDefinitionCall {
        name: String,
        context: ValidationContext,
    },
    /// A definition parameter is never referenced by the body. A call
    /// passing an unbound variable for it would always fail at runtime,
    /// since nothing could give it a value.
    #[error(
        "parameter `{parameter}` of definition `{definition}` is not referenced by the definition body; remove it or reference it in a condition"
    )]
    ParameterNotReferenced {
        definition: String,
        parameter: String,
    },
    /// A definition declares the same parameter name twice, so the later
    /// argument would silently overwrite the earlier one.
    #[error(
        "definition `{definition}` declares parameter `{parameter}` more than once; each parameter is one binding slot"
    )]
    DuplicateParameter {
        definition: String,
        parameter: String,
    },
    /// A predicate or intent declaration repeats an argument name. Named
    /// patterns, views, schemas and the named codec all address fields by
    /// name, so a repeat is ambiguous.
    #[error(
        "{vocabulary} `{name}` declares argument `{field}` more than once; \
         each field names one position"
    )]
    DuplicateArgName {
        vocabulary: VocabularyKind,
        name: String,
        field: String,
    },
    /// `pre(...)` was used inside a definition body. Definitions are
    /// context-free, so a call means the same everywhere. Wrap the *call*
    /// in `pre(...)` instead.
    #[error(
        "pre(...) is used in {context}, but definitions are context-free and carry no pre-state; wrap the call in pre(...) at the use site instead"
    )]
    PreNotAvailable { context: ValidationContext },
    /// A discipline clause names a field its predicate does not have.
    #[error(
        "a discipline on predicate `{predicate}` names field `{field}`, which the declaration does not have"
    )]
    DisciplineUnknownField { predicate: String, field: String },
    /// A `unique by` / `current pointer by` clause whose key set leaves
    /// nothing to determine: zero fields, or every field a key (claims
    /// are a set - two identical claims are already one claim).
    #[error(
        "a uniqueness discipline on `{predicate}` needs at least one key field and at least one field for the keys to determine; keying every field adds nothing, because claims are a set and two identical claims are already one claim"
    )]
    DisciplineVacuousKeys { predicate: String },
    /// The same discipline clause declared twice on one predicate.
    #[error("predicate `{predicate}` declares the same discipline clause twice")]
    DisciplineDuplicateClause { predicate: String },
    /// `append only` and `current pointer by` on the same predicate: a
    /// current pointer must be retractable to move, which is the exact
    /// opposite commitment.
    #[error(
        "`{predicate}` is declared both `append only` and `current pointer`; a pointer must be retractable to move, which is the opposite commitment - drop one"
    )]
    DisciplinePointerCannotBeAppendOnly { predicate: String },
    /// A `superseded via` clause whose lineage predicate is undeclared,
    /// does not have exactly two arguments `(successor, prior)`, or is
    /// itself a current pointer.
    #[error(
        "`superseded via {lineage}` on `{pointer}`: {reason} (a lineage predicate has exactly two arguments, successor then prior, and is not itself a pointer)"
    )]
    DisciplineLineageUnfit {
        pointer: String,
        lineage: String,
        reason: String,
    },
    /// `superseded via` on a predicate that is not declared a current
    /// pointer. Supersession only has meaning for a pointer.
    #[error(
        "`superseded via` on `{predicate}`, which is not declared `current pointer by (...)`; supersession history is the pointer's history - declare the pointer, or drop the clause"
    )]
    DisciplineSupersededWithoutPointer { predicate: String },
    /// A transformation retracts an append-only predicate (declared so,
    /// or the lineage of a `superseded via`).
    #[error(
        "{context} retracts `{predicate}`, which is append only; corrections are admitted as supersessions or exception claims, never by retracting the record"
    )]
    RetractsAppendOnly {
        predicate: String,
        context: ValidationContext,
    },
    /// A discipline's generated invariant is missing: the IR was
    /// hand-built and `lower_disciplines` never ran (the parser runs it),
    /// so the discipline would go unenforced.
    #[error(
        "a discipline on `{predicate}` implies the generated invariant `{invariant}`, which this programme does not carry; run `lower_disciplines` before validating (the parser does this) so the declared commitment is actually enforced"
    )]
    DisciplineNotLowered {
        predicate: String,
        invariant: String,
    },
}

impl std::fmt::Display for ValidationContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValidationContext::Invariant { name } => write!(f, "invariant `{name}`"),
            ValidationContext::Transformation { name, statement } => {
                write!(f, "transformation `{name}`")?;
                if let Some(index) = statement {
                    write!(f, ", statement {}", index + 1)?;
                }
                Ok(())
            }
            ValidationContext::DerivedClaim { predicate } => {
                write!(f, "derived claim `{predicate}`")
            }
            ValidationContext::Definition { name } => write!(f, "definition `{name}`"),
        }
    }
}

/// Strict programme validation, called via [`Program::validate`].
/// Name-level checks (declarations, disciplines) live here because they
/// compare names rather than walk bodies; every body walk lives in
/// [`crate::check::check_program`].
pub(crate) fn validate_program(p: &Program) -> Result<(), Vec<ValidationError>> {
    // A definition cycle is checked first: the depth guard and
    // evaluation both expand calls and would loop forever on one. A
    // cyclic programme gets only the cycle and name-level errors.
    let order = match crate::definitions::definition_topo_order(&p.definitions) {
        Ok(order) => order,
        Err(names) => {
            let mut errors = collect_duplicate_decl_errors(p);
            errors.extend(collect_discipline_errors(p));
            errors.push(ValidationError::DefinitionCycle { names });
            return Err(errors);
        }
    };

    // The depth guard runs next and short-circuits, because `check`
    // recurses over every body and a deep enough one would overflow it.
    // Definitions are measured callees first, at their expanded depth,
    // so a chain of shallow-looking definitions cannot hide deep nesting.
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
    errors.extend(collect_discipline_errors(p));
    errors.extend(crate::check::check_program(p));
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Nesting depth of `prop` (at least 1), counting a definition call at
/// its callee's expanded depth, or `None` once it exceeds `budget`. Stops
/// as soon as the budget runs out, so its own recursion stays bounded.
fn prop_depth_capped(
    prop: &Prop,
    budget: usize,
    depths: &HashMap<DefinitionName, usize>,
) -> Option<usize> {
    let inner = budget.checked_sub(1)?;
    let below = match prop {
        Prop::Claim { .. } | Prop::In(_, _) => 0,
        // An unknown name counts as zero; `check` reports it.
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
    let below =
        match expr {
            ValueExpr::Term(_) => 0,
            ValueExpr::Arith { left, right, .. } => value_depth_capped(left, inner, depths)?
                .max(value_depth_capped(right, inner, depths)?),
            ValueExpr::Sum { value, body, .. } => value_depth_capped(value, inner, depths)?
                .max(prop_depth_capped(body, inner, depths)?),
            ValueExpr::Extremum { body, .. } => prop_depth_capped(body, inner, depths)?,
            ValueExpr::ValueOf { default, .. } => match default.as_deref() {
                Some(d) => value_depth_capped(d, inner, depths)?,
                None => 0,
            },
            ValueExpr::Call { args, .. } => {
                let mut deepest = 0;
                for a in args {
                    deepest = deepest.max(value_depth_capped(a, inner, depths)?);
                }
                deepest
            }
            ValueExpr::Cond {
                when,
                then,
                otherwise,
            } => prop_depth_capped(when, inner, depths)?
                .max(value_depth_capped(then, inner, depths)?)
                .max(value_depth_capped(otherwise, inner, depths)?),
        };
    let total = below + 1;
    (total <= budget).then_some(total)
}

/// Maximum expression / nested-statement depth accepted by
/// [`Program::validate`]. The evaluator and the check walk recurse once
/// per level, so this keeps `propose` from overflowing the stack on
/// untrusted IR. Generous for hand-written programmes, well within a
/// default stack.
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
        if prop_depth_capped(&inv.body, MAX_EXPR_DEPTH, depths).is_none() {
            errors.push(ValidationError::NestingTooDeep {
                context: ValidationContext::Invariant {
                    name: inv.name.to_string(),
                },
            });
        }
    }
    for t in &p.transformations {
        if let Some(index) = t
            .body
            .iter()
            .position(|s| stmt_exceeds_depth(s, MAX_EXPR_DEPTH, depths))
        {
            errors.push(ValidationError::NestingTooDeep {
                context: ValidationContext::Transformation {
                    name: t.name.to_string(),
                    statement: Some(index),
                },
            });
        }
    }
    for d in &p.derived_claims {
        let too_deep = prop_depth_capped(&d.domain, MAX_EXPR_DEPTH, depths).is_none()
            || d.values
                .iter()
                .any(|v| value_depth_capped(&v.expr, MAX_EXPR_DEPTH, depths).is_none());
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

/// True if `stmt` nests deeper than `budget` levels, counting both its
/// expressions and nested `for` statements.
fn stmt_exceeds_depth(stmt: &Stmt, budget: usize, depths: &HashMap<DefinitionName, usize>) -> bool {
    let Some(budget) = budget.checked_sub(1) else {
        return true;
    };
    match stmt {
        Stmt::Require { prop: p, .. } | Stmt::BindOne { prop: p, .. } => {
            prop_depth_capped(p, budget, depths).is_none()
        }
        Stmt::Let { value, .. } => value_depth_capped(value, budget, depths).is_none(),
        Stmt::Assert(_) | Stmt::Retract { .. } | Stmt::Emit(_) | Stmt::LetNewSubject { .. } => {
            false
        }
        Stmt::For {
            collection, body, ..
        } => {
            value_depth_capped(collection, budget, depths).is_none()
                || body.iter().any(|s| stmt_exceeds_depth(s, budget, depths))
        }
    }
}

/// The names appearing more than once, sorted so errors come out in a
/// stable order.
fn duplicated<'a>(names: impl Iterator<Item = &'a str>) -> Vec<&'a str> {
    let mut seen = HashMap::<&str, usize>::new();
    for name in names {
        *seen.entry(name).or_insert(0) += 1;
    }
    let mut duplicates: Vec<&str> = seen
        .into_iter()
        .filter(|(_, count)| *count > 1)
        .map(|(name, _)| name)
        .collect();
    duplicates.sort_unstable();
    duplicates
}

/// Name-level declaration checks: duplicate names in each vocabulary,
/// repeated parameter and argument names, `CalendarSpan` arguments, and
/// definition-predicate name collisions.
fn collect_duplicate_decl_errors(p: &Program) -> Vec<ValidationError> {
    let mut errors = Vec::new();
    fn duplicate_decls(
        errors: &mut Vec<ValidationError>,
        vocabulary: VocabularyKind,
        names: Vec<&str>,
    ) {
        for name in names {
            errors.push(ValidationError::DuplicateDecl {
                vocabulary,
                name: name.to_string(),
            });
        }
    }
    duplicate_decls(
        &mut errors,
        VocabularyKind::Predicate,
        duplicated(p.predicates.iter().map(|d| d.name.as_str())),
    );

    // CalendarSpan is expression-only; the surface cannot declare it,
    // and hand-built IR is held to the same rule.
    let declared_args = || {
        p.predicates
            .iter()
            .map(|d| (VocabularyKind::Predicate, d.name.to_string(), &d.args))
            .chain(
                p.intents
                    .iter()
                    .map(|d| (VocabularyKind::Intent, d.name.to_string(), &d.args)),
            )
    };
    for (_, name, args) in declared_args() {
        for arg in args {
            if arg.kind == crate::ir::PredicateArgKind::CalendarSpan {
                errors.push(ValidationError::CalendarSpanNotDeclarable {
                    declaration: name.clone(),
                    argument: arg.name.clone(),
                });
            }
        }
    }

    // Intents, definitions, and derived heads are separate namespaces.
    duplicate_decls(
        &mut errors,
        VocabularyKind::Intent,
        duplicated(p.intents.iter().map(|d| d.name.as_str())),
    );
    duplicate_decls(
        &mut errors,
        VocabularyKind::Definition,
        duplicated(p.definitions.iter().map(|d| d.name.as_str())),
    );
    duplicate_decls(
        &mut errors,
        VocabularyKind::Derived,
        duplicated(p.derived_claims.iter().map(|d| d.predicate.as_str())),
    );

    for def in &p.definitions {
        for parameter in duplicated(def.parameters.iter().map(crate::ir::Var::as_str)) {
            errors.push(ValidationError::DuplicateParameter {
                definition: def.name.to_string(),
                parameter: parameter.to_string(),
            });
        }
    }

    for (vocabulary, name, args) in declared_args() {
        for field in duplicated(args.iter().map(|a| a.name.as_str())) {
            errors.push(ValidationError::DuplicateArgName {
                vocabulary,
                name: name.clone(),
                field: field.to_string(),
            });
        }
    }

    // Definitions and predicates share one namespace in rule bodies.
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

/// Name-level checks for declared disciplines, plus the static ban on
/// retracting append-only predicates. `lower_disciplines` skips any
/// clause flagged here; the diagnostics live in this pass.
fn collect_discipline_errors(p: &Program) -> Vec<ValidationError> {
    use crate::ir::Discipline;

    let mut errors = Vec::new();
    for decl in &p.predicates {
        // Key fields are a set: `unique by (a, b)` and `unique by (b, a)`
        // are the same clause, so compare them with fields sorted.
        let canonical = |d: &Discipline| match d {
            Discipline::UniqueBy { fields } => {
                let mut fields = fields.clone();
                fields.sort_unstable();
                Discipline::UniqueBy { fields }
            }
            Discipline::CurrentPointerBy { fields } => {
                let mut fields = fields.clone();
                fields.sort_unstable();
                Discipline::CurrentPointerBy { fields }
            }
            other => other.clone(),
        };
        let canonical_clauses: Vec<Discipline> = decl.disciplines.iter().map(canonical).collect();
        for (i, d) in canonical_clauses.iter().enumerate() {
            if canonical_clauses[..i].contains(d) {
                errors.push(ValidationError::DisciplineDuplicateClause {
                    predicate: decl.name.to_string(),
                });
            }
        }
        let is_pointer = decl
            .disciplines
            .iter()
            .any(|d| matches!(d, Discipline::CurrentPointerBy { .. }));
        let is_append_only = decl
            .disciplines
            .iter()
            .any(|d| matches!(d, Discipline::AppendOnly));
        let has_superseded = decl
            .disciplines
            .iter()
            .any(|d| matches!(d, Discipline::SupersededVia { .. }));
        if is_pointer && is_append_only {
            errors.push(ValidationError::DisciplinePointerCannotBeAppendOnly {
                predicate: decl.name.to_string(),
            });
        }
        if has_superseded && !is_pointer {
            errors.push(ValidationError::DisciplineSupersededWithoutPointer {
                predicate: decl.name.to_string(),
            });
        }
        if decl
            .disciplines
            .iter()
            .filter(|d| matches!(d, Discipline::EffectiveBy { .. }))
            .count()
            > 1
        {
            errors.push(ValidationError::MultipleEffectiveClauses {
                predicate: decl.name.to_string(),
            });
        }
        for d in &decl.disciplines {
            match d {
                Discipline::UniqueBy { fields } | Discipline::CurrentPointerBy { fields } => {
                    let mut all_known = true;
                    for field in fields {
                        if !decl.args.iter().any(|a| a.name == *field) {
                            all_known = false;
                            errors.push(ValidationError::DisciplineUnknownField {
                                predicate: decl.name.to_string(),
                                field: field.clone(),
                            });
                        }
                    }
                    let keys_everything =
                        all_known && decl.args.iter().all(|a| fields.contains(&a.name));
                    if fields.is_empty() || keys_everything {
                        errors.push(ValidationError::DisciplineVacuousKeys {
                            predicate: decl.name.to_string(),
                        });
                    }
                }
                Discipline::EffectiveBy { keys, on, .. } => {
                    for field in keys.iter().chain(std::iter::once(on)) {
                        if !decl.args.iter().any(|a| a.name == *field) {
                            errors.push(ValidationError::DisciplineUnknownField {
                                predicate: decl.name.to_string(),
                                field: field.clone(),
                            });
                        }
                    }
                    if keys.contains(on) {
                        errors.push(ValidationError::EffectiveDateIsAKey {
                            predicate: decl.name.to_string(),
                            field: on.clone(),
                        });
                    }
                    if let Some(arg) = decl.args.iter().find(|a| a.name == *on)
                        && !matches!(
                            arg.kind,
                            PredicateArgKind::Date | PredicateArgKind::Timestamp
                        )
                    {
                        errors.push(ValidationError::EffectiveDateNotATime {
                            predicate: decl.name.to_string(),
                            field: on.clone(),
                            actual: arg.kind.clone(),
                        });
                    }
                }
                Discipline::AppendOnly => {}
                Discipline::SupersededVia { lineage } => {
                    let reason = match p.predicates.iter().find(|l| l.name == *lineage) {
                        None => Some("it is not a declared predicate".to_string()),
                        Some(l) if l.args.len() != 2 => {
                            Some(format!("it has {} argument(s)", l.args.len()))
                        }
                        Some(l)
                            if l.disciplines
                                .iter()
                                .any(|d| matches!(d, Discipline::CurrentPointerBy { .. })) =>
                        {
                            Some("it is itself a current pointer".to_string())
                        }
                        Some(_) => None,
                    };
                    if let Some(reason) = reason {
                        errors.push(ValidationError::DisciplineLineageUnfit {
                            pointer: decl.name.to_string(),
                            lineage: lineage.to_string(),
                            reason,
                        });
                    }
                }
            }
        }
    }

    for (predicate, invariant) in crate::disciplines::expected_generated_invariants(p) {
        let lowered = p.invariants.iter().any(|inv| {
            inv.name.as_str() == invariant && inv.origin == crate::ir::InvariantOrigin::Discipline
        });
        if !lowered {
            errors.push(ValidationError::DisciplineNotLowered {
                predicate: predicate.to_string(),
                invariant,
            });
        }
    }

    let append_only = crate::disciplines::append_only_predicates(p);
    if !append_only.is_empty() {
        for t in &p.transformations {
            for (index, stmt) in t.body.iter().enumerate() {
                let context = ValidationContext::Transformation {
                    name: t.name.to_string(),
                    statement: Some(index),
                };
                collect_retract_bans(stmt, &append_only, &context, &mut errors);
            }
        }
    }
    errors
}

/// Recursive worker for the append-only retract ban: a `retract` of a
/// protected predicate anywhere in a body, including nested `for`s.
fn collect_retract_bans(
    stmt: &Stmt,
    append_only: &std::collections::BTreeSet<crate::ir::PredicateName>,
    context: &ValidationContext,
    errors: &mut Vec<ValidationError>,
) {
    match stmt {
        Stmt::Retract { predicate, .. } => {
            if append_only.contains(predicate) {
                errors.push(ValidationError::RetractsAppendOnly {
                    predicate: predicate.to_string(),
                    context: context.clone(),
                });
            }
        }
        Stmt::For { body, .. } => {
            for inner in body {
                collect_retract_bans(inner, append_only, context, errors);
            }
        }
        Stmt::Require { .. }
        | Stmt::BindOne { .. }
        | Stmt::Let { .. }
        | Stmt::LetNewSubject { .. }
        | Stmt::Assert(_)
        | Stmt::Emit(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir_builder::*;

    fn empty_program() -> Program {
        program("t").build()
    }

    #[test]
    fn a_declaration_repeating_an_argument_name_is_refused() {
        // A field names one position; named claim patterns (and every
        // other field-name consumer) are undefined over a repeat.
        let p = program("dup")
            .predicates(vec![predicate("P").decimal("x").decimal("x").build()])
            .build();
        let errs = p.validate().expect_err("duplicate field names refuse");
        assert!(
            errs.iter().any(|e| matches!(
                e,
                ValidationError::DuplicateArgName {
                    vocabulary: VocabularyKind::Predicate,
                    ..
                }
            )),
            "expected DuplicateArgName, got {errs:?}"
        );
    }

    #[test]
    fn expression_nested_past_the_limit_is_rejected() {
        // A `not not not ... A()` chain deeper than the limit. Only the
        // bounded depth check walks it, so the test cannot overflow.
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
                    context: ValidationContext::Transformation { name, .. }
                } if name == "deep"
            )),
            "expected NestingTooDeep for the transformation; got {errs:?}"
        );
    }

    #[test]
    fn shallow_nesting_passes_the_depth_guard() {
        // A few levels: the guard must add no error to a clean programme.
        let mut p = empty_program();
        p.predicates = vec![predicate("A").build()];
        p.invariants = vec![invariant("shallow", not(not(not(claim("A", vec![])))))];
        assert!(
            p.validate().is_ok(),
            "shallow nesting must validate cleanly"
        );
    }
}
