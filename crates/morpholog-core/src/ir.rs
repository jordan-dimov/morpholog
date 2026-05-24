//! IR types: the structural surface of a Morpholog programme.
//!
//! `Invariant`, `Expr`, `Term`, `Value`, `Claim`, `Intent`, `Stmt`,
//! `Transformation`, `Program`, `DerivedClaim`, `DerivedValue`, plus the
//! predicate-declaration types `PredicateDecl`, `ArgDecl`,
//! `PredicateArgKind`. These are pure data; runtime concerns (state,
//! evaluation, proposal execution, validation, persistence) live in
//! sibling modules.

use serde::{Deserialize, Serialize};

use crate::validate::{ValidationError, validate_program};

/// A named, versioned rule that must hold over admitted state. Invariants
/// are evaluated against the candidate state produced by a
/// [`Transformation`]; if any active invariant fails, the transformation is
/// rejected atomically.
///
/// The `version` field is carried from day one (v0 is `version: 1`
/// everywhere) so that audit rows can record exactly which invariant
/// version-set governed each committed transition. Adding versioning later
/// would be painful; the empty cost of carrying it now is cheap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invariant {
    pub name: String,
    pub version: u32,
    pub body: Expr,
}

/// Expression nodes used inside invariant bodies, transformation
/// requires, and let-bindings. An `Expr` evaluates against a state and a
/// binding set to yield either a truth-witness (predicate position) or a
/// value (value position).
///
/// The variants are deliberately narrow - composition, claim and
/// (in)equality matching, bounded aggregation, and one comparator or
/// arithmetic primitive per kind. Anything not expressible within this
/// set is, by design, not yet a runtime concern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    Claim {
        predicate: String,
        args: Vec<Term>,
    },
    Implies {
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Exists {
        binding: String,
        body: Box<Expr>,
    },
    And(Vec<Expr>),
    /// Predicate-shaped disjunction. Concatenates the binding sets each
    /// branch produces against the same base context; empty when every
    /// branch is empty. No deduplication (matches `And`'s convention).
    /// Flattened `Vec<Expr>` so `a or b or c` is one node.
    Or(Vec<Expr>),
    /// Evaluates the wrapped subtree against the pre-transition state
    /// instead of the candidate (post) state, so one invariant can
    /// relate pre and post values. Raises
    /// [`crate::EvalError::PreStateUnavailable`] where no pre-state is in
    /// scope (derived-claim bodies, transformation `require`s, a context
    /// built with `pre_state: None`, the inner of a nested `Pre`).
    ///
    /// Quantifier composition is non-commutative: `pre(forall x in C:
    /// ...)` resolves both the domain and the body against pre, while
    /// `forall x in C: pre(...)` iterates the post-state domain and flips
    /// only the body - they diverge when the iteration set changes.
    Pre(Box<Expr>),
    Not(Box<Expr>),
    Neq(Term, Term),
    Term(Term),
    Eq(Box<Expr>, Box<Expr>),
    /// Decimal comparators. Both operands must evaluate to
    /// `EvalValue::Decimal`. Predicate-shaped: the empty match set when
    /// the comparison is false, the unchanged binding set when true.
    /// `Le`/`Lt`/`Ge`/`Gt` are first-class rather than derived from a
    /// single primitive so each renders and round-trips as written -
    /// `amount > limit` stays `amount > limit`, never `not (amount <=
    /// limit)`.
    Le(Box<Expr>, Box<Expr>),
    /// Decimal strict less-than. See [`Expr::Le`].
    Lt(Box<Expr>, Box<Expr>),
    /// Decimal greater-than-or-equal. See [`Expr::Le`].
    Ge(Box<Expr>, Box<Expr>),
    /// Decimal strict greater-than. See [`Expr::Le`].
    Gt(Box<Expr>, Box<Expr>),
    /// Civil-date comparators. Both operands must evaluate to
    /// [`crate::EvalValue::Date`] (ISO-8601 `YYYY-MM-DD`, no time-of-day,
    /// no time zone). Predicate-shaped like the decimal comparators, but
    /// kept separate so each type-checks its own operands. Validity
    /// windows built from `DateLe(from, d)` and `DateLe(d, to)` are
    /// **inclusive at both ends**: `to == d` admits.
    DateLe(Box<Expr>, Box<Expr>),
    /// Civil-date strict before. See [`Expr::DateLe`].
    DateLt(Box<Expr>, Box<Expr>),
    /// Civil-date on-or-after. See [`Expr::DateLe`].
    DateGe(Box<Expr>, Box<Expr>),
    /// Civil-date strict after. See [`Expr::DateLe`].
    DateGt(Box<Expr>, Box<Expr>),
    /// Decimal subtraction; both operands must evaluate to
    /// `EvalValue::Decimal`, result is left minus right.
    Sub(Box<Expr>, Box<Expr>),
    /// Decimal addition; both operands must evaluate to
    /// `EvalValue::Decimal`, result is left plus right. With `Sub`, the
    /// whole decimal-arithmetic surface in v0 - no multiplication or
    /// division until an example forces them.
    Add(Box<Expr>, Box<Expr>),
    /// Sums `value` over every binding the `body` produces. `value` is
    /// usually a variable bound by the body (`sum(amount | ...)`); a
    /// decimal-literal `value` turns the sum into a count of matches
    /// (`sum(1 | ...)`).
    Sum {
        value: Term,
        body: Box<Expr>,
    },
    Forall {
        binding: String,
        source: Box<Expr>,
        body: Box<Expr>,
    },
    In(Term, Term),
    /// Reads exactly one matching claim and yields its value-position
    /// binding; wildcards in `args` mark the value position(s). Zero
    /// matches errors unless `default` is supplied; multiple matches
    /// always errors.
    ///
    /// Prefer [`Stmt::BindOne`] in transformation bodies (it rejects
    /// lawfully on zero matches, where `ValueOf` raises a kernel error).
    /// `ValueOf` is for value positions that are not statement-level
    /// binding extensions: inside `Sum`/`Add`/`Sub`/`Eq`/`Le`/`DateLe`,
    /// a `Let` value, or a `DerivedClaim` value expression.
    ValueOf {
        predicate: String,
        args: Vec<Term>,
        default: Option<Box<Expr>>,
    },
}

