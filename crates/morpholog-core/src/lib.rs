//! Morpholog v0 semantic kernel.
//!
//! This crate is the synchronous, pure heart of Morpholog. It defines
//! the IR (invariants, transformations, claims, statements, expressions),
//! evaluates invariants against in-memory state, and exposes [`propose`]
//! — the function that turns a proposed transformation into either an
//! accepted post-state or a rejected attempt.
//!
//! `morpholog-core` does no I/O. The PostgreSQL persistence adapter
//! lives in the separate `morpholog-postgres` crate and wraps this
//! kernel as an async boundary. Worked-example IR lives in the
//! `morpholog-examples` crate.

use jiff::civil::Date;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::str::FromStr;

pub mod dsl;
pub mod format;

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
/// `Not`, `Implies`, `Exists`, `Forall`), claim and (in)equality matching
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
    /// [`EvalValue::Date`]. Predicate-shaped: returns the empty match
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

/// A Claim is an admitted assertion candidate — a statement that may be
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
/// Distinct from [`IntentInstance`], which is the resolved (no-variables)
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
/// Its body is a sequence of [`Stmt`]s; when invoked via [`propose`],
/// the body executes against a snapshot of pre-state, stages assertions
/// and retractions and intents, and produces an [`Outcome`] that the
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
/// transformation by name and passing it to [`propose`] (or to the
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
    pub fn predicate(&self, name: &str) -> Option<&PredicateDecl> {
        self.predicates.iter().find(|p| p.name == name)
    }

    /// Structural validation: every predicate referenced in any
    /// transformation body, invariant body, or derived-claim shape
    /// must be declared in [`Program::predicates`], and every
    /// reference must match the declared arity.
    ///
    /// Strict mode: undeclared predicates are an error, not a
    /// passthrough. The cost is that every example must enumerate
    /// its predicates; the benefit is that a programme's vocabulary
    /// becomes a real self-describing contract and typos surface at
    /// validation time rather than at runtime.
    ///
    /// Returns `Ok(())` when no errors are found. Returns the
    /// **full** error list on failure (not just the first); a
    /// programme migration that adds predicate declarations should
    /// see every missing or mismatched site at once.
    ///
    /// Out of scope for v0:
    /// - Argument-kind checking against [`PredicateArgKind`]. The
    ///   metadata is recorded for future use (docs, CLI inspection,
    ///   future parser diagnostics, eventual kind validation that
    ///   would require tracking variable kinds through binding
    ///   contexts).
    /// - Intent arity validation. Intents are outbox vocabulary,
    ///   not claim vocabulary; an `IntentDecl` is conceivable but
    ///   not pursued until a worked example needs it.
    /// - Predicate references that name a derived-claim predicate
    ///   recursively from inside the derived claim's own domain.
    ///   Recursion through derived claims is on the deferred list.
    ///
    /// `validate` is **not** called automatically by `propose`. The
    /// kernel boundary is statement-level, not programme-level;
    /// adding a programme validation pass to every proposal would
    /// muddle that distinction and add overhead. Tests on the
    /// built-in registry call `validate` explicitly.
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
/// Deliberately a separate type from [`Value`] and [`EvalValue`] - this
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
    /// A predicate referenced somewhere in the programme is not
    /// listed in `Program::predicates`. Strict mode: every reference
    /// must have a declaration.
    UndeclaredPredicate {
        predicate: String,
        context: ValidationContext,
    },
    /// A predicate reference passes a different number of arguments
    /// than the declaration calls for.
    ArityMismatch {
        predicate: String,
        expected: usize,
        actual: usize,
        context: ValidationContext,
    },
    /// Two `PredicateDecl`s in `Program::predicates` share the same
    /// name. Even if both declarations agree on arity, the duplicate
    /// is a modelling bug.
    DuplicatePredicateDecl { predicate: String },
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
            ValidationError::UndeclaredPredicate { predicate, context } => write!(
                f,
                "undeclared predicate `{predicate}` referenced in {context}"
            ),
            ValidationError::ArityMismatch {
                predicate,
                expected,
                actual,
                context,
            } => write!(
                f,
                "predicate `{predicate}` declared with arity {expected} \
                 but referenced with {actual} args in {context}"
            ),
            ValidationError::DuplicatePredicateDecl { predicate } => {
                write!(f, "duplicate predicate declaration for `{predicate}`")
            }
        }
    }
}

impl std::error::Error for ValidationError {}

