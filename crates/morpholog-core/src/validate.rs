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
    Invariant {
        name: String,
    },
    Transformation {
        name: String,
        /// 0-based index of the top-level body statement the finding
        /// was made in, when the walk knows it; `None` for
        /// transformation-level findings. A finding inside a `for`
        /// carries the `for`'s own index. Surface tooling resolves
        /// this to the statement's source span.
        statement: Option<usize>,
    },
    DerivedClaim {
        predicate: String,
    },
    Definition {
        name: String,
    },
}

/// The remedy sentence for an unbound variable is context-dependent:
/// `require`/`bind`/`let` are transformation-body vocabulary, so the
/// hint renders only where those statements exist. Invariant,
/// definition, and derived-claim bodies bind through matching
/// propositions and get no statement advice.
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

/// A single failure surfaced by [`Program::validate`]. The validator
/// collects every error rather than failing fast, so a programme
/// migration that adds declarations sees the full work list rather
/// than fixing one site, re-running, and discovering the next.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    /// A predicate or intent referenced somewhere in the programme
    /// is not declared. Strict mode: every reference must have a
    /// declaration.
    #[error("undeclared {vocabulary} `{name}` referenced in {context}")]
    Undeclared {
        vocabulary: VocabularyKind,
        name: String,
        context: ValidationContext,
    },
    /// A predicate or intent reference passes a different number of
    /// arguments than the declaration calls for.
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
    /// A predicate-call or intent-emit argument does not match the
    /// kind declared for that position. Surfaces things like
    /// `Policy(amount, 100)` where the first position is declared
    /// `Subject`, or a date literal flowing into a `Decimal` slot.
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
    /// A predicate declaration carries a discipline and a `derived`
    /// declaration computes it.
    ///
    /// Disciplines are promises about governed state - what may be
    /// retracted, which claims must agree, which pointer is current. A
    /// derived output is not governed state: it is computed on demand and
    /// its materialised generations are replaced wholesale on refresh, so
    /// it can honour none of them.
    ///
    /// Reported at the declaration rather than through the lowering,
    /// because that is where the author wrote the clause. `unique by`
    /// lowers to a generated invariant, so refusing it there names a rule
    /// nobody typed; `append only` lowers to nothing at all and would
    /// pass unnoticed.
    #[error(
        "`{predicate}` is computed by a derived claim, so it cannot carry a \
         discipline: disciplines promise how governed state behaves, and a \
         derived claim is a read model replaced wholesale on refresh."
    )]
    DisciplineOnDerived { predicate: String },

    /// A rule named a predicate that a `derived` declaration computes.
    ///
    /// The kernel evaluates against admitted claims; a derived claim is a
    /// read model, enumerated on demand and refreshed out of band, and no
    /// transformation ever admits one. So `bind`, `require`, `for` and
    /// the invariants cannot see it - the design type-checks and then
    /// fails against a live database, which is where a trial lost an hour
    /// to it.
    ///
    /// Refused as a modelling error rather than reported as a rule that
    /// matches nothing: state outlives a source file, so rows admitted
    /// under that name by an older shape of the programme may well exist.
    /// That is precisely the problem - the name would have two sources,
    /// the computed view and the stale rows.
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
    /// `max`/`min` ranged over a kind with no order: a subject is an
    /// opaque identifier, a boolean is not a scale, a collection is not a
    /// point on one. Refused here rather than given an arbitrary order.
    ///
    /// The message names the kinds that DO order, because the checker is
    /// an allow-list - listing the excluded ones would go stale the next
    /// time a kind is added, which is how it came to name only two.
    #[error(
        "{op} needs an ordered kind but received {actual} in {context}; \
         only decimals, dates, timestamps, durations and quantities have an order"
    )]
    UnorderedExtremum {
        op: &'static str,
        actual: PredicateArgKind,
        context: ValidationContext,
    },
    /// An operator (comparator, arithmetic, `sum`, `for`, `in`,
    /// `value default`) received an operand of the wrong kind.
    /// `Le(date, decimal)`, `Add(subject, decimal)`,
    /// `For` over a Decimal value - the kernel raises these as
    /// `EvalError::TypeMismatch` at runtime; this validator
    /// surfaces them at authoring time.
    #[error("{operator} expects {expected} operand(s) but received {actual} in {context}")]
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
    #[error("no arithmetic rule for {left} {operator} {right} in {context}")]
    NoArithRule {
        operator: &'static str,
        left: PredicateArgKind,
        right: PredicateArgKind,
        context: ValidationContext,
    },
    /// `abs(...)` was applied to a value that has no magnitude. Defined
    /// on the signed numeric kinds - decimals, quantities, and durations.
    #[error("abs is defined on decimals, quantities, and durations, not {kind} in {context}")]
    AbsKind {
        kind: PredicateArgKind,
        context: ValidationContext,
    },
    /// A `round(...)` whose quantum is a literal zero or negative
    /// decimal. Refused at authoring time; a non-positive quantum
    /// arriving through a variable is the runtime backstop
    /// `EvalError::RoundQuantumNotPositive`.
    #[error("round quantum must be a positive decimal, got {quantum} in {context}")]
    RoundQuantumNotPositive {
        quantum: String,
        context: ValidationContext,
    },
    /// An equality (`==` or `!=`) had two operands of distinct,
    /// incompatible kinds. Symmetric by nature: there is no
    /// "expected" side - both kinds are equally constrained by the
    /// other. `Subject == Decimal` is a kind error, not a silent
    /// coercion to false.
    #[error("{operator} operands must have the same kind; got {left} vs {right} in {context}")]
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
    #[error(
        "variable `{variable}` was first constrained to {previous} but later used as {new} in {context}"
    )]
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
    #[error(
        "`actor` is not available in {context}; it resolves only inside transformation bodies, so authority checks belong in a `require`"
    )]
    ActorNotAvailable { context: ValidationContext },
    /// A body in this context nests deeper than the validator's fixed
    /// maximum depth. The recursive evaluator and check walk descend
    /// one stack frame per nesting level, so a pathologically deep
    /// expression or `for`-statement chain would exhaust the stack
    /// during `propose`. Validation rejects it first, which is why
    /// untrusted IR must be validated before it is proposed.
    #[error("nesting in {context} exceeds the maximum depth of {}", MAX_EXPR_DEPTH)]
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
    #[error(
        "variable `{variable}` is used in {context} but nothing binds it{}",
        unbound_variable_hint(context)
    )]
    UnboundVariable {
        variable: String,
        context: ValidationContext,
    },
    /// A definition shares a name with a predicate. The two vocabularies
    /// share the claim-shaped reference namespace in body position - a
    /// reference `name(args)` resolves to exactly one of them - so a
    /// collision would let adding a definition silently change what
    /// existing text means.
    #[error(
        "definition `{name}` collides with predicate `{name}`; the two share the reference namespace in rule bodies, so a reference could mean either - rename one"
    )]
    DefinitionNameCollision { name: String },
    /// Definitions reference each other in a cycle. A definition is a
    /// named proposition expanded at evaluation; a cycle would never
    /// terminate. `names` carries one cycle's members in sorted order.
    #[error(
        "definitions reference each other in a cycle ({}); a definition must expand to claims and conditions, never back to itself",
        .names.join(", ")
    )]
    DefinitionCycle { names: Vec<String> },
    /// A reference names a definition where a predicate is required:
    /// a hand-built `Prop::Claim` that skipped resolution, or an
    /// `admit` / `retract` / `value` target. A definition names a
    /// condition - it is proposition-valued only, so it can be called
    /// in rule bodies but never changes state and never serves as a
    /// value lookup; hand-built IR constructs body calls as
    /// `Prop::Defined` (via `ir_builder::defined`) or runs
    /// [`crate::resolve_defined_calls`] before validating.
    #[error(
        "`{name}` names a definition where a predicate is required, in {context}; a definition is a condition - callable in rule bodies, never an `admit`/`retract`/`emit` target or a `value` lookup (hand-built body calls use `ir_builder::defined` or `resolve_defined_calls`)"
    )]
    UnresolvedDefinitionCall {
        name: String,
        context: ValidationContext,
    },
    /// A definition parameter is never referenced by the definition
    /// body. Such a parameter is dead weight at best; at worst a call
    /// passing an unbound variable for it is a guaranteed runtime
    /// error, since nothing could ever give it a value. (A parameter
    /// the body *uses* without binding is fine - it is a use-only
    /// parameter, required bound at every call site.)
    #[error(
        "parameter `{parameter}` of definition `{definition}` is not referenced by the definition body; remove it or reference it in a condition"
    )]
    ParameterNotReferenced {
        definition: String,
        parameter: String,
    },
    /// A definition declares the same parameter name twice. Each
    /// parameter is one binding slot in the call frame; a duplicate
    /// would let the later argument silently overwrite the earlier
    /// one during frame construction.
    #[error(
        "definition `{definition}` declares parameter `{parameter}` more than once; each parameter is one binding slot"
    )]
    DuplicateParameter {
        definition: String,
        parameter: String,
    },
    /// `pre(...)` was used inside a definition body. Definitions are
    /// context-free so a call means the same thing in a gate as in an
    /// invariant; a body that read pre-state would break that. Wrap the
    /// *call* in `pre(...)` instead - the context swap applies to the
    /// body's evaluation.
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
    /// A `superseded via` clause whose lineage predicate cannot carry
    /// the supersession chain: undeclared, not exactly two arguments
    /// in the `(successor, prior)` convention, or itself declared a
    /// current pointer.
    #[error(
        "`superseded via {lineage}` on `{pointer}`: {reason} (a lineage predicate has exactly two arguments, successor then prior, and is not itself a pointer)"
    )]
    DisciplineLineageUnfit {
        pointer: String,
        lineage: String,
        reason: String,
    },
    /// `superseded via` on a predicate that is not declared a current
    /// pointer: supersession history is the pointer's history, so the
    /// clause would be a dangling doctrine phrase anywhere else.
    #[error(
        "`superseded via` on `{predicate}`, which is not declared `current pointer by (...)`; supersession history is the pointer's history - declare the pointer, or drop the clause"
    )]
    DisciplineSupersededWithoutPointer { predicate: String },
    /// A transformation retracts a predicate that is append-only
    /// (declared, or the lineage of a `superseded via`). Ordinary
    /// programmes correct append-only claims by supersession or
    /// exception claims, never retraction.
    #[error(
        "{context} retracts `{predicate}`, which is append only; corrections are admitted as supersessions or exception claims, never by retracting the record"
    )]
    RetractsAppendOnly {
        predicate: String,
        context: ValidationContext,
    },
    /// A discipline that lowers to a generated invariant has no such
    /// invariant in the programme: the IR was hand-built and
    /// `lower_disciplines` was never run (the parser runs it), so the
    /// declared commitment would be silently unenforced.
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
            errors.extend(collect_discipline_errors(p));
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
    errors.extend(collect_discipline_errors(p));
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
        ValueExpr::Sum { body, .. } | ValueExpr::Extremum { body, .. } => {
            prop_depth_capped(body, inner, depths)?
        }
        ValueExpr::ValueOf { default, .. } => match default.as_deref() {
            Some(d) => value_depth_capped(d, inner, depths)?,
            None => 0,
        },
        ValueExpr::Abs(operand) => value_depth_capped(operand, inner, depths)?,
        ValueExpr::Round { value, quantum } => value_depth_capped(value, inner, depths)?
            .max(value_depth_capped(quantum, inner, depths)?),
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
        ValueExpr::Sum { body, .. } | ValueExpr::Extremum { body, .. } => {
            prop_exceeds_depth(body, budget, depths)
        }
        ValueExpr::ValueOf { default, .. } => default
            .as_deref()
            .is_some_and(|d| value_exceeds_depth(d, budget, depths)),
        ValueExpr::Abs(operand) => value_exceeds_depth(operand, budget, depths),
        ValueExpr::Round { value, quantum } => {
            value_exceeds_depth(value, budget, depths)
                || value_exceeds_depth(quantum, budget, depths)
        }
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

    // Each definition's parameter list must be duplicate-free: a
    // parameter is one binding slot in the call frame, so a repeated
    // name would let the later argument silently overwrite the
    // earlier one.
    for def in &p.definitions {
        let mut seen_params = HashMap::<&str, usize>::new();
        for param in &def.parameters {
            *seen_params.entry(param.as_str()).or_insert(0) += 1;
        }
        let mut dup_params: Vec<&str> = seen_params
            .iter()
            .filter(|(_, count)| **count > 1)
            .map(|(name, _)| *name)
            .collect();
        dup_params.sort_unstable();
        for parameter in dup_params {
            errors.push(ValidationError::DuplicateParameter {
                definition: def.name.to_string(),
                parameter: parameter.to_string(),
            });
        }
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

/// Name-level checks for declared disciplines, plus the static ban on
/// retracting append-only predicates. Lowering (`lower_disciplines`)
/// skips any clause flagged here; this pass owns the diagnostics.
fn collect_discipline_errors(p: &Program) -> Vec<ValidationError> {
    use crate::ir::Discipline;

    let mut errors = Vec::new();
    for decl in &p.predicates {
        // Duplicate detection compares clauses under a canonical key:
        // a uniqueness key is a SET, so `unique by (a, b)` and
        // `unique by (b, a)` are the same commitment (full agreement
        // does not depend on field order) and declaring both would
        // generate two same-meaning invariants under different names.
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
        Stmt::Require(_)
        | Stmt::BindOne(_)
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
                    context: ValidationContext::Transformation { name, .. }
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
