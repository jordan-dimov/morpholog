//! IR types: the structural surface of a Morpholog programme.
//!
//! `Invariant`, `Expr`, `Term`, `Value`, `Claim`, `Intent`, `Stmt`,
//! `Transformation`, `Program`, `DerivedClaim`, `DerivedValue`, plus the
//! predicate-declaration types `PredicateDecl`, `PredicateArgDecl`,
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

/// Expression nodes used inside invariant bodies, transformation requires,
/// and let-bindings. An `Expr` is evaluated against a state and a set of
/// variable bindings to yield either a boolean / truth-witness (when used
/// as a predicate) or a value (when used in value position).
///
/// The variants are deliberately narrow: predicate composition (`And`,
/// `Or`, `Not`, `Implies`, `Exists`, `Forall`, `Pre`), claim and (in)equality matching
/// (`Claim`, `Neq`, `Eq`), one decimal-comparison primitive (`Le`), one
/// civil-date-comparison primitive (`DateLe`), one bounded aggregation
/// (`Sum`), one decimal-arithmetic primitive (`Sub`), one collection
/// primitive (`In`), one functional-lookup primitive (`ValueOf`), and
/// `Term`-as-value lifting. Anything that cannot be expressed within this
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
    /// instead of the candidate (post) state. The default for an
    /// invariant body is post; `Pre` flips the lookup for one subtree
    /// so a single invariant can relate pre and post values.
    ///
    /// Raises [`crate::EvalError::PreStateUnavailable`] when the
    /// evaluation context has no pre-state in scope: derived-claim
    /// bodies, transformation `require`s, evaluator calls whose
    /// `EvalContext` was constructed with `pre_state: None`, and the
    /// inner of a nested `Pre`.
    ///
    /// Quantifier composition is non-commutative: `pre(forall x in
    /// Squares: ...)` resolves both the iteration domain and the body
    /// against pre; `forall x in Squares: pre(...)` iterates the post-
    /// state domain and only flips the body. The two diverge when the
    /// iteration set itself changes between states.
    Pre(Box<Expr>),
    Not(Box<Expr>),
    Neq(Term, Term),
    Term(Term),
    Eq(Box<Expr>, Box<Expr>),
    /// Decimal less-than-or-equal. Both operands must evaluate to
    /// `EvalValue::Decimal`. Predicate-shaped: returns the empty match
    /// set when the comparison is false, the unchanged binding set when
    /// true. Added with the approval-limits worked example so that
    /// `require amount <= limit` can be expressed without smuggling
    /// quantitative authority into the bindings via `Eq` games.
    /// Deliberately the only decimal-comparison primitive in v0; `Lt`,
    /// `Gt`, `Ge` arrive when an example forces them.
    Le(Box<Expr>, Box<Expr>),
    /// Civil-date less-than-or-equal. Both operands must evaluate to
    /// [`crate::EvalValue::Date`]. Predicate-shaped: returns the empty match
    /// set when the comparison is false, the unchanged binding set when
    /// true. Dates are ISO-8601 civil dates (`YYYY-MM-DD`) with no
    /// time-of-day and no time zone. Validity windows modelled with
    /// `DateLe(from, action_date)` and `DateLe(action_date, to)` are
    /// **inclusive at both ends**: `effective_to == action_date` admits.
    ///
    /// Added with the clinical-trial-enrolment worked example so that
    /// admission can require a protocol version, consent form,
    /// eligibility evidence and investigator delegation that are all
    /// valid on the action date. Deliberately separate from `Le` to
    /// keep decimal and date ordering from sharing a generic-dispatch
    /// shape before a third comparator forces one. Deliberately the
    /// only date-comparison primitive in v0; `DateLt`, `DateGt`,
    /// `DateGe`, date arithmetic, instants, time zones, durations and
    /// business calendars arrive only when a worked example forces
    /// them.
    DateLe(Box<Expr>, Box<Expr>),
    /// Decimal subtraction. Both operands must evaluate to
    /// `EvalValue::Decimal`; the result is the left minus the right.
    /// Added with the trial-balance derived-claim example so that
    /// `balance == sum(debits) - sum(credits)` can be expressed without
    /// extending `Sum`'s value position into an expression sublanguage.
    Sub(Box<Expr>, Box<Expr>),
    /// Decimal addition. Both operands must evaluate to
    /// `EvalValue::Decimal`; the result is the left plus the right.
    /// Added with the insurance-claim-settlement example so that
    /// cumulative-cap rules like `sum(paid) + proposed <= aggregate`
    /// can be expressed directly, instead of contorting the natural
    /// rule into `proposed <= aggregate - sum(paid)`. Together with
    /// `Sub` this is the entire decimal-arithmetic surface in v0; no
    /// multiplication or division until a real example forces them.
    Add(Box<Expr>, Box<Expr>),
    Sum {
        value: Term,
        binding: String,
        body: Box<Expr>,
    },
    Forall {
        binding: String,
        source: Box<Expr>,
        body: Box<Expr>,
    },
    In(Term, Term),
    /// Reads exactly one matching claim and yields its value-position
    /// binding. Wildcards in `args` mark the value position(s). Zero
    /// matches is an error unless `default` is supplied; multiple
    /// matches is always an error.
    ///
    /// **Prefer [`Stmt::BindOne`] in transformation bodies.** When
    /// the goal is to extract a uniquely-matching claim's values
    /// into the statement-level binding context, `bind_one` reads
    /// more directly and rejects lawfully on zero matches (where
    /// `ValueOf` raises a kernel error). `ValueOf` remains the
    /// right tool for **value-producing positions** that aren't
    /// statement-level binding extensions: inside `Sum`, `Add`,
    /// `Sub`, `Eq`, `Le`, or `DateLe` expressions, inside a `Let`
    /// computing a derived value, or inside a `DerivedClaim` value
    /// expression where a statement form does not fit.
    ValueOf {
        predicate: String,
        args: Vec<Term>,
        default: Option<Box<Expr>>,
    },
}