/// Strict arity validation for a [`Program`]. See [`Program::validate`].
fn validate_program(p: &Program) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    // 1. Duplicate predicate declarations.
    let mut seen = HashMap::<&str, usize>::new();
    for decl in &p.predicates {
        *seen.entry(decl.name.as_str()).or_insert(0) += 1;
    }
    for (name, count) in &seen {
        if *count > 1 {
            errors.push(ValidationError::DuplicatePredicateDecl {
                predicate: (*name).to_string(),
            });
        }
    }

    // Build a name -> arity lookup once. If duplicates exist, the last
    // declaration wins for arity-lookup purposes; the duplicate error
    // above already surfaces the problem.
    let arities: HashMap<&str, usize> = p
        .predicates
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
            validate_stmt(stmt, &arities, &ctx, &mut errors);
        }
    }

    // 4. Walk each derived claim: output predicate declared, output
    //    arity matches keys + values count, domain references validated.
    for d in &p.derived_claims {
        let ctx = ValidationContext::DerivedClaim {
            predicate: d.predicate.clone(),
        };
        match arities.get(d.predicate.as_str()) {
            None => errors.push(ValidationError::UndeclaredPredicate {
                predicate: d.predicate.clone(),
                context: ctx.clone(),
            }),
            Some(&decl_arity) => {
                let actual_arity = d.keys.len() + d.values.len();
                if decl_arity != actual_arity {
                    errors.push(ValidationError::ArityMismatch {
                        predicate: d.predicate.clone(),
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

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

/// Walk a statement and collect arity/declaration errors.
fn validate_stmt(
    stmt: &Stmt,
    arities: &HashMap<&str, usize>,
    ctx: &ValidationContext,
    errors: &mut Vec<ValidationError>,
) {
    match stmt {
        Stmt::Require(e) | Stmt::BindOne(e) => validate_expr(e, arities, ctx, errors),
        Stmt::Let { value, .. } => validate_expr(value, arities, ctx, errors),
        Stmt::LetNewSubject { .. } => {}
        Stmt::Assert(c) => {
            check_predicate(&c.predicate, c.args.len(), arities, ctx, errors);
        }
        Stmt::Retract { predicate, args } => {
            check_predicate(predicate, args.len(), arities, ctx, errors);
        }
        Stmt::For {
            collection, body, ..
        } => {
            validate_expr(collection, arities, ctx, errors);
            for inner in body {
                validate_stmt(inner, arities, ctx, errors);
            }
        }
        Stmt::Emit(_) => {
            // Intents are not part of the claim vocabulary; an
            // IntentDecl is a future, separate concept.
        }
    }
}

/// Walk an expression and collect arity/declaration errors.
fn validate_expr(
    expr: &Expr,
    arities: &HashMap<&str, usize>,
    ctx: &ValidationContext,
    errors: &mut Vec<ValidationError>,
) {
    match expr {
        Expr::Claim { predicate, args } => {
            check_predicate(predicate, args.len(), arities, ctx, errors);
        }
        Expr::ValueOf {
            predicate,
            args,
            default,
        } => {
            check_predicate(predicate, args.len(), arities, ctx, errors);
            if let Some(d) = default {
                validate_expr(d, arities, ctx, errors);
            }
        }
        Expr::Implies { left, right } => {
            validate_expr(left, arities, ctx, errors);
            validate_expr(right, arities, ctx, errors);
        }
        Expr::And(exprs) => {
            for e in exprs {
                validate_expr(e, arities, ctx, errors);
            }
        }
        Expr::Not(e) | Expr::Exists { body: e, .. } => {
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

/// Helper: emit the right error variant for a predicate call site.
fn check_predicate(
    predicate: &str,
    actual: usize,
    arities: &HashMap<&str, usize>,
    ctx: &ValidationContext,
    errors: &mut Vec<ValidationError>,
) {
    match arities.get(predicate) {
        None => errors.push(ValidationError::UndeclaredPredicate {
            predicate: predicate.to_string(),
            context: ctx.clone(),
        }),
        Some(&expected) if expected != actual => {
            errors.push(ValidationError::ArityMismatch {
                predicate: predicate.to_string(),
                expected,
                actual,
                context: ctx.clone(),
            });
        }
        Some(_) => {}
    }
}

/// A computed view over admitted state, packaged as part of a
/// [`Program`]. The shape is keys-and-values: `keys` are the
/// enumerated/grouping dimensions, `values` are the per-key computed
/// expressions, and `domain` is the expression whose satisfying
/// bindings define the set of distinct keys.
///
/// Evaluating a derived claim against a [`State`] (via
/// [`enumerate_derived`]) produces one [`ClaimInstance`] per distinct
/// key tuple: its `predicate` is the derived claim's `predicate`, and
/// its `args` are the key values followed by the computed value
/// expressions evaluated under the per-key bindings.
///
/// In v0 a derived claim's output [`ClaimInstance`]s are *not* added
/// to [`State::claims`], *not* visible to invariants or
/// transformations, *not* persisted by the PostgreSQL adapter, and
/// *not* recursively referenceable from another derived claim's body.
/// See `docs/design-history.md` for the derived-claims retrospective
/// and what derived claims forced into the IR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedClaim {
    pub predicate: String,
    pub keys: Vec<String>,
    pub values: Vec<DerivedValue>,
    pub domain: Expr,
}

/// One computed value in a [`DerivedClaim`]. `name` is the variable
/// name within the derived claim's scope (used only for documentation
/// today; the output [`ClaimInstance`] is positional, key values
/// followed by computed values in declaration order). `expr` is a
/// value-producing [`Expr`] that runs once per distinct key binding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedValue {
    pub name: String,
    pub expr: Expr,
}

// ===========================================================================
// In-memory evaluator
// ===========================================================================

/// A runtime value flowing through evaluation. Distinct from the IR's
/// `Value` (which holds literals only).
///
/// JSON encoding uses an adjacently-tagged shape
/// (`{ "type": "...", "value": ... }`), suitable for the PG JSONB columns
/// defined in `crates/morpholog-core/sql/schema.sql`. Decimals serialise
/// as JSON **strings** to preserve exactness; never as JSON numbers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "lowercase")]
pub enum EvalValue {
    Decimal(#[serde(with = "rust_decimal::serde::str")] Decimal),
    Subject(String),
    Bool(bool),
    Collection(Vec<EvalValue>),
    /// Civil date (ISO-8601 `YYYY-MM-DD`) with no time-of-day and no
    /// time zone. JSON shape: `{ "type": "date", "value": "YYYY-MM-DD" }`
    /// (jiff's default serde format for [`jiff::civil::Date`]).
    Date(Date),
}

/// A grounded claim: all args are values, no variables or wildcards.
///
/// JSON encoding shape: `{ "predicate": "...", "args": [ ... ] }`.
///
/// Used as-is for elements of `audit.asserted_claims` and
/// `audit.retracted_claims` (each column is a JSONB array of these objects).
///
/// For row writes to the `claims` table itself, the PG adapter **splits**
/// the claim across two columns: `predicate_name` (text, from `predicate`)
/// and `arguments` (JSONB array, from `args`). The `arguments` column has
/// a CHECK constraint that requires `jsonb_typeof(arguments) = 'array'`,
/// so writing the full object there would fail. The `claim_args_serialise_as_a_json_array`
/// test pins this contract.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ClaimInstance {
    pub predicate: String,
    pub args: Vec<EvalValue>,
}

/// The admitted state of the runtime: a set of grounded [`ClaimInstance`]s
/// against which invariants are evaluated and transformations are
/// proposed. State is set-valued: identity is `(predicate, args)`. The
/// PG adapter persists this set as rows in `morpholog.claims`; this
/// in-memory representation is what the kernel evaluates against.
///
/// Internally indexed by predicate name AND by `(predicate, arg
/// position, arg value)` to support ground-argument lookup. Construct
/// via [`State::from_claims`] or [`State::default`]; mutation is not
/// part of the API (the indexes would otherwise go stale). The
/// public accessors are [`State::claims`] (all admitted claims, in
/// construction order) and [`State::claims_for`] (`O(1)` lookup of
/// all claims for a given predicate). Argument-position lookup is
/// internal to the kernel and used by `find_claim_matches` to narrow
/// the candidate set when any argument is already ground.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct State {
    claims: Vec<ClaimInstance>,
    by_predicate: HashMap<String, PredicateIndex>,
}

/// Per-predicate index entry stored on [`State`]. Holds the
/// construction-order positions of every claim with this predicate,
/// plus a secondary index keyed on `(arg position, arg value)` for
/// ground-argument lookup.
///
/// `by_arg` grows lazily as predicates of varying arity are observed:
/// position `p` gets a map only when some claim of this predicate has
/// at least `p + 1` args.
#[derive(Clone, Default, PartialEq, Eq)]
struct PredicateIndex {
    /// Indices into `State.claims` for every claim with this predicate.
    all: Vec<usize>,
    /// `by_arg[position][value]` -> indices into `State.claims` for
    /// claims with this predicate where `args[position] == value`.
    by_arg: Vec<HashMap<EvalValue, Vec<usize>>>,
}

impl std::fmt::Debug for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("State")
            .field("claims", &self.claims)
            .finish_non_exhaustive()
    }
}

impl State {
    /// Build a `State` from a vector of admitted claims. Builds two
    /// indexes during construction: a per-predicate bucket of claim
    /// positions, and a per-`(predicate, arg position, arg value)`
    /// bucket of claim positions for ground-argument lookup. Both
    /// indexes are immutable thereafter; the State itself is
    /// immutable.
    pub fn from_claims(claims: Vec<ClaimInstance>) -> Self {
        let mut by_predicate: HashMap<String, PredicateIndex> = HashMap::new();
        for (i, c) in claims.iter().enumerate() {
            let entry = by_predicate.entry(c.predicate.clone()).or_default();
            entry.all.push(i);
            if entry.by_arg.len() < c.args.len() {
                entry.by_arg.resize_with(c.args.len(), HashMap::new);
            }
            for (pos, value) in c.args.iter().enumerate() {
                entry.by_arg[pos].entry(value.clone()).or_default().push(i);
            }
        }
        Self {
            claims,
            by_predicate,
        }
    }

    /// All admitted claims, in the order supplied to
    /// [`State::from_claims`]. Read-only.
    pub fn claims(&self) -> &[ClaimInstance] {
        &self.claims
    }

    /// Iterator over every admitted claim whose predicate name matches
    /// `predicate`. `O(1)` to find the bucket; iteration is linear in
    /// the bucket's size. Returns an empty iterator when no claims of
    /// that predicate are admitted.
    pub fn claims_for<'a>(
        &'a self,
        predicate: &str,
    ) -> impl Iterator<Item = &'a ClaimInstance> + 'a {
        self.by_predicate
            .get(predicate)
            .map(|idx| idx.all.iter().map(|&i| &self.claims[i]))
            .into_iter()
            .flatten()
    }

    /// Indices into `claims()` for every claim where `predicate`
    /// matches AND `args[position] == value`. `O(1)` lookup. Returns
    /// `None` when no claim of this predicate has this value at this
    /// position, which the caller uses to short-circuit an empty
    /// intersection. Internal to the kernel; used by
    /// `find_claim_matches` to narrow the candidate set when at least
    /// one argument is already ground (a literal in the IR, or a
    /// variable already bound in the surrounding context).
    pub(crate) fn claim_indices_for_arg(
        &self,
        predicate: &str,
        position: usize,
        value: &EvalValue,
    ) -> Option<&[usize]> {
        self.by_predicate
            .get(predicate)
            .and_then(|idx| idx.by_arg.get(position))
            .and_then(|m| m.get(value))
            .map(|v| v.as_slice())
    }

    /// Look up a claim by its `claims()` index. Used internally
    /// alongside [`State::claim_indices_for_arg`] when iterating an
    /// argument-position bucket.
    pub(crate) fn claim_at(&self, index: usize) -> &ClaimInstance {
        &self.claims[index]
    }

    /// Total number of admitted claims across all predicates.
    pub fn len(&self) -> usize {
        self.claims.len()
    }

    pub fn is_empty(&self) -> bool {
        self.claims.is_empty()
    }
}

/// Variable bindings used during expression evaluation and
/// transformation execution. Maps variable name to resolved
/// [`EvalValue`].
pub type Bindings = HashMap<String, EvalValue>;

/// Errors raised by the evaluator and the transformation runner. These
/// are distinct from *lawful business rejection* (which is reported as
/// [`Outcome::Rejected`]); an `EvalError` indicates that an expression
/// or transformation was structurally ill-formed and cannot be run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalError {
    /// A variable was referenced before being bound by a parameter,
    /// `let`, `for`, or `exists` binding.
    UnboundVariable(String),
    /// An expression demanded an operand of one kind but received
    /// another (e.g. arithmetic on a subject, membership on a non-
    /// collection, etc.).
    TypeMismatch(String),
    /// An expression that must be predicate-shaped (boolean-valued)
    /// was used in a position that cannot interpret it.
    NotPredicate,
    /// An expression that must be value-producing was used in a
    /// position that requires a value (e.g. as a `let` right-hand side
    /// or a sum target).
    NotValue,
    /// `Expr::ValueOf(predicate, args)` matched zero claims and no
    /// `default` was supplied.
    ValueOfZeroMatches(String),
    /// `Expr::ValueOf(predicate, args)` matched more than one claim;
    /// the functional-lookup contract requires exactly one match.
    ValueOfMultipleMatches(String),
    /// `Term::Actor` was referenced in a context that has no transition
    /// in scope - any path that calls into the evaluator with
    /// `actor = None`. The common cases are invariant bodies and
    /// derived-claim bodies (both evaluate against admitted state, not
    /// against any specific proposing transition). Authority checks
    /// belong in `require`, not in invariants; this error makes that
    /// doctrine enforceable rather than convention.
    UnboundActor,
}

impl std::fmt::Display for EvalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EvalError::UnboundVariable(name) => write!(f, "unbound variable: {name}"),
            EvalError::TypeMismatch(msg) => write!(f, "type mismatch: {msg}"),
            EvalError::NotPredicate => write!(f, "expression is not a predicate"),
            EvalError::NotValue => write!(f, "expression is not value-producing"),
            EvalError::ValueOfZeroMatches(p) => {
                write!(f, "value({p}, _): zero matches")
            }
            EvalError::ValueOfMultipleMatches(p) => {
                write!(f, "value({p}, _): multiple matches")
            }
            EvalError::UnboundActor => write!(
                f,
                "Term::Actor referenced with no transition in scope (likely used outside a transformation body - e.g., inside an invariant or derived-claim body; authority checks belong in `require`)"
            ),
        }
    }
}

impl std::error::Error for EvalError {}

/// Evaluate an invariant against a state. Returns true if the invariant
/// holds, false if it fails.
pub fn eval_invariant(inv: &Invariant, state: &State) -> Result<bool, EvalError> {
    let bindings = Bindings::new();
    // Invariants evaluate against admitted state with no transition in
    // scope. `Term::Actor` inside an invariant body surfaces as
    // `EvalError::UnboundActor`, enforcing the doctrine that authority
    // checks live in `require`, not in invariants.
    let matches = find_matches(&inv.body, state, &bindings, None)?;
    Ok(!matches.is_empty())
}

/// Enumerate a derived claim against current admitted state. Returns
/// one [`ClaimInstance`] per distinct key tuple, in deterministic key
/// order.
///
/// Algorithm:
///
/// 1. Run `find_matches` on `derived.domain` to get every binding that
///    satisfies the domain expression.
/// 2. Project each binding onto the `derived.keys` and deduplicate.
///    The deduplication uses a `BTreeSet`, which also gives the output
///    a stable ordering by key tuple.
/// 3. For each distinct key binding, evaluate each
///    [`DerivedValue::expr`] via the internal value evaluator under that
///    binding. Append the resulting values to the key tuple to form
///    the output `ClaimInstance`.
///
/// Errors propagate from the underlying evaluator: a non-decimal
/// `Sub`, a missing key binding, a malformed body expression, etc.
///
/// Returned `ClaimInstance`s are *not* added to `state.claims`. The
/// caller decides what to do with them; in v0 nothing else in the
/// runtime sees them.
pub fn enumerate_derived(
    derived: &DerivedClaim,
    state: &State,
) -> Result<Vec<ClaimInstance>, EvalError> {
    // Derived claims, like invariants, evaluate against admitted state
    // with no transition in scope. `Term::Actor` in a derived claim body
    // surfaces as `EvalError::UnboundActor`.
    let raw_bindings = find_matches(&derived.domain, state, &Bindings::new(), None)?;

    let mut key_tuples: BTreeSet<Vec<EvalValueOrd>> = BTreeSet::new();
    for b in &raw_bindings {
        let mut tuple = Vec::with_capacity(derived.keys.len());
        for k in &derived.keys {
            let v = b.get(k).ok_or_else(|| {
                EvalError::UnboundVariable(format!(
                    "derived claim `{}`: key `{}` not bound by domain expression",
                    derived.predicate, k
                ))
            })?;
            tuple.push(EvalValueOrd(v.clone()));
        }
        key_tuples.insert(tuple);
    }

    let mut out: Vec<ClaimInstance> = Vec::with_capacity(key_tuples.len());
    for tuple in key_tuples {
        let mut per_key = Bindings::new();
        for (k, v) in derived.keys.iter().zip(tuple.iter()) {
            per_key.insert(k.clone(), v.0.clone());
        }
        let mut args: Vec<EvalValue> = tuple.iter().map(|w| w.0.clone()).collect();
        for value_def in &derived.values {
            let v = eval_value(&value_def.expr, state, &per_key, None)?;
            args.push(v);
        }
        out.push(ClaimInstance {
            predicate: derived.predicate.clone(),
            args,
        });
    }
    Ok(out)
}