/// A positional argument in a claim, intent, or expression: a variable
/// bound by the surrounding context, a wildcard matching anything, a
/// literal constant, or `Actor`. `Term::Actor` resolves only inside a
/// transformation body; in an invariant it surfaces as
/// `EvalError::UnboundActor` - the require-vs-invariant doctrine made
/// enforceable: authority checks belong in `require`, not invariants.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Term {
    Var(String),
    Wildcard,
    Literal(Value),
    /// Resolves to the actor of the proposed transition. Available
    /// inside transformation bodies; not available inside invariant
    /// bodies.
    Actor,
}

/// Literal constants embeddable in IR `Term`s. Distinct from `EvalValue`
/// (a runtime value, including the booleans and collections that cannot
/// appear as IR literals).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// Arbitrary-precision decimal stored as its exact source string.
    /// Parsing into a numeric type is the evaluator's concern, not the IR's.
    Decimal(String),
    /// Opaque subject identifier embedded as a literal in the IR.
    /// Lets predicates and requires reference named constants
    /// (purposes, statuses, named authorities, etc.) without forcing
    /// every transformation to take them as extra parameters.
    Subject(String),
    /// ISO-8601 civil date (`YYYY-MM-DD`) stored as its exact source string.
    /// Parsing into [`jiff::civil::Date`] is the evaluator's concern, not the
    /// IR's; mirrors how [`Value::Decimal`] defers parsing to evaluation.
    /// No time-of-day, no time zone: validity-window modelling on civil
    /// dates is the only temporal primitive in v0.
    Date(String),
}

/// A Claim is an admitted assertion candidate - a statement that may be
/// admitted into governed state. It is not objective reality.
///
/// Distinct from `Expr::Claim`, which is a *query* over candidate state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub predicate: String,
    pub args: Vec<Term>,
}

/// An outbound effect declared by a transformation's `emit` statement.
/// Intents are *staged* during transformation execution and *enqueued*
/// to the outbox at commit time; they are never sent during the
/// transaction itself.
///
/// Distinct from [`crate::IntentInstance`], which is the resolved (no-variables)
/// form ready to be enqueued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    pub name: String,
    pub args: Vec<Term>,
}

/// One step inside a transformation body, run in declared order against
/// a binding context. `Require` and `BindOne` can short-circuit the
/// transformation; `Assert`, `Retract`, `Emit`, `Let`, `LetNewSubject`,
/// and `For` extend the staged outcome or the bindings. The
/// require/bind_one/let/for binding quartet is documented per variant
/// below and in full in `docs/runtime-semantics.md`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    Require(Expr),
    /// Deterministic unique-lookup binding statement. Evaluates a
    /// predicate-shaped expression against current state and bindings:
    /// - Zero matches: transformation rejected (lawful: the expected
    ///   governed record is absent).
    /// - One match: the returned binding set *replaces* the current
    ///   bindings; later statements see the newly-bound variables.
    /// - Multiple matches: `EvalError::TypeMismatch` (the programme
    ///   expected unique state but admitted ambiguous state - a missing
    ///   structural-uniqueness invariant, or corruption).
    BindOne(Expr),
    Let {
        name: String,
        value: Expr,
    },
    LetNewSubject {
        name: String,
    },
    Assert(Claim),
    /// Pattern-based retraction. Each Var in `args` is resolved against
    /// the current bindings; each Wildcard matches anything. All claims
    /// in the pre-state matching the resolved pattern are staged for
    /// retraction. Zero matches is an idempotent no-op (not an error).
    Retract {
        predicate: String,
        args: Vec<Term>,
    },
    For {
        binding: String,
        collection: Expr,
        body: Vec<Stmt>,
    },
    Emit(Intent),
}