/// A positional argument in a claim, intent, or expression. A `Term` is
/// either a variable to be bound by the surrounding context, a wildcard
/// that matches anything, a literal constant, or `Actor` - a reserved
/// term that resolves to the actor of the proposed transition.
///
/// `Term::Actor` is only resolvable inside a transformation body
/// (require, let, assert, retract, emit, for). Invariant bodies do not
/// have a transition in scope; `Term::Actor` used inside an invariant
/// surfaces as `EvalError::UnboundActor` at evaluation time. This is
/// the require-vs-invariant doctrine made enforceable: authority
/// checks belong in `require`, not in invariants.
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
/// (which is a runtime value, including booleans and collections that
/// cannot appear as IR literals). The variants are deliberately narrow:
/// each was added when a worked example forced it.
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

/// One step inside a transformation body. Statements run in declared
/// order against a binding context; a failing `Require` or `BindOne`
/// short-circuits the transformation, while `Assert`, `Retract`,
/// `Emit`, `Let`, `LetNewSubject`, and `For` extend the staged
/// outcome or the binding context. `Retract` of a non-existent claim
/// is an idempotent no-op (see the variant doc), not a short-circuit.
///
/// The statement-level binding doctrine is a four-way carve:
///
/// - [`Stmt::Require`] is a yes/no gate; bindings unchanged on
///   success, transformation rejected on failure.
/// - [`Stmt::BindOne`] is a deterministic unique lookup; on
///   success the current binding context is *replaced* with the
///   matching binding set, transformation rejected on zero matches,
///   kernel error on multiple matches.
/// - [`Stmt::Let`] computes a value-producing expression and binds
///   its result under a new variable name.
/// - [`Stmt::For`] iterates over a collection, executing its body
///   once per element.
///
/// See `docs/runtime-semantics.md` for the require/bind_one/let/for
/// quartet in full.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    Require(Expr),
    /// Deterministic unique-lookup binding statement. Evaluates a
    /// predicate-shaped expression against current state and
    /// bindings; the surviving binding set is treated as the next
    /// binding context.
    ///
    /// Semantics:
    /// - Zero matches: transformation rejected (lawful business
    ///   outcome; the expected governed record is not present).
    /// - One match: the returned binding set *replaces* the current
    ///   bindings. Statements after a successful `BindOne` see the
    ///   newly-bound variables.
    /// - Multiple matches: `EvalError::TypeMismatch` (kernel error;
    ///   the programme expected unique state but admitted ambiguous
    ///   state - missing structural-uniqueness invariant or
    ///   corruption).
    ///
    /// Replaces the `require + let + value_of` chain that previous
    /// examples used to extract a uniquely-matching claim's values
    /// into the binding context. Added with the insurance-claim-
    /// settlement migration; see `docs/design-history.md`.
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