/// Return the set of predicate names this expression references
/// anywhere in its tree. Used by the PostgreSQL adapter's read path
/// to load only the claims a derived-claim enumeration needs,
/// instead of fetching the whole `morpholog.claims` table.
///
/// The match below is **exhaustive over `Expr` variants on purpose**
/// (no `_` arm). If a future PR adds a new `Expr` variant, the
/// compiler will refuse this function until the new variant is
/// handled. That compile-time check is what keeps the analysis
/// honest: a missed variant here would silently produce
/// wrong-answer bugs at runtime - the read path would skip claims
/// the kernel actually needs, and `enumerate_derived` would return
/// an answer computed against an incomplete state.
///
/// `Neq`, `Term`, and `In` take only `Term`s (variables, wildcards,
/// or literals), none of which can reference a predicate; they
/// contribute nothing.
pub fn predicates_referenced_by_expr(expr: &Expr, out: &mut BTreeSet<String>) {
    match expr {
        Expr::Claim { predicate, .. } => {
            out.insert(predicate.clone());
        }
        Expr::ValueOf {
            predicate, default, ..
        } => {
            out.insert(predicate.clone());
            if let Some(d) = default {
                predicates_referenced_by_expr(d, out);
            }
        }
        Expr::Implies { left, right } => {
            predicates_referenced_by_expr(left, out);
            predicates_referenced_by_expr(right, out);
        }
        Expr::And(exprs) => {
            for e in exprs {
                predicates_referenced_by_expr(e, out);
            }
        }
        Expr::Not(e) | Expr::Exists { body: e, .. } => {
            predicates_referenced_by_expr(e, out);
        }
        Expr::Eq(l, r)
        | Expr::Le(l, r)
        | Expr::DateLe(l, r)
        | Expr::Sub(l, r)
        | Expr::Add(l, r) => {
            predicates_referenced_by_expr(l, out);
            predicates_referenced_by_expr(r, out);
        }
        Expr::Sum { body, .. } => {
            predicates_referenced_by_expr(body, out);
        }
        Expr::Forall { source, body, .. } => {
            predicates_referenced_by_expr(source, out);
            predicates_referenced_by_expr(body, out);
        }
        Expr::Neq(_, _) | Expr::Term(_) | Expr::In(_, _) => {
            // No predicate references; operate on Terms only.
        }
    }
}

/// Return the set of predicate names that `enumerate_derived(derived,
/// state)` will need to read out of `state`. Computed as the union of
/// the `domain` expression's referenced predicates and every
/// `DerivedValue.expr`'s referenced predicates.
///
/// The `predicate` field on the derived claim itself is **not**
/// included: that names the OUTPUT predicate of the enumeration,
/// which the kernel never reads from state. Including it would tell
/// callers to load claims they have no use for.
pub fn predicates_referenced_by_derived(derived: &DerivedClaim) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    predicates_referenced_by_expr(&derived.domain, &mut out);
    for v in &derived.values {
        predicates_referenced_by_expr(&v.expr, &mut out);
    }
    out
}

/// `EvalValue` does not derive `Ord`. Wrap it in a newtype that
/// implements `Ord` *structurally* so we can deduplicate key tuples
/// in a `BTreeSet` without committing the kernel's runtime-value
/// type to a sort order externally. Used only inside
/// [`enumerate_derived`]; not exposed.
///
/// The ordering is infallible and `Eq`-consistent:
/// - Variants order as `Decimal < Subject < Bool < Collection`.
/// - Within `Decimal`, the natural decimal ordering applies (so
///   `100` sorts before `200`, not lexicographic on the string).
/// - Within `Subject`, the natural string ordering applies.
/// - Within `Bool`, `false < true` (the derived `Ord` on `bool`).
/// - Within `Collection`, lexicographic on elements with the same
///   structural ordering applied recursively; shorter tuples
///   sort before longer when one is a prefix of the other.
///
/// The contract that `enumerate_derived` makes about output order
/// is *determinism*. Callers that need a specific business ordering
/// should sort the result themselves.
#[derive(Clone)]
struct EvalValueOrd(EvalValue);

impl PartialEq for EvalValueOrd {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl Eq for EvalValueOrd {}

impl PartialOrd for EvalValueOrd {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for EvalValueOrd {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering;

        /// Variant discriminant for cross-variant comparisons.
        /// Order is arbitrary but stable.
        fn discriminant(v: &EvalValue) -> u8 {
            match v {
                EvalValue::Decimal(_) => 0,
                EvalValue::Subject(_) => 1,
                EvalValue::Bool(_) => 2,
                EvalValue::Collection(_) => 3,
                EvalValue::Date(_) => 4,
            }
        }