/// A named, parameterised proposal to change admitted state. A
/// transformation is the only path by which governed state may change.
/// Its body is a sequence of [`Stmt`]s; when invoked via [`crate::propose`],
/// the body executes against a snapshot of pre-state, stages assertions
/// and retractions and intents, and produces an [`crate::Outcome`] that the
/// caller can either commit or discard.
///
/// Reads inside a transformation always see the *pre-transformation*
/// snapshot. Writes are staged and become real only at commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transformation {
    pub name: String,
    pub parameters: Vec<String>,
    pub body: Vec<Stmt>,
}

/// A governed domain model: a named set of predicate and intent
/// vocabularies, invariants, transformations, and derived claims,
/// packaged so the runtime, CLI, and external callers can refer to it as
/// one unit. It is the smallest possible container - it owns no state,
/// no connection, no schema, just the rules and the admitted
/// state-change paths. A caller proposes by looking up a transformation
/// by name and passing it to [`crate::propose`] (or the PostgreSQL
/// adapter's `propose_against_pg`) with the invariants and arguments.
/// `name` is a stable snake_case identifier the CLI selects on; each
/// worked example exposes a `program()` constructor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Program {
    pub name: String,
    /// The vocabulary of admissible claim shapes. Every `Expr::Claim`,
    /// `Stmt::Assert`, `Stmt::Retract`, `Expr::ValueOf`, and
    /// `DerivedClaim` output must target a declared predicate (validated
    /// by [`Program::validate`]).
    pub predicates: Vec<PredicateDecl>,
    /// The vocabulary of outbox intent shapes this programme may emit.
    /// Every `Stmt::Emit` must target a declared intent, so a misspelled
    /// name is a validation error, not a silent route-to-nowhere.
    /// Separate namespace from predicates.
    pub intents: Vec<IntentDecl>,
    pub invariants: Vec<Invariant>,
    pub transformations: Vec<Transformation>,
    pub derived_claims: Vec<DerivedClaim>,
}

impl Program {
    /// Look up a transformation by name. Returns `None` if no
    /// transformation in the program has that name.
    pub fn transformation(&self, name: &str) -> Option<&Transformation> {
        self.transformations.iter().find(|t| t.name == name)
    }

    /// Look up an invariant by name. Returns `None` if no invariant
    /// in the program has that name.
    pub fn invariant(&self, name: &str) -> Option<&Invariant> {
        self.invariants.iter().find(|i| i.name == name)
    }

    /// Look up a derived claim by predicate name. Returns `None` if no
    /// derived claim in the program has that name. Symmetric with
    /// [`Program::transformation`] and [`Program::invariant`].
    pub fn derived_claim(&self, name: &str) -> Option<&DerivedClaim> {
        self.derived_claims.iter().find(|d| d.predicate == name)
    }

    /// Look up a predicate declaration by name. Returns `None` if no
    /// declaration in the program has that name. Symmetric with the
    /// other lookup methods.
    ///
    /// If duplicate declarations exist this returns the first; the
    /// validator's arity lookup uses the last. Either way, duplicate
    /// declarations are invalid and are reported by
    /// [`Program::validate`] as `ValidationError::DuplicateDecl`.
    pub fn predicate(&self, name: &str) -> Option<&PredicateDecl> {
        self.predicates.iter().find(|p| p.name == name)
    }

    /// Look up an intent declaration by name. Returns `None` if no
    /// intent in the program has that name. Same duplicate-handling
    /// semantics as [`Self::predicate`].
    pub fn intent(&self, name: &str) -> Option<&IntentDecl> {
        self.intents.iter().find(|i| i.name == name)
    }