/// A governed domain model: a named set of invariants and named
/// transformations, packaged together so the runtime, the CLI, and
/// external callers can refer to it as one unit.
///
/// `Program` is deliberately the smallest possible container. It
/// does not own any state, does not own a connection, does not own
/// a schema. It is just the set of rules and the set of admitted
/// state-change paths that make up one governed model. A caller
/// proposes a transformation against a `Program` by looking up the
/// transformation by name and passing it to [`crate::propose`] (or to the
/// PostgreSQL adapter's `propose_against_pg`) together with the
/// program's `invariants` and the arguments.
///
/// Each worked example exposes a `program()` constructor that
/// returns its `Program`. Whether `Program`s are eventually loaded
/// from `.morph` source files, or assembled programmatically, or
/// both, is a later decision; the type is the smallest stable
/// surface for naming "a governed domain model" today.
///
/// `name` is a stable identifier (snake_case is conventional; the
/// built-in examples use `"settlement_netting"`, `"revenue_restatement"`,
/// `"claim_standing"`, `"double_entry_ledger"`). The CLI uses it to
/// select a program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub name: String,
    /// The vocabulary of admissible claim shapes for this programme.
    /// Every `Expr::Claim`, `Stmt::Assert`, `Stmt::Retract`, and
    /// `Expr::ValueOf` reference must target a declared predicate
    /// (validated by [`Program::validate`]); every `DerivedClaim`'s
    /// output predicate must also appear here. Intent declarations
    /// are deliberately out of scope - intents are outbox effects,
    /// not admitted-claim vocabulary; that distinction is captured
    /// here rather than papered over.
    pub predicates: Vec<PredicateDecl>,
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

    /// Structural and kind/type validation of the whole programme.
    /// Two layers run and contribute to a single error list:
    ///
    /// - **Structural**: every claim reference targets a declared
    ///   predicate at the declared arity; no two declarations share
    ///   a name.
    /// - **Kind/type**: every value flowing into a slot carries a
    ///   compatible kind, comparator and arithmetic operands match
    ///   the expected kind, variables refine-and-conflict across
    ///   claim and let uses, `Sum`/`Forall`/`Exists` bindings
    ///   shadow correctly.
    ///
    /// Returns the **full** error list on failure (not just the
    /// first); a programme migration that adds declarations should
    /// see every site at once.
    ///
    /// Out of scope for v0: intent arity validation (intents are
    /// outbox vocabulary, awaiting an `IntentDecl`); recursive
    /// derived-claim references from inside the same derived
    /// claim's domain; source spans on diagnostics (the IR drops
    /// parser spans on lowering).
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
    pub args: Vec<PredicateArgDecl>,
}

/// One argument-position declaration in a [`PredicateDecl`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PredicateArgDecl {
    pub name: String,
    pub kind: PredicateArgKind,
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