        match (&self.0, &other.0) {
            (EvalValue::Decimal(a), EvalValue::Decimal(b)) => a.cmp(b),
            (EvalValue::Subject(a), EvalValue::Subject(b)) => a.cmp(b),
            (EvalValue::Bool(a), EvalValue::Bool(b)) => a.cmp(b),
            (EvalValue::Date(a), EvalValue::Date(b)) => a.cmp(b),
            (EvalValue::Collection(a), EvalValue::Collection(b)) => {
                for (l, r) in a.iter().zip(b.iter()) {
                    let ord = EvalValueOrd(l.clone()).cmp(&EvalValueOrd(r.clone()));
                    if ord != Ordering::Equal {
                        return ord;
                    }
                }
                a.len().cmp(&b.len())
            }
            (l, r) => discriminant(l).cmp(&discriminant(r)),
        }
    }
}

/// `find_matches` is the predicate-evaluation primitive. It returns the
/// set of binding extensions under which the expression holds. An empty
/// vector means the expression fails; a non-empty vector means it succeeds
/// (potentially with extended bindings).
fn find_matches(
    e: &Expr,
    state: &State,
    base: &Bindings,
    actor: Option<&EvalValue>,
) -> Result<Vec<Bindings>, EvalError> {
    match e {
        Expr::Claim { predicate, args } => find_claim_matches(predicate, args, state, base, actor),
        Expr::And(exprs) => find_conjunction(exprs, state, base, actor),
        Expr::Not(inner) => {
            let m = find_matches(inner, state, base, actor)?;
            Ok(if m.is_empty() {
                vec![base.clone()]
            } else {
                vec![]
            })
        }
        Expr::Implies { left, right } => {
            let lm = find_matches(left, state, base, actor)?;
            for m in lm {
                if find_matches(right, state, &m, actor)?.is_empty() {
                    return Ok(vec![]);
                }
            }
            Ok(vec![base.clone()])
        }
        Expr::Exists { binding: _, body } => {
            let m = find_matches(body, state, base, actor)?;
            Ok(if m.is_empty() {
                vec![]
            } else {
                vec![base.clone()]
            })
        }
        Expr::Forall {
            binding: _,
            source,
            body,
        } => {
            let sm = find_matches(source, state, base, actor)?;
            for m in sm {
                if find_matches(body, state, &m, actor)?.is_empty() {
                    return Ok(vec![]);
                }
            }
            Ok(vec![base.clone()])
        }
        Expr::Eq(lhs, rhs) => {
            let l = eval_value(lhs, state, base, actor)?;
            let r = eval_value(rhs, state, base, actor)?;
            Ok(if l == r { vec![base.clone()] } else { vec![] })
        }
        Expr::Le(lhs, rhs) => {
            let l = eval_value(lhs, state, base, actor)?;
            let r = eval_value(rhs, state, base, actor)?;
            match (l, r) {
                (EvalValue::Decimal(a), EvalValue::Decimal(b)) => {
                    Ok(if a <= b { vec![base.clone()] } else { vec![] })
                }
                _ => Err(EvalError::TypeMismatch(
                    "Le expects decimal operands".into(),
                )),
            }
        }
        Expr::DateLe(lhs, rhs) => {
            let l = eval_value(lhs, state, base, actor)?;
            let r = eval_value(rhs, state, base, actor)?;
            match (l, r) {
                (EvalValue::Date(a), EvalValue::Date(b)) => {
                    Ok(if a <= b { vec![base.clone()] } else { vec![] })
                }
                _ => Err(EvalError::TypeMismatch(
                    "DateLe expects civil-date operands".into(),
                )),
            }
        }
        Expr::Neq(t1, t2) => {
            let l = resolve_term(t1, base, actor)?;
            let r = resolve_term(t2, base, actor)?;
            Ok(if l != r { vec![base.clone()] } else { vec![] })
        }
        Expr::In(elem, coll) => find_in_matches(elem, coll, base, actor),
        Expr::Term(_)
        | Expr::Sub(_, _)
        | Expr::Add(_, _)
        | Expr::Sum { .. }
        | Expr::ValueOf { .. } => Err(EvalError::NotPredicate),
    }
}

/// Parse a `Value::Date(String)` literal into a `jiff::civil::Date`.
/// Centralised so the IR-level literal and the runtime value cannot drift
/// in how they interpret `YYYY-MM-DD`. Used by `resolve_term`, by
/// `unify_args` for `Value::Date` literals in claim patterns, and by
/// `find_claim_matches` when narrowing a predicate bucket by a ground
/// date argument.
fn parse_date_literal(s: &str) -> Result<Date, EvalError> {
    s.parse::<Date>()
        .map_err(|e| EvalError::TypeMismatch(format!("invalid civil date `{s}`: {e}")))
}

fn find_claim_matches(
    predicate: &str,
    args: &[Term],
    state: &State,
    base: &Bindings,
    actor: Option<&EvalValue>,
) -> Result<Vec<Bindings>, EvalError> {
    let mut out = vec![];

    // Pre-pass: any occurrence of `Term::Actor` requires an actor in
    // scope. Without this, a selective ground arg appearing *earlier*
    // in the args could short-circuit to `Ok(empty)` (missing bucket)
    // before the loop ever reaches `Term::Actor`. That would leak the
    // doctrine - an invariant referencing `Term::Actor` could silently
    // produce no matches instead of erroring. Make the requirement
    // position-independent.
    if actor.is_none() && args.iter().any(|t| matches!(t, Term::Actor)) {
        return Err(EvalError::UnboundActor);
    }

    // First pass: identify every argument position that is *ground* in
    // the current binding context (Term::Literal in the IR, or
    // Term::Var already bound in `base`). Pick the position whose
    // (predicate, position, value) bucket is smallest; that's the most
    // selective lookup.
    //
    // For a typical invariant body like `JournalLine(entry, _, d, _)`
    // evaluated inside a `forall entry: ...`, `entry` is bound to a
    // specific subject and position 0 has a bucket of exactly the few
    // lines for that entry. That changes the scan from "all
    // JournalLines" to "JournalLines for this entry" - the difference
    // between O(N) and O(lines_per_entry) per lookup, which is where
    // the quadratic in `balanced_posted_entry` lives.
    //
    // If a ground arg's bucket is missing entirely, no claim of this
    // predicate has that value at that position; the result set is
    // empty and we short-circuit.
    //
    // If no argument is ground, fall back to scanning the whole
    // predicate bucket via `state.claims_for(predicate)`.
    let mut best: Option<&[usize]> = None;
    for (pos, term) in args.iter().enumerate() {
        let ground = match term {
            Term::Wildcard => None,
            Term::Var(name) => base.get(name).cloned(),
            Term::Literal(Value::Subject(s)) => Some(EvalValue::Subject(s.clone())),
            Term::Literal(Value::Decimal(s)) => Decimal::from_str(s).ok().map(EvalValue::Decimal),
            Term::Literal(Value::Date(s)) => parse_date_literal(s).ok().map(EvalValue::Date),
            Term::Actor => match actor {
                Some(a) => Some(a.clone()),
                None => return Err(EvalError::UnboundActor),
            },
        };
        let Some(value) = ground else {
            continue;
        };
        match state.claim_indices_for_arg(predicate, pos, &value) {
            None => return Ok(out),
            Some(bucket) => match best {
                Some(prev) if prev.len() <= bucket.len() => {}
                _ => best = Some(bucket),
            },
        }
    }

    if let Some(bucket) = best {
        for &i in bucket {
            let claim = state.claim_at(i);
            if claim.args.len() != args.len() {
                continue;
            }
            if let Some(b) = unify_args(args, &claim.args, base, actor) {
                out.push(b);
            }
        }
    } else {
        for claim in state.claims_for(predicate) {
            if claim.args.len() != args.len() {
                continue;
            }
            if let Some(b) = unify_args(args, &claim.args, base, actor) {
                out.push(b);
            }
        }
    }
    Ok(out)
}

fn unify_args(
    patterns: &[Term],
    values: &[EvalValue],
    base: &Bindings,
    actor: Option<&EvalValue>,
) -> Option<Bindings> {
    let mut b = base.clone();
    for (p, v) in patterns.iter().zip(values.iter()) {
        match p {
            Term::Wildcard => {}
            Term::Var(name) => {
                if let Some(existing) = b.get(name) {
                    if existing != v {
                        return None;
                    }
                } else {
                    b.insert(name.clone(), v.clone());
                }
            }
            Term::Literal(Value::Decimal(s)) => {
                let parsed = Decimal::from_str(s).ok()?;
                match v {
                    EvalValue::Decimal(d) if *d == parsed => {}
                    _ => return None,
                }
            }
            Term::Literal(Value::Subject(s)) => match v {
                EvalValue::Subject(id) if id == s => {}
                _ => return None,
            },
            Term::Literal(Value::Date(s)) => {
                let parsed = parse_date_literal(s).ok()?;
                match v {
                    EvalValue::Date(d) if *d == parsed => {}
                    _ => return None,
                }
            }
            Term::Actor => match actor {
                Some(a) if a == v => {}
                _ => return None,
            },
        }
    }
    Some(b)
}

fn find_conjunction(
    exprs: &[Expr],
    state: &State,
    base: &Bindings,
    actor: Option<&EvalValue>,
) -> Result<Vec<Bindings>, EvalError> {
    let mut current = vec![base.clone()];
    for expr in exprs {
        let mut next = vec![];
        for b in &current {
            next.extend(find_matches(expr, state, b, actor)?);
        }
        if next.is_empty() {
            return Ok(vec![]);
        }
        current = next;
    }
    Ok(current)
}

fn find_in_matches(
    elem: &Term,
    coll: &Term,
    base: &Bindings,
    actor: Option<&EvalValue>,
) -> Result<Vec<Bindings>, EvalError> {
    let coll_val = resolve_term(coll, base, actor)?;
    let items = match coll_val {
        EvalValue::Collection(v) => v,
        _ => return Err(EvalError::TypeMismatch("In expects a collection".into())),
    };
    match elem {
        Term::Wildcard => Err(EvalError::TypeMismatch("wildcard not valid in In".into())),
        Term::Literal(_) | Term::Actor => {
            let e = resolve_term(elem, base, actor)?;
            Ok(if items.contains(&e) {
                vec![base.clone()]
            } else {
                vec![]
            })
        }
        Term::Var(name) => {
            if let Some(existing) = base.get(name) {
                Ok(if items.contains(existing) {
                    vec![base.clone()]
                } else {
                    vec![]
                })
            } else {
                Ok(items
                    .into_iter()
                    .map(|v| {
                        let mut b = base.clone();
                        b.insert(name.clone(), v);
                        b
                    })
                    .collect())
            }
        }
    }
}

fn eval_value(
    e: &Expr,
    state: &State,
    bindings: &Bindings,
    actor: Option<&EvalValue>,
) -> Result<EvalValue, EvalError> {
    match e {
        Expr::Term(t) => resolve_term(t, bindings, actor),
        Expr::Sub(lhs, rhs) => {
            let l = eval_value(lhs, state, bindings, actor)?;
            let r = eval_value(rhs, state, bindings, actor)?;
            match (l, r) {
                (EvalValue::Decimal(a), EvalValue::Decimal(b)) => Ok(EvalValue::Decimal(a - b)),
                _ => Err(EvalError::TypeMismatch(
                    "Sub expects decimal operands".into(),
                )),
            }
        }
        Expr::Add(lhs, rhs) => {
            let l = eval_value(lhs, state, bindings, actor)?;
            let r = eval_value(rhs, state, bindings, actor)?;
            match (l, r) {
                (EvalValue::Decimal(a), EvalValue::Decimal(b)) => Ok(EvalValue::Decimal(a + b)),
                _ => Err(EvalError::TypeMismatch(
                    "Add expects decimal operands".into(),
                )),
            }
        }
        Expr::Sum {
            value,
            binding: _,
            body,
        } => {
            let matches = find_matches(body, state, bindings, actor)?;
            let mut total = Decimal::ZERO;
            for m in matches {
                match resolve_term(value, &m, actor)? {
                    EvalValue::Decimal(d) => total += d,
                    _ => return Err(EvalError::TypeMismatch("Sum expects decimal".into())),
                }
            }
            Ok(EvalValue::Decimal(total))
        }
        Expr::ValueOf {
            predicate,
            args,
            default,
        } => {
            let matches = find_claim_matches(predicate, args, state, bindings, actor)?;
            match matches.len() {
                1 => {
                    let pos = args
                        .iter()
                        .position(|t| matches!(t, Term::Wildcard))
                        .ok_or_else(|| {
                            EvalError::TypeMismatch("ValueOf requires a wildcard arg".into())
                        })?;
                    let claim = state
                        .claims_for(predicate)
                        .find(|f| {
                            f.args.len() == args.len()
                                && unify_args(args, &f.args, bindings, actor).is_some()
                        })
                        .ok_or_else(|| EvalError::ValueOfZeroMatches(predicate.clone()))?;
                    Ok(claim.args[pos].clone())
                }
                0 => match default {
                    Some(d) => eval_value(d, state, bindings, actor),
                    None => Err(EvalError::ValueOfZeroMatches(predicate.clone())),
                },
                _ => Err(EvalError::ValueOfMultipleMatches(predicate.clone())),
            }
        }
        _ => Err(EvalError::NotValue),
    }
}

fn resolve_term(
    t: &Term,
    bindings: &Bindings,
    actor: Option<&EvalValue>,
) -> Result<EvalValue, EvalError> {
    match t {
        Term::Var(name) => bindings
            .get(name)
            .cloned()
            .ok_or_else(|| EvalError::UnboundVariable(name.clone())),
        Term::Wildcard => Err(EvalError::TypeMismatch(
            "wildcard cannot be resolved as a value".into(),
        )),
        Term::Literal(Value::Decimal(s)) => {
            let d = Decimal::from_str(s)
                .map_err(|_| EvalError::TypeMismatch(format!("invalid decimal: {s}")))?;
            Ok(EvalValue::Decimal(d))
        }
        Term::Literal(Value::Subject(s)) => Ok(EvalValue::Subject(s.clone())),
        Term::Literal(Value::Date(s)) => Ok(EvalValue::Date(parse_date_literal(s)?)),
        Term::Actor => actor.cloned().ok_or(EvalError::UnboundActor),
    }
}

// ===========================================================================
// Transformation execution (in-memory)
// ===========================================================================

/// A resolved intent: all args are values, ready to be enqueued in an outbox.
///
/// JSON encoding shape: `{ "name": "...", "args": [ ... ] }`.
///
/// Used as-is for elements of `audit.emitted_intents` (a JSONB array of these
/// objects).
///
/// For row writes to the `outbox` table, the PG adapter **splits** the intent
/// across two columns: `intent_type` (text, from `name`) and `arguments`
/// (JSONB array, from `args`). The `arguments` column has a CHECK constraint
/// that requires `jsonb_typeof(arguments) = 'array'`, so writing the full
/// object there would fail. The `intent_args_serialise_as_a_json_array`
/// test pins this contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentInstance {
    pub name: String,
    pub args: Vec<EvalValue>,
}

/// A proposed state transition under proposed context.
///
/// A `Transition` is the value evaluated, accepted-or-rejected, and
/// persisted to the audit log on acceptance. It bundles three things:
///
/// - `transformation_name`: which named transformation is being proposed.
///   Must match the `name` of the [`Transformation`] passed to [`propose`].
/// - `args`: the per-call arguments to that transformation, positional,
///   matching the transformation's declared `parameters`.
/// - `actor`: the [`EvalValue::Subject`] under whose authority the
///   transition is being proposed. Carried as transition context, not
///   as a transformation parameter, so domain payloads stay free of
///   plumbing concerns.
///
/// The actor is plumbed through `propose` and persisted with the audit
/// row from this PR forward; admission rules that consult the actor
/// (authority checks) arrive in a later PR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    pub transformation_name: String,
    pub args: Vec<EvalValue>,
    pub actor: EvalValue,
}

/// The result of proposing a transformation. Either the candidate state is
/// admissible (Accepted) or some predicate or invariant rejected it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Accepted {
        asserted_claims: Vec<ClaimInstance>,
        retracted_claims: Vec<ClaimInstance>,
        emitted_intents: Vec<IntentInstance>,
        candidate_state: State,
    },
    Rejected {
        reason: String,
    },
}

enum StmtOutcome {
    Continue,
    Rejected(String),
}