    /// Full static validation of the whole programme. Several checks
    /// contribute to a single error list:
    ///
    /// - **Structural**: every predicate and intent reference targets
    ///   a declaration at the declared arity; no two declarations in a
    ///   vocabulary share a name.
    /// - **Kind/type**: every value flowing into a slot, comparator,
    ///   or arithmetic operand carries a compatible kind; variables
    ///   refine-and-conflict across their uses; `Any` is unconstrained,
    ///   not a kind-eraser.
    /// - **Binding flow**: a name consumed where a bound value is
    ///   required must have been bound first, following the runtime
    ///   quartet's export rules.
    /// - **Shape**: a value-producing expression at a predicate
    ///   position, or the reverse.
    /// - **Actor context**: `Term::Actor` in an invariant or
    ///   derived-claim body, where no proposing transition is in scope.
    /// - **Nesting depth**: a body whose expressions or `for`-statements
    ///   nest past a fixed limit, which the recursive evaluator would
    ///   otherwise risk exhausting the stack on.
    ///
    /// Returns the **full** error list on failure (not just the
    /// first); a programme migration that adds declarations should
    /// see every site at once.
    ///
    /// Out of scope for v0: recursive derived-claim references from
    /// inside the same derived claim's domain; source spans on
    /// diagnostics (the IR drops parser spans on lowering).
    ///
    /// `validate` is **not** called automatically by `propose`. The
    /// kernel boundary is statement-level, not programme-level;
    /// adding a programme validation pass to every proposal would
    /// muddle that distinction. `morpholog check` runs it
    /// explicitly; tests on the built-in registry do the same.
    pub fn validate(&self) -> Result<(), Vec<ValidationError>> {
        validate_program(self)
    }
}

/// A predicate declaration: the name of a predicate and the named-and-
/// kinded shape of its argument list. Declarations appear in
/// [`Program::predicates`]; references appear inside `Expr::Claim`,
/// `Stmt::Assert`, `Stmt::Retract`, `Expr::ValueOf`, and `DerivedClaim`
/// output positions.
///
/// Argument *names* in a declaration are documentation - they describe
/// what each position means, surface in `morpholog inspect predicates`,
/// and inform future parser diagnostics. They have no runtime effect on
/// matching, which remains positional.
///
/// Argument *kinds* (see [`PredicateArgKind`]) are metadata recorded
/// for future use. Kind validation against the kinds of values flowing
/// through the binding context is not enforced in v0; recording the
/// metadata now means migrations stay shallow when kind checking
/// arrives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredicateDecl {
    pub name: String,
    pub args: Vec<ArgDecl>,
}

/// One argument-position declaration. Used by both
/// [`PredicateDecl`] and [`IntentDecl`]; both vocabularies share
/// the same `name`-plus-kind shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArgDecl {
    pub name: String,
    pub kind: PredicateArgKind,
}

/// A declaration of an outbox intent: the intent name and the
/// shape of its argument list. Declarations appear in
/// [`Program::intents`]; references appear inside [`Stmt::Emit`].
///
/// Mirrors [`PredicateDecl`] structurally - intents and predicates
/// have the same shape (named, kinded positional args) but live in
/// distinct vocabularies because they play distinct roles: predicates
/// describe admitted claim shapes, intents describe outbox-effect
/// shapes. The check pass validates `emit` arg kinds against
/// these declarations the same way it validates `assert` against
/// [`PredicateDecl`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentDecl {
    pub name: String,
    pub args: Vec<ArgDecl>,
}

/// The expected kind of a predicate argument position.
///
/// Deliberately a separate type from [`Value`] and [`crate::EvalValue`] - this
/// names a *declaration-time* expectation about an argument position,
/// not a runtime value or an IR literal. Conflating them in a single
/// enum was considered and rejected (CLAUDE.md: the
/// `Value`/`EvalValue` duality is already a delicate distinction; a
/// declaration-kind annotation should not be tangled into it).
///
/// `Any` is the kind escape hatch for argument positions whose kind
/// is genuinely polymorphic (e.g. a future audit-row payload that may
/// hold any admitted value), or for declarations that are not yet
/// ready to commit to a kind. Use it sparingly; the value of the
/// declaration metadata is highest when kinds are specific.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PredicateArgKind {
    Subject,
    Decimal,
    Date,
    Bool,
    Collection,
    Any,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedClaim {
    pub predicate: String,
    pub keys: Vec<String>,
    pub values: Vec<DerivedValue>,
    pub domain: Expr,
}

/// One computed value in a [`DerivedClaim`]. `name` is the variable
/// name within the derived claim's scope (used only for documentation
/// today; the output [`crate::ClaimInstance`] is positional, key values
/// followed by computed values in declaration order). `expr` is a
/// value-producing [`Expr`] that runs once per distinct key binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedValue {
    pub name: String,
    pub expr: Expr,
}