/// Propose a transformation against a pre-state. Stages asserts/retracts/
/// intents, builds the candidate state, evaluates every invariant against
/// that candidate state, and returns Accepted iff all invariants hold.
///
/// No PostgreSQL, no audit, no outbox - that's a later concern. This
/// proves the semantic loop: transformation proposes, invariants decide.
///
/// The proposal is given as a [`Transition`], which bundles the
/// transformation name (verified against `transformation.name`), the
/// arguments, and the actor under whose authority the transition is
/// being proposed. The actor is plumbed through from this PR; admission
/// rules that consult it arrive later.
pub fn propose(
    transformation: &Transformation,
    transition: &Transition,
    pre_state: &State,
    invariants: &[Invariant],
) -> Result<Outcome, EvalError> {
    if transformation.name != transition.transformation_name {
        return Err(EvalError::TypeMismatch(format!(
            "transition names transformation `{}` but Transformation passed is `{}`",
            transition.transformation_name, transformation.name,
        )));
    }
    if !matches!(transition.actor, EvalValue::Subject(_)) {
        return Err(EvalError::TypeMismatch(
            "transition actor must be a subject".to_string(),
        ));
    }
    if transition.args.len() != transformation.parameters.len() {
        return Err(EvalError::TypeMismatch(format!(
            "transformation `{}` expects {} args, got {}",
            transformation.name,
            transformation.parameters.len(),
            transition.args.len(),
        )));
    }

    let mut bindings = Bindings::new();
    for (name, val) in transformation
        .parameters
        .iter()
        .zip(transition.args.iter().cloned())
    {
        bindings.insert(name.clone(), val);
    }

    let mut asserted: Vec<ClaimInstance> = vec![];
    let mut retracted: Vec<ClaimInstance> = vec![];
    let mut emitted: Vec<IntentInstance> = vec![];

    let actor = Some(&transition.actor);
    for stmt in &transformation.body {
        match execute_stmt(
            stmt,
            pre_state,
            &mut bindings,
            actor,
            &mut asserted,
            &mut retracted,
            &mut emitted,
        )? {
            StmtOutcome::Continue => {}
            StmtOutcome::Rejected(reason) => return Ok(Outcome::Rejected { reason }),
        }
    }

    let candidate = build_candidate_state(pre_state, &asserted, &retracted);

    for inv in invariants {
        if !eval_invariant(inv, &candidate)? {
            return Ok(Outcome::Rejected {
                reason: format!("invariant `{}` violated", inv.name),
            });
        }
    }

    Ok(Outcome::Accepted {
        asserted_claims: asserted,
        retracted_claims: retracted,
        emitted_intents: emitted,
        candidate_state: candidate,
    })
}

#[allow(clippy::too_many_arguments)]
fn execute_stmt(
    stmt: &Stmt,
    pre_state: &State,
    bindings: &mut Bindings,
    actor: Option<&EvalValue>,
    asserted: &mut Vec<ClaimInstance>,
    retracted: &mut Vec<ClaimInstance>,
    emitted: &mut Vec<IntentInstance>,
) -> Result<StmtOutcome, EvalError> {
    match stmt {
        Stmt::Require(expr) => {
            let matches = find_matches(expr, pre_state, bindings, actor)?;
            if matches.is_empty() {
                Ok(StmtOutcome::Rejected(format!(
                    "require failed: {} did not hold over pre-state",
                    format::format_expr_inline(expr)
                )))
            } else {
                Ok(StmtOutcome::Continue)
            }
        }
        Stmt::BindOne(expr) => {
            // Single-path deterministic unique lookup. See `Stmt::BindOne`
            // rustdoc for the multi-outcome contract. Crucially, on a
            // unique match we *replace* the binding context with the
            // returned match rather than extending. `find_matches` already
            // evaluates `expr` against the incoming `bindings`, so the
            // single returned entry is the new authoritative context;
            // extending would be redundant and risks subtle drift when
            // an already-bound variable is also constrained by the
            // expression.
            let mut matches = find_matches(expr, pre_state, bindings, actor)?;
            match matches.len() {
                0 => Ok(StmtOutcome::Rejected(format!(
                    "bind_one failed: {} matched no candidates",
                    format::format_expr_inline(expr)
                ))),
                1 => {
                    *bindings = matches.swap_remove(0);
                    Ok(StmtOutcome::Continue)
                }
                n => Err(EvalError::TypeMismatch(format!(
                    "bind_one matched {n} candidates; expected exactly one: {}",
                    format::format_expr_inline(expr)
                ))),
            }
        }
        Stmt::Let { name, value } => {
            let v = eval_value(value, pre_state, bindings, actor)?;
            bindings.insert(name.clone(), v);
            Ok(StmtOutcome::Continue)
        }
        Stmt::LetNewSubject { name } => {
            let id = uuid::Uuid::now_v7().to_string();
            bindings.insert(name.clone(), EvalValue::Subject(id));
            Ok(StmtOutcome::Continue)
        }
        Stmt::Assert(claim) => {
            asserted.push(resolve_claim(claim, bindings, actor)?);
            Ok(StmtOutcome::Continue)
        }
        Stmt::Retract { predicate, args } => {
            for claim in pre_state.claims_for(predicate) {
                if claim.args.len() != args.len() {
                    continue;
                }
                if unify_args(args, &claim.args, bindings, actor).is_some() {
                    retracted.push(claim.clone());
                }
            }
            Ok(StmtOutcome::Continue)
        }
        Stmt::For {
            binding,
            collection,
            body,
        } => {
            let coll_val = eval_value(collection, pre_state, bindings, actor)?;
            let items = match coll_val {
                EvalValue::Collection(v) => v,
                _ => return Err(EvalError::TypeMismatch("For expects a collection".into())),
            };
            // Iteration scope. Variables bound inside the body must not
            // leak across iterations or escape the loop. With `Stmt::Let`
            // alone this was harmless (each Let overwrote the same key
            // every iteration), but `Stmt::BindOne` exposes the latent
            // footgun: a residual binding from iteration N constrains
            // the find_matches narrowing in iteration N+1 and turns a
            // valid lookup into a zero-match rejection. We snapshot the
            // surrounding bindings before the loop, reset to that
            // snapshot plus the iteration variable at the start of each
            // iteration, and restore the snapshot when the loop
            // completes. The body sees only `outer ++ {binding ->
            // item}`, never the residue of a prior iteration.
            //
            // `clone_from` is used on the per-iteration reset rather
            // than `*bindings = outer.clone()` so the existing
            // HashMap's allocation is reused across iterations. The
            // final restore moves `outer` because it goes out of
            // scope afterwards.
            let outer = bindings.clone();
            for item in items {
                bindings.clone_from(&outer);
                bindings.insert(binding.clone(), item);
                for inner in body {
                    match execute_stmt(
                        inner, pre_state, bindings, actor, asserted, retracted, emitted,
                    )? {
                        StmtOutcome::Continue => {}
                        StmtOutcome::Rejected(r) => return Ok(StmtOutcome::Rejected(r)),
                    }
                }
            }
            *bindings = outer;
            Ok(StmtOutcome::Continue)
        }
        Stmt::Emit(intent) => {
            emitted.push(resolve_intent(intent, bindings, actor)?);
            Ok(StmtOutcome::Continue)
        }
    }
}

fn resolve_claim(
    claim: &Claim,
    bindings: &Bindings,
    actor: Option<&EvalValue>,
) -> Result<ClaimInstance, EvalError> {
    let mut args = Vec::with_capacity(claim.args.len());
    for t in &claim.args {
        if matches!(t, Term::Wildcard) {
            return Err(EvalError::TypeMismatch(
                "wildcard not allowed in assert".into(),
            ));
        }
        args.push(resolve_term(t, bindings, actor)?);
    }
    Ok(ClaimInstance {
        predicate: claim.predicate.clone(),
        args,
    })
}

fn resolve_intent(
    intent: &Intent,
    bindings: &Bindings,
    actor: Option<&EvalValue>,
) -> Result<IntentInstance, EvalError> {
    let mut args = Vec::with_capacity(intent.args.len());
    for t in &intent.args {
        if matches!(t, Term::Wildcard) {
            return Err(EvalError::TypeMismatch(
                "wildcard not allowed in emit".into(),
            ));
        }
        args.push(resolve_term(t, bindings, actor)?);
    }
    Ok(IntentInstance {
        name: intent.name.clone(),
        args,
    })
}

fn build_candidate_state(
    pre: &State,
    asserted: &[ClaimInstance],
    retracted: &[ClaimInstance],
) -> State {
    let mut claims = pre.claims().to_vec();
    claims.retain(|f| !retracted.iter().any(|r| r == f));
    for a in asserted {
        if !claims.iter().any(|f| f == a) {
            claims.push(a.clone());
        }
    }
    State::from_claims(claims)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    //! Kernel-internal unit tests for IR literals.
    //!
    //! Tests that depend on private items (`unify_args`, `resolve_term`,
    //! `Bindings`) live here. Tests that exercise the public surface —
    //! example chains, codec round-trips, IR-shape assertions — live in
    //! the `tests/` directory as integration tests, one file per example
    //! plus `tests/codec.rs` and the shared `tests/common/mod.rs`.

    use super::*;

    #[test]
    fn decimal_literal_constructs() {
        let v = Value::Decimal("1250.75".to_string());
        assert_eq!(
            Term::Literal(v),
            Term::Literal(Value::Decimal("1250.75".to_string()))
        );
    }

    #[test]
    fn subject_literal_constructs_and_resolves() {
        let v = Value::Subject("bank_debt_service".to_string());
        assert_eq!(
            Term::Literal(v),
            Term::Literal(Value::Subject("bank_debt_service".to_string()))
        );
        let resolved = resolve_term(
            &Term::Literal(Value::Subject("bank_debt_service".to_string())),
            &Bindings::new(),
            None,
        )
        .unwrap();
        assert_eq!(
            resolved,
            EvalValue::Subject("bank_debt_service".to_string())
        );
    }

    #[test]
    fn subject_literal_unifies_with_matching_subject_arg() {
        let pattern = vec![Term::Literal(Value::Subject("p1".to_string()))];
        let value = vec![EvalValue::Subject("p1".to_string())];
        assert!(unify_args(&pattern, &value, &Bindings::new(), None).is_some());

        let mismatch = vec![EvalValue::Subject("p2".to_string())];
        assert!(unify_args(&pattern, &mismatch, &Bindings::new(), None).is_none());

        let wrong_kind = vec![EvalValue::Decimal(Decimal::new(1, 0))];
        assert!(unify_args(&pattern, &wrong_kind, &Bindings::new(), None).is_none());
    }

    /// Pins the contract of `State::claims_for`: it returns *only*
    /// claims whose predicate matches the requested name, it returns
    /// them with arg values intact, it returns an empty iterator for
    /// predicates that have no admitted claims, and it does not
    /// interfere with `State::claims` returning the construction-order
    /// list.
    #[test]
    fn claims_for_returns_only_matching_predicate() {
        let a1 = ClaimInstance {
            predicate: "A".to_string(),
            args: vec![EvalValue::Subject("a1".to_string())],
        };
        let b1 = ClaimInstance {
            predicate: "B".to_string(),
            args: vec![EvalValue::Decimal(Decimal::new(42, 0))],
        };
        let a2 = ClaimInstance {
            predicate: "A".to_string(),
            args: vec![EvalValue::Subject("a2".to_string())],
        };
        let state = State::from_claims(vec![a1.clone(), b1.clone(), a2.clone()]);

        let a_rows: Vec<&ClaimInstance> = state.claims_for("A").collect();
        assert_eq!(a_rows.len(), 2, "two A claims admitted");
        assert!(a_rows.iter().all(|c| c.predicate == "A"));
        assert!(a_rows.contains(&&a1));
        assert!(a_rows.contains(&&a2));

        let b_rows: Vec<&ClaimInstance> = state.claims_for("B").collect();
        assert_eq!(b_rows, vec![&b1], "single B claim, args intact");

        let absent: Vec<&ClaimInstance> = state.claims_for("Nope").collect();
        assert!(
            absent.is_empty(),
            "no claims for an unknown predicate, not an error"
        );

        assert_eq!(
            state.claims(),
            &[a1, b1, a2],
            "claims() preserves construction order across all predicates"
        );
    }

    /// Pins the contract of `State::claim_indices_for_arg`: it returns
    /// the indices of claims with the requested predicate where the
    /// argument at the requested position equals the requested value,
    /// `None` (not Some empty) when no such bucket exists, and does
    /// not match claims of a different predicate that happen to share
    /// a value at the same position. The lookup is what
    /// `find_claim_matches` uses to make ground-argument matching
    /// O(bucket size) instead of O(predicate size).
    #[test]
    fn claim_indices_for_arg_narrows_by_predicate_position_and_value() {
        let line_for_entry_a = ClaimInstance {
            predicate: "JournalLine".to_string(),
            args: vec![
                EvalValue::Subject("entry_a".to_string()),
                EvalValue::Subject("account_cash".to_string()),
            ],
        };
        let line_for_entry_b = ClaimInstance {
            predicate: "JournalLine".to_string(),
            args: vec![
                EvalValue::Subject("entry_b".to_string()),
                EvalValue::Subject("account_cash".to_string()),
            ],
        };
        // Same value at position 0 but different predicate; must not
        // pollute the JournalLine[0=entry_a] bucket.
        let je_for_entry_a = ClaimInstance {
            predicate: "JournalEntry".to_string(),
            args: vec![EvalValue::Subject("entry_a".to_string())],
        };
        let state = State::from_claims(vec![
            line_for_entry_a.clone(),
            line_for_entry_b.clone(),
            je_for_entry_a.clone(),
        ]);

        let entry_a = EvalValue::Subject("entry_a".to_string());
        let positions = state
            .claim_indices_for_arg("JournalLine", 0, &entry_a)
            .expect("entry_a appears at JournalLine[0]");
        let claims: Vec<&ClaimInstance> = positions.iter().map(|&i| state.claim_at(i)).collect();
        assert_eq!(
            claims,
            vec![&line_for_entry_a],
            "must return only the JournalLine claim, not JournalEntry"
        );

        let unknown = EvalValue::Subject("entry_z".to_string());
        assert!(
            state
                .claim_indices_for_arg("JournalLine", 0, &unknown)
                .is_none(),
            "absent value returns None, signalling empty intersection"
        );

        let cash = EvalValue::Subject("account_cash".to_string());
        let cash_positions = state
            .claim_indices_for_arg("JournalLine", 1, &cash)
            .expect("account_cash appears at JournalLine[1]");
        assert_eq!(
            cash_positions.len(),
            2,
            "both JournalLine claims share account_cash at position 1"
        );
    }

    /// Pins the contract of `predicates_referenced_by_expr` by
    /// building an `Expr` that touches every variant carrying at
    /// least one nested `Expr` or `Claim`-shaped node. Each `Claim`
    /// and `ValueOf` site uses a unique predicate name. The
    /// extracted set must contain every planted name.
    ///
    /// This is the runtime safety net for the analysis. The
    /// compile-time safety net is the exhaustive `match` in
    /// `predicates_referenced_by_expr` itself: if a new `Expr`
    /// variant is added without handling, the function will not
    /// compile.
    #[test]
    fn predicates_referenced_by_expr_covers_every_variant() {
        // Helper to build a Claim-shaped Expr with a given predicate.
        let claim = |p: &str| Expr::Claim {
            predicate: p.to_string(),
            args: vec![],
        };
        // Helper to build a ValueOf-shaped Expr with a given predicate
        // and optionally a default expression that may carry more
        // predicates.
        let value_of = |p: &str, default: Option<Expr>| Expr::ValueOf {
            predicate: p.to_string(),
            args: vec![Term::Wildcard],
            default: default.map(Box::new),
        };

        let expr = Expr::And(vec![
            // Implies wraps two sides; both should be visited.
            Expr::Implies {
                left: Box::new(claim("P_implies_left")),
                right: Box::new(claim("P_implies_right")),
            },
            // Exists has a body.
            Expr::Exists {
                binding: "x".to_string(),
                body: Box::new(claim("P_exists_body")),
            },
            // Not wraps one expression.
            Expr::Not(Box::new(claim("P_not_body"))),
            // Eq operates on two sub-expressions.
            Expr::Eq(Box::new(claim("P_eq_left")), Box::new(claim("P_eq_right"))),
            // Le operates on two sub-expressions.
            Expr::Le(Box::new(claim("P_le_left")), Box::new(claim("P_le_right"))),
            // DateLe operates on two sub-expressions.
            Expr::DateLe(
                Box::new(claim("P_datele_left")),
                Box::new(claim("P_datele_right")),
            ),
            // Sub operates on two sub-expressions.
            Expr::Sub(
                Box::new(claim("P_sub_left")),
                Box::new(claim("P_sub_right")),
            ),
            // Add operates on two sub-expressions.
            Expr::Add(
                Box::new(claim("P_add_left")),
                Box::new(claim("P_add_right")),
            ),
            // Sum wraps a body.
            Expr::Sum {
                value: Term::Var("v".to_string()),
                binding: "v".to_string(),
                body: Box::new(claim("P_sum_body")),
            },
            // Forall has both source and body.
            Expr::Forall {
                binding: "y".to_string(),
                source: Box::new(claim("P_forall_source")),
                body: Box::new(claim("P_forall_body")),
            },
            // ValueOf carries its own predicate AND a recursive
            // default expression with another predicate.
            value_of("P_valueof_self", Some(claim("P_valueof_default"))),
            // Variants that carry no predicate references: must
            // contribute nothing. If any of these incorrectly added
            // entries the test below would still pass, but the
            // exhaustive set comparison further down catches
            // unexpected predicates too.
            Expr::Neq(Term::Var("a".to_string()), Term::Var("b".to_string())),
            Expr::Term(Term::Var("z".to_string())),
            Expr::In(Term::Var("e".to_string()), Term::Var("coll".to_string())),
        ]);

        let mut got = BTreeSet::new();
        predicates_referenced_by_expr(&expr, &mut got);

        let expected: BTreeSet<String> = [
            "P_implies_left",
            "P_implies_right",
            "P_exists_body",
            "P_not_body",
            "P_eq_left",
            "P_eq_right",
            "P_le_left",
            "P_le_right",
            "P_datele_left",
            "P_datele_right",
            "P_sub_left",
            "P_sub_right",
            "P_add_left",
            "P_add_right",
            "P_sum_body",
            "P_forall_source",
            "P_forall_body",
            "P_valueof_self",
            "P_valueof_default",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        assert_eq!(
            got, expected,
            "every Expr variant that carries a predicate reference must contribute it"
        );
    }

    /// `Expr::Add` returns the decimal sum of its operands when both
    /// evaluate to decimals.
    #[test]
    fn add_sums_two_decimals() {
        let expr = Expr::Add(
            Box::new(Expr::Term(Term::Literal(Value::Decimal("10".to_string())))),
            Box::new(Expr::Term(Term::Literal(Value::Decimal(
                "32.5".to_string(),
            )))),
        );
        let v = eval_value(&expr, &State::from_claims(vec![]), &Bindings::new(), None).unwrap();
        assert_eq!(v, EvalValue::Decimal(Decimal::new(425, 1)));
    }

    /// Non-decimal operands surface as `TypeMismatch`. Same contract as
    /// `Sub`. Authority records and other claims that admit non-decimal
    /// values into an `Add` position must trip this rather than fall
    /// through silently.
    #[test]
    fn add_with_non_decimal_operand_is_type_mismatch() {
        let expr = Expr::Add(
            Box::new(Expr::Term(Term::Literal(Value::Decimal("10".to_string())))),
            Box::new(Expr::Term(Term::Literal(Value::Subject(
                "oops".to_string(),
            )))),
        );
        let err = eval_value(&expr, &State::from_claims(vec![]), &Bindings::new(), None)
            .expect_err("expected TypeMismatch");
        match err {
            EvalError::TypeMismatch(msg) => assert!(msg.contains("Add")),
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    fn date_lit(s: &str) -> Expr {
        Expr::Term(Term::Literal(Value::Date(s.to_string())))
    }

    /// `DateLe(a, b)` admits when `a < b`. The successful match
    /// returns the unchanged binding set, mirroring decimal `Le`.
    #[test]
    fn date_le_admits_before() {
        let expr = Expr::DateLe(
            Box::new(date_lit("2026-03-11")),
            Box::new(date_lit("2026-03-12")),
        );
        let matches =
            find_matches(&expr, &State::from_claims(vec![]), &Bindings::new(), None).unwrap();
        assert_eq!(matches.len(), 1, "earlier date must admit under DateLe");
    }

    /// Boundary case: equal dates admit. This pins the **inclusive**
    /// semantics of validity windows in v0 - `effective_to ==
    /// action_date` is admissible, not rejected. The clinical-trial
    /// enrolment example relies on this for "the protocol expires
    /// today" being a valid randomisation date.
    #[test]
    fn date_le_admits_equal() {
        let expr = Expr::DateLe(
            Box::new(date_lit("2026-03-12")),
            Box::new(date_lit("2026-03-12")),
        );
        let matches =
            find_matches(&expr, &State::from_claims(vec![]), &Bindings::new(), None).unwrap();
        assert_eq!(
            matches.len(),
            1,
            "equal dates must admit under DateLe (inclusive window semantics)"
        );
    }

    /// `DateLe(a, b)` with `a > b` returns no matches - the lawful
    /// rejection path, distinct from `TypeMismatch`.
    #[test]
    fn date_le_rejects_after() {
        let expr = Expr::DateLe(
            Box::new(date_lit("2026-03-13")),
            Box::new(date_lit("2026-03-12")),
        );
        let matches =
            find_matches(&expr, &State::from_claims(vec![]), &Bindings::new(), None).unwrap();
        assert!(matches.is_empty(), "later date must reject under DateLe");
    }

    /// Mixed operand kinds raise `TypeMismatch`, not silent rejection.
    /// The clinical-trial example must not be able to admit by mistake
    /// because someone passed a decimal where a date was expected.
    #[test]
    fn date_le_type_mismatch_decimal_vs_date() {
        let expr = Expr::DateLe(
            Box::new(Expr::Term(Term::Literal(Value::Decimal("1".to_string())))),
            Box::new(date_lit("2026-03-12")),
        );
        let err = find_matches(&expr, &State::from_claims(vec![]), &Bindings::new(), None)
            .expect_err("decimal lhs must be a TypeMismatch");
        match err {
            EvalError::TypeMismatch(msg) => assert!(msg.contains("DateLe")),
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    /// Symmetric to the above: a date on the left and a non-date on
    /// the right also raises `TypeMismatch`. Pins that the type guard
    /// covers both positions.
    #[test]
    fn date_le_type_mismatch_date_vs_subject() {
        let expr = Expr::DateLe(
            Box::new(date_lit("2026-03-12")),
            Box::new(Expr::Term(Term::Literal(Value::Subject(
                "oops".to_string(),
            )))),
        );
        let err = find_matches(&expr, &State::from_claims(vec![]), &Bindings::new(), None)
            .expect_err("subject rhs must be a TypeMismatch");
        match err {
            EvalError::TypeMismatch(msg) => assert!(msg.contains("DateLe")),
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    /// A malformed `Value::Date` source string surfaces as
    /// `TypeMismatch` at evaluation time, mirroring how an invalid
    /// `Value::Decimal` surfaces. There is no separate IR validation
    /// pass; parsing is the evaluator's concern.
    #[test]
    fn date_le_invalid_iso_string_is_type_mismatch() {
        let expr = Expr::DateLe(
            Box::new(date_lit("not-a-date")),
            Box::new(date_lit("2026-03-12")),
        );
        let err = find_matches(&expr, &State::from_claims(vec![]), &Bindings::new(), None)
            .expect_err("invalid ISO string must be a TypeMismatch");
        match err {
            EvalError::TypeMismatch(msg) => {
                assert!(msg.contains("invalid civil date"), "msg was: {msg}")
            }
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    /// A `Value::Date` literal in a `claim` argument matches a
    /// claim admitted with the same date in that position. Pins the
    /// unify-against-literal-date path, the parallel of the existing
    /// decimal/subject literal unification.
    #[test]
    fn date_literal_unifies_with_matching_date_arg() {
        let claim = ClaimInstance {
            predicate: "OnDate".to_string(),
            args: vec![EvalValue::Date(
                "2026-03-12".parse::<Date>().expect("hand-built ISO date"),
            )],
        };
        let state = State::from_claims(vec![claim]);
        let expr = Expr::Claim {
            predicate: "OnDate".to_string(),
            args: vec![Term::Literal(Value::Date("2026-03-12".to_string()))],
        };
        let matches = find_matches(&expr, &state, &Bindings::new(), None).unwrap();
        assert_eq!(matches.len(), 1, "literal date arg must unify");

        let other = Expr::Claim {
            predicate: "OnDate".to_string(),
            args: vec![Term::Literal(Value::Date("2026-03-13".to_string()))],
        };
        let none = find_matches(&other, &state, &Bindings::new(), None).unwrap();
        assert!(
            none.is_empty(),
            "literal date arg must not unify with a different date"
        );
    }

    /// The cumulative-cap shape: `Le(Add(running, proposed), cap)`.
    /// This is the load-bearing composition the insurance-claim-settlement
    /// example uses to gate authorisations under a policy aggregate
    /// limit. Pinning it here so the kernel composition cannot drift.
    #[test]
    fn add_nests_under_le_for_cumulative_cap() {
        let running = Expr::Term(Term::Literal(Value::Decimal("60".to_string())));
        let proposed = Expr::Term(Term::Literal(Value::Decimal("40".to_string())));
        let cap = Expr::Term(Term::Literal(Value::Decimal("100".to_string())));

        // 60 + 40 <= 100 admits (binding pass-through).
        let under_cap = Expr::Le(
            Box::new(Expr::Add(Box::new(running.clone()), Box::new(proposed))),
            Box::new(cap.clone()),
        );
        let matches = find_matches(
            &under_cap,
            &State::from_claims(vec![]),
            &Bindings::new(),
            None,
        )
        .unwrap();
        assert_eq!(matches.len(), 1, "60 + 40 <= 100 should admit");

        // 60 + 50 <= 100 fails (empty match set).
        let over_cap = Expr::Le(
            Box::new(Expr::Add(
                Box::new(running),
                Box::new(Expr::Term(Term::Literal(Value::Decimal("50".to_string())))),
            )),
            Box::new(cap),
        );
        let matches = find_matches(
            &over_cap,
            &State::from_claims(vec![]),
            &Bindings::new(),
            None,
        )
        .unwrap();
        assert!(matches.is_empty(), "60 + 50 <= 100 should reject");
    }

    // ============================================================
    // Stmt::BindOne
    //
    // The deterministic unique-lookup binding statement. The
    // doctrine these tests pin:
    //
    //   require  = gate; does not export bindings
    //   bind_one = unique lookup; exports bindings
    //   let      = compute a value expression
    //
    // BindOne sits between Require (no binding export) and Let (a
    // value-producing expression). The tests below cover every
    // load-bearing branch: zero matches reject lawfully, one match
    // extends the binding context, two-or-more matches surface a
    // kernel error, the binding flows into subsequent statements,
    // and the existing NotPredicate path catches value-only
    // expressions slid into a BindOne by mistake.
    // ============================================================

    /// Build a one-statement transformation body containing the given
    /// statement, parameterless. Used by BindOne tests to drive the
    /// full `propose` path so we exercise the statement contract
    /// against a real transformation, not just `find_matches`.
    fn single_stmt_transformation(name: &str, body: Vec<Stmt>) -> Transformation {
        Transformation {
            name: name.to_string(),
            parameters: vec![],
            body,
        }
    }

    fn run(t: &Transformation, state: &State) -> Result<Outcome, EvalError> {
        let transition = Transition {
            transformation_name: t.name.clone(),
            args: vec![],
            actor: EvalValue::Subject("test_actor".to_string()),
        };
        propose(t, &transition, state, &[])
    }

    /// `bind_one` with a uniquely matching claim binds the variable
    /// for use by subsequent statements. Pinned against `propose`,
    /// not `execute_stmt` directly, so the test exercises the same
    /// path a real transformation does.
    #[test]
    fn bind_one_with_unique_match_extends_bindings_for_subsequent_stmts() {
        use dsl::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "Policy".to_string(),
            args: vec![
                EvalValue::Subject("p1".to_string()),
                EvalValue::Decimal(Decimal::new(100, 0)),
            ],
        }]);
        let t = single_stmt_transformation(
            "extract_then_assert",
            vec![
                bind_one(claim("Policy", vec![var("policy_id"), var("limit")])),
                assert_("Echo", vec![var("policy_id"), var("limit")]),
            ],
        );
        let Outcome::Accepted {
            asserted_claims, ..
        } = run(&t, &state).unwrap()
        else {
            panic!("expected Accepted");
        };
        assert_eq!(asserted_claims.len(), 1);
        assert_eq!(asserted_claims[0].predicate, "Echo");
        assert_eq!(
            asserted_claims[0].args,
            vec![
                EvalValue::Subject("p1".to_string()),
                EvalValue::Decimal(Decimal::new(100, 0)),
            ],
            "bind_one must have bound policy_id and limit for the assert"
        );
    }

    /// `bind_one` against a state with no matching claim rejects
    /// lawfully. The rejection reason names the expression so
    /// debugging is possible from the reason alone.
    #[test]
    fn bind_one_with_zero_matches_rejects_with_named_predicate() {
        use dsl::*;
        let state = State::default();
        let t = single_stmt_transformation(
            "extract_missing",
            vec![bind_one(claim(
                "Policy",
                vec![var("policy_id"), var("limit")],
            ))],
        );
        let Outcome::Rejected { reason } = run(&t, &state).unwrap() else {
            panic!("expected Rejected");
        };
        assert!(
            reason.contains("bind_one failed"),
            "reason should start with bind_one failed: {reason}"
        );
        assert!(
            reason.contains("Policy(policy_id, limit)"),
            "reason should name the expression: {reason}"
        );
    }

    /// `bind_one` against a state with two matching claims surfaces
    /// a kernel error, not a lawful rejection. Two matches means
    /// the programme expected unique state but admitted ambiguous
    /// state - missing structural-uniqueness invariant or
    /// corruption.
    #[test]
    fn bind_one_with_multiple_matches_is_kernel_error() {
        use dsl::*;
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "Policy".to_string(),
                args: vec![
                    EvalValue::Subject("p1".to_string()),
                    EvalValue::Decimal(Decimal::new(100, 0)),
                ],
            },
            ClaimInstance {
                predicate: "Policy".to_string(),
                args: vec![
                    EvalValue::Subject("p2".to_string()),
                    EvalValue::Decimal(Decimal::new(200, 0)),
                ],
            },
        ]);
        let t = single_stmt_transformation(
            "ambiguous_lookup",
            vec![bind_one(claim(
                "Policy",
                vec![var("policy_id"), var("limit")],
            ))],
        );
        let err = run(&t, &state).expect_err("expected EvalError");
        match err {
            EvalError::TypeMismatch(msg) => {
                assert!(
                    msg.contains("bind_one matched 2 candidates"),
                    "error should report multiplicity: {msg}"
                );
            }
            other => panic!("expected TypeMismatch, got {other:?}"),
        }
    }

    /// A bind_one whose pattern uses an already-bound variable narrows
    /// the candidate set by that variable. With `policy_id` pre-bound
    /// (e.g. by an enclosing parameter or earlier bind_one), the
    /// pattern matches only the row carrying that policy_id.
    #[test]
    fn bind_one_with_pre_bound_var_constrains_match() {
        use dsl::*;
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "Policy".to_string(),
                args: vec![
                    EvalValue::Subject("p1".to_string()),
                    EvalValue::Decimal(Decimal::new(100, 0)),
                ],
            },
            ClaimInstance {
                predicate: "Policy".to_string(),
                args: vec![
                    EvalValue::Subject("p2".to_string()),
                    EvalValue::Decimal(Decimal::new(200, 0)),
                ],
            },
        ]);
        // Two bind_ones in sequence: the first binds policy_id from
        // a literal subject; the second uses that binding to narrow
        // the Policy pattern. Without the narrowing, the second
        // bind_one would see two Policy candidates and error.
        let t = Transformation {
            name: "narrow_by_var".to_string(),
            parameters: vec![],
            body: vec![
                let_(
                    "policy_id",
                    term(Term::Literal(Value::Subject("p2".to_string()))),
                ),
                bind_one(claim("Policy", vec![var("policy_id"), var("limit")])),
                assert_("Echo", vec![var("limit")]),
            ],
        };
        let Outcome::Accepted {
            asserted_claims, ..
        } = run(&t, &state).unwrap()
        else {
            panic!("expected Accepted");
        };
        assert_eq!(
            asserted_claims[0].args,
            vec![EvalValue::Decimal(Decimal::new(200, 0))],
            "bound policy_id should narrow to p2's limit, not p1's"
        );
    }

    /// `bind_one` composes inside `For` bodies. The settlement-
    /// netting migration relies on this - the per-line value lookup
    /// (`bind_one LineAmount(line, amt)`) lives inside a
    /// `for line in lines:` body. Also pins the For-scoping fix:
    /// iteration 2 of the loop must not see iteration 1's `amt`
    /// binding, or its bind_one would narrow to the wrong row.
    #[test]
    fn bind_one_inside_for_body_composes() {
        use dsl::*;
        let state = State::from_claims(vec![
            ClaimInstance {
                predicate: "LineAmount".to_string(),
                args: vec![
                    EvalValue::Subject("L1".to_string()),
                    EvalValue::Decimal(Decimal::new(60, 0)),
                ],
            },
            ClaimInstance {
                predicate: "LineAmount".to_string(),
                args: vec![
                    EvalValue::Subject("L2".to_string()),
                    EvalValue::Decimal(Decimal::new(40, 0)),
                ],
            },
        ]);
        let t = Transformation {
            name: "iterate_lines".to_string(),
            parameters: vec!["lines".to_string()],
            body: vec![for_(
                "line",
                term(var("lines")),
                vec![
                    bind_one(claim("LineAmount", vec![var("line"), var("amt")])),
                    assert_("Echo", vec![var("line"), var("amt")]),
                ],
            )],
        };
        let transition = Transition {
            transformation_name: t.name.clone(),
            args: vec![EvalValue::Collection(vec![
                EvalValue::Subject("L1".to_string()),
                EvalValue::Subject("L2".to_string()),
            ])],
            actor: EvalValue::Subject("test_actor".to_string()),
        };
        let Outcome::Accepted {
            asserted_claims, ..
        } = propose(&t, &transition, &state, &[]).unwrap()
        else {
            panic!("expected Accepted");
        };
        assert_eq!(asserted_claims.len(), 2);
        assert_eq!(
            asserted_claims[0].args[0],
            EvalValue::Subject("L1".to_string())
        );
        assert_eq!(
            asserted_claims[0].args[1],
            EvalValue::Decimal(Decimal::new(60, 0))
        );
        assert_eq!(
            asserted_claims[1].args[0],
            EvalValue::Subject("L2".to_string())
        );
        assert_eq!(
            asserted_claims[1].args[1],
            EvalValue::Decimal(Decimal::new(40, 0))
        );
    }

    /// `Term::Actor` is resolvable inside a `bind_one` expression,
    /// because `bind_one` runs inside a transformation body (which
    /// has a transition in scope). Pinned because the
    /// `DelegatedInvestigator` pattern in the clinical-trial
    /// example - and any future authority lookup migrated to
    /// `bind_one` - depends on this.
    #[test]
    fn bind_one_with_actor_in_pattern() {
        use dsl::*;
        let state = State::from_claims(vec![ClaimInstance {
            predicate: "Authority".to_string(),
            args: vec![
                EvalValue::Subject("dr_smith".to_string()),
                EvalValue::Decimal(Decimal::new(50_000, 0)),
            ],
        }]);
        let t = Transformation {
            name: "lookup_my_authority".to_string(),
            parameters: vec![],
            body: vec![
                bind_one(claim("Authority", vec![actor(), var("limit")])),
                assert_("Echo", vec![var("limit")]),
            ],
        };
        let transition = Transition {
            transformation_name: t.name.clone(),
            args: vec![],
            actor: EvalValue::Subject("dr_smith".to_string()),
        };
        let Outcome::Accepted {
            asserted_claims, ..
        } = propose(&t, &transition, &state, &[]).unwrap()
        else {
            panic!("expected Accepted");
        };
        assert_eq!(
            asserted_claims[0].args,
            vec![EvalValue::Decimal(Decimal::new(50_000, 0))]
        );
    }

    /// A value-producing expression (e.g. `Add`) inside `bind_one`
    /// surfaces as `EvalError::NotPredicate`, via the existing
    /// `find_matches` guardrail. Pinned because the public DSL
    /// permits the construction; the runtime is the right place
    /// to enforce the predicate-shaped contract.
    #[test]
    fn bind_one_rejects_value_expr_as_not_predicate() {
        use dsl::*;
        let state = State::default();
        let t = single_stmt_transformation(
            "misuse_value_expr",
            vec![bind_one(add(
                term(Term::Literal(Value::Decimal("1".to_string()))),
                term(Term::Literal(Value::Decimal("2".to_string()))),
            ))],
        );
        let err = run(&t, &state).expect_err("expected EvalError");
        assert!(
            matches!(err, EvalError::NotPredicate),
            "expected NotPredicate, got {err:?}"
        );
    }

    // ============================================================
    // Program::validate() - strict arity validation
    //
    // The tests below pin each ValidationError variant and the
    // happy path. The validator collects every error rather than
    // failing on the first; a programme migration that adds
    // predicate declarations should see the full work list at
    // once, not one item per re-run.
    // ============================================================

    /// Build a tiny one-claim programme with a `predicate` declaration
    /// that matches by default. Per-test mutations adjust the
    /// predicates list or the transformation body to exercise each
    /// validator branch.
    fn one_claim_program() -> Program {
        use dsl::*;
        Program {
            name: "tiny".to_string(),
            predicates: vec![predicate("Echo").subject("id").decimal("amount").build()],
            invariants: vec![],
            transformations: vec![Transformation {
                name: "echo".to_string(),
                parameters: params(&["id", "amount"]),
                body: vec![assert_("Echo", vec![var("id"), var("amount")])],
            }],
            derived_claims: vec![],
        }
    }

    #[test]
    fn validate_succeeds_when_every_predicate_use_matches_declared_arity() {
        let p = one_claim_program();
        assert_eq!(p.validate(), Ok(()));
    }

    #[test]
    fn validate_reports_undeclared_predicate_in_transformation_body() {
        use dsl::*;
        let mut p = one_claim_program();
        p.transformations[0]
            .body
            .push(assert_("MissingPredicate", vec![var("id")]));
        let errors = p.validate().expect_err("expected validation errors");
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ValidationError::UndeclaredPredicate { predicate, .. }
                    if predicate == "MissingPredicate"
            )),
            "expected UndeclaredPredicate(MissingPredicate); got: {errors:?}"
        );
    }

    #[test]
    fn validate_reports_arity_mismatch_in_transformation_body() {
        use dsl::*;
        let mut p = one_claim_program();
        // Echo is declared with arity 2; calling with 1 arg trips
        // ArityMismatch.
        p.transformations[0].body = vec![assert_("Echo", vec![var("id")])];
        let errors = p.validate().expect_err("expected validation errors");
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ValidationError::ArityMismatch {
                    predicate,
                    expected: 2,
                    actual: 1,
                    ..
                } if predicate == "Echo"
            )),
            "expected ArityMismatch(Echo, 2, 1); got: {errors:?}"
        );
    }

    #[test]
    fn validate_reports_arity_mismatch_in_invariant_body() {
        use dsl::*;
        let mut p = one_claim_program();
        p.invariants.push(Invariant {
            name: "bad_inv".to_string(),
            version: 1,
            // Echo has arity 2; invariant body uses arity 3.
            body: claim("Echo", vec![var("x"), var("y"), var("z")]),
        });
        let errors = p.validate().expect_err("expected validation errors");
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ValidationError::ArityMismatch {
                    predicate,
                    expected: 2,
                    actual: 3,
                    context: ValidationContext::Invariant { name },
                    ..
                } if predicate == "Echo" && name == "bad_inv"
            )),
            "expected ArityMismatch in invariant context; got: {errors:?}"
        );
    }

    #[test]
    fn validate_reports_duplicate_predicate_decl() {
        use dsl::*;
        let mut p = one_claim_program();
        p.predicates
            .push(predicate("Echo").subject("a").subject("b").build());
        let errors = p.validate().expect_err("expected validation errors");
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ValidationError::DuplicatePredicateDecl { predicate }
                    if predicate == "Echo"
            )),
            "expected DuplicatePredicateDecl(Echo); got: {errors:?}"
        );
    }

    #[test]
    fn validate_reports_undeclared_derived_predicate() {
        use dsl::*;
        let mut p = one_claim_program();
        p.derived_claims.push(DerivedClaim {
            predicate: "Computed".to_string(),
            keys: vec!["id".to_string()],
            values: vec![DerivedValue {
                name: "n".to_string(),
                expr: term(var("id")),
            }],
            domain: claim("Echo", vec![var("id"), wildcard()]),
        });
        let errors = p.validate().expect_err("expected validation errors");
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ValidationError::UndeclaredPredicate { predicate, .. }
                    if predicate == "Computed"
            )),
            "expected UndeclaredPredicate(Computed); got: {errors:?}"
        );
    }

    #[test]
    fn validate_reports_derived_claim_arity_mismatch_against_declared_predicate() {
        use dsl::*;
        let mut p = one_claim_program();
        // Declare Computed with arity 3 but build it with keys=1,
        // values=1 (total arity 2 - one short).
        p.predicates.push(
            predicate("Computed")
                .subject("id")
                .subject("category")
                .decimal("balance")
                .build(),
        );
        p.derived_claims.push(DerivedClaim {
            predicate: "Computed".to_string(),
            keys: vec!["id".to_string()],
            values: vec![DerivedValue {
                name: "balance".to_string(),
                expr: term(var("id")),
            }],
            domain: claim("Echo", vec![var("id"), wildcard()]),
        });
        let errors = p.validate().expect_err("expected validation errors");
        assert!(
            errors.iter().any(|e| matches!(
                e,
                ValidationError::ArityMismatch {
                    predicate,
                    expected: 3,
                    actual: 2,
                    context: ValidationContext::DerivedClaim { .. },
                    ..
                } if predicate == "Computed"
            )),
            "expected ArityMismatch on derived claim Computed; got: {errors:?}"
        );
    }

    /// The validator collects every error and returns the full list.
    /// A migration that adds declarations should see all undeclared
    /// predicates at once, not one per re-run.
    #[test]
    fn validate_returns_all_errors_not_just_the_first() {
        use dsl::*;
        let mut p = one_claim_program();
        p.transformations[0].body.push(assert_("MissingA", vec![]));
        p.transformations[0].body.push(assert_("MissingB", vec![]));
        let errors = p.validate().expect_err("expected validation errors");
        assert!(
            errors.len() >= 2,
            "expected at least 2 errors; got: {errors:?}"
        );
        let names: Vec<&str> = errors
            .iter()
            .filter_map(|e| match e {
                ValidationError::UndeclaredPredicate { predicate, .. } => Some(predicate.as_str()),
                _ => None,
            })
            .collect();
        assert!(names.contains(&"MissingA"));
        assert!(names.contains(&"MissingB"));
    }
}
