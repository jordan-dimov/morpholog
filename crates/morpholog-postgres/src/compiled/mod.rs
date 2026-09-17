//! Compile fragment invariants into SQL violation queries.
//!
//! Denial orientation: each invariant compiles to one query that returns a
//! witnessing row when the invariant FAILS over the claims table and no rows
//! when it holds. The kernel semantics being reproduced: an invariant holds
//! iff its body yields at least one binding witness; `Implies`/`Forall`/
//! `Exists`/`Not` export no bindings; `And` threads bindings left to right;
//! decimal comparison is scale-insensitive (`::numeric`, never JSON text).
//!
//! The compiler is also the fragment classifier: an invariant outside the
//! fragment is a [`CompileRefusal`], collected whole-run like
//! `sql_views::ViewRefusal`. Input is a [`ValidatedProgram`], so a refusal
//! means exactly "a valid Morpholog invariant the fragment cannot express" -
//! undeclared names and arity mismatches stay validation errors.
//!
//! Equality representations are exhaustive per declared kind, each with its
//! proof, never a convenience fallback:
//!
//! - `Decimal`: `(arguments -> N ->> 'value')::numeric`. The kernel compares
//!   decimals scale-insensitively while the stored string preserves scale
//!   (`1.0` vs `1.00`), so text or jsonb equality would be WRONG.
//! - `Subject`: `arguments -> N ->> 'value'` text. Subjects are opaque
//!   strings; kernel equality is string equality.
//! - `Bool`, `Date`, `Timestamp`, `Duration`: the whole tagged value,
//!   `arguments -> N`, compared as jsonb. Sound because these kinds
//!   deserialise into semantic types (`bool`, jiff's `Date`/`Timestamp`/
//!   `SignedDuration`) whose serde output is canonical - two kernel-equal
//!   values re-serialise to the same tagged JSON - and every stored claim
//!   passed through that serialisation.
//! - `Quantity` (decimal-string amount), `Collection` (may hold decimals),
//!   `Any` (may hold anything): no equality representation is proved, so a
//!   variable join or filter on such a position refuses by kind.
//!
//! The kernel stays the executable spec: the compiled path's correctness
//! claim is held by the same-candidate differential in
//! `compiled_differential`, which stages each probe's delta once and
//! requires the kernel and both compiled stages to agree over it.
//!
//! The adopted witness contract (spike verdict): rule name, version, and
//! the witness VARIABLE SET are strict across evaluators; witness values
//! are observational - a symmetric self-join lawfully names the violating
//! pair in a different order. (The spike had a second cause, bodies
//! minting `new Subject()` twice; the same-candidate differential stages
//! the body once, so that source of divergence no longer exists.)

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use rust_decimal::Decimal;

use morpholog_core::{
    ClaimInstance, EvalValue, Invariant, InvariantName, OrderedDomain, PredicateArgKind,
    PredicateDecl, PredicateName, Prop, SumSeed, Term, ValidatedProgram, Value, ValueExpr, Var,
};

use crate::sql_quote::{quote_ident, quote_literal};

/// Why one invariant is outside the compiled fragment. Typed so tests and
/// callers dispatch on the variant, never on message text.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompileReason {
    /// A construct the fragment has no rendering for (`or`, `pre`, a
    /// defined call, arithmetic, ...).
    Construct { construct: &'static str },
    /// An ordered comparison in a non-decimal domain.
    ComparisonDomain { domain: OrderedDomain },
    /// A variable join, filter, or extraction on a position whose declared
    /// kind has no proved equality representation.
    ArgumentKind { kind: PredicateArgKind },
    /// A literal kind the fragment cannot render as a SQL constant.
    Literal { kind: &'static str },
    /// A `sum` outside the compiled shape (target must be a bound decimal
    /// variable or a decimal literal, body must bind claims, seed decimal).
    SumShape { detail: &'static str },
    /// A shape a validated programme cannot exhibit (defensive: reachable
    /// only through IR that skipped `Program::validated`).
    UnvalidatedShape { detail: String },
}

impl std::fmt::Display for CompileReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompileReason::Construct { construct } => {
                write!(f, "`{construct}` is outside the compiled fragment")
            }
            CompileReason::ComparisonDomain { domain } => {
                write!(
                    f,
                    "comparison domain {domain:?} is outside the compiled fragment"
                )
            }
            CompileReason::ArgumentKind { kind } => write!(
                f,
                "no proved equality representation for kind {kind} in the compiled fragment"
            ),
            CompileReason::Literal { kind } => {
                write!(f, "{kind} literals are outside the compiled fragment")
            }
            CompileReason::SumShape { detail } => {
                write!(f, "sum outside the compiled fragment: {detail}")
            }
            CompileReason::UnvalidatedShape { detail } => {
                write!(f, "shape a validated programme cannot exhibit: {detail}")
            }
        }
    }
}

/// One invariant the compiler could not express in the fragment.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CompileRefusal {
    pub invariant: InvariantName,
    pub reason: CompileReason,
}

/// Every invariant of a programme, compiled. Programme order is preserved:
/// the runner evaluates in this order and refuses on the first violation,
/// matching the kernel's first-failure contract.
#[derive(Debug)]
pub(crate) struct CompiledInvariantSet {
    pub(crate) invariants: Vec<CompiledInvariant>,
}

impl CompiledInvariantSet {
    /// Every index the set's SQL can seek on, once each, in
    /// specification order.
    pub(crate) fn required_indexes(&self) -> Vec<IndexSpec> {
        let mut specs: Vec<IndexSpec> = self
            .invariants
            .iter()
            .flat_map(|inv| inv.required_indexes.iter().cloned())
            .collect();
        specs.sort();
        specs.dedup();
        specs
    }
}

/// One index the compiled SQL can seek on: a partial expression index
/// over one argument position of one predicate, in the representation
/// the SQL reads that position with. Emitted by the compiler beside the
/// query, from the same [`Representation`], so the index necessarily
/// matches the extractor. Provisioning reconciles these against the
/// database; correctness never depends on them.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct IndexSpec {
    pub predicate: PredicateName,
    pub position: usize,
    pub representation: Representation,
    /// The indexed expression, unqualified, as it appears inside the
    /// parentheses of `CREATE INDEX`.
    pub expression_sql: String,
    /// The partial-index predicate.
    pub partial_predicate_sql: String,
}

impl IndexSpec {
    fn new(predicate: PredicateName, position: usize, representation: Representation) -> Self {
        let expression_sql = representation.extractor("", position);
        let partial_predicate_sql =
            format!("predicate_name = {}", quote_literal(predicate.as_str()));
        Self {
            predicate,
            position,
            representation,
            expression_sql,
            partial_predicate_sql,
        }
    }

    /// The digest of the whole canonical specification: same digest,
    /// same physical requirement.
    pub fn digest(&self) -> String {
        use sha2::{Digest as _, Sha256};
        let canonical = format!(
            "morpholog.claims\nbtree\n{}\n{}\n{}\n{}\n{}\n",
            self.predicate,
            self.position,
            self.representation.as_str(),
            self.expression_sql,
            self.partial_predicate_sql
        );
        hex::encode(Sha256::digest(canonical.as_bytes()))
    }

    /// The deterministic name in Morpholog's reserved namespace: a
    /// readable prefix and the digest that makes it unique, well under
    /// the identifier limit.
    pub fn index_name(&self) -> String {
        let readable: String = self
            .predicate
            .as_str()
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    '_'
                }
            })
            .take(24)
            .collect();
        format!(
            "morpholog_ci_{readable}_{}_{}_{}",
            self.position,
            self.representation.as_str(),
            &self.digest()[..12]
        )
    }

    /// The build statement, concurrent so the table stays writable; it
    /// cannot run inside a transaction.
    pub fn create_sql(&self) -> String {
        format!(
            "CREATE INDEX CONCURRENTLY {} ON morpholog.claims USING btree (({})) WHERE {}",
            quote_ident(&self.index_name()),
            self.expression_sql,
            self.partial_predicate_sql
        )
    }
}

/// How much of a compiled invariant a transition's delta touches.
#[derive(Debug, Clone, PartialEq, Eq)]
// The checks themselves are dormant until the stage-1 integration
// reaches production; the differential exercises them under test.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) enum CaseFilter {
    /// Delta disjoint from the invariant's occurrences: skip it entirely.
    Untouched,
    /// The touched cases, as a SQL disjunction over the antecedent's
    /// columns - spliced into the stage-1 query.
    Bounded(String),
    /// Touched, but not boundable to antecedent columns: run full stage 1.
    Unbounded,
}

#[derive(Debug, Clone)]
struct ColRef {
    alias: String,
    predicate: PredicateName,
    position: usize,
    kind: PredicateArgKind,
}

/// A claim pattern occurring anywhere in the body: which delta claims can
/// affect this invariant, and how their constants bound the antecedent.
#[derive(Debug, Clone)]
#[cfg_attr(not(test), allow(dead_code))]
struct OccurrenceBinder {
    predicate: PredicateName,
    /// Literal guards: a delta claim mismatching one cannot affect this
    /// occurrence.
    guards: Vec<(usize, Value)>,
    /// Occurrence position -> antecedent variable (only vars the witness
    /// columns carry; others merely widen the case).
    var_map: Vec<(usize, Var)>,
}

#[derive(Debug)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct CompiledInvariant {
    pub(crate) name: InvariantName,
    pub(crate) version: u32,
    /// Witness variables, sorted by name. Each violation row carries the
    /// full tagged value as `w_<var>`, decoded through `EvalValue`'s own
    /// serde - the one wire contract, no second kind decoder.
    pub(crate) witness_vars: Vec<Var>,
    occurrences: Vec<OccurrenceBinder>,
    case_cols: BTreeMap<Var, ColRef>,
    sql_select_from_where: String,
    sql_order_limit: String,
    /// The indexes this invariant's SQL can seek on, in specification
    /// order.
    pub(crate) required_indexes: Vec<IndexSpec>,
}

impl CompiledInvariant {
    /// The violation query. `case_filter` is a stage-2 bound produced by
    /// [`Self::case_filter`]; `None` is the full stage-1 check.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn violation_sql(&self, case_filter: Option<&str>) -> String {
        let stage = if case_filter.is_some() { 2 } else { 1 };
        let mut sql = format!(
            "/* morpholog compiled invariant {} v{} stage{} */\n{}",
            comment_safe(self.name.as_str()),
            self.version,
            stage,
            self.sql_select_from_where
        );
        if let Some(filter) = case_filter {
            let _ = write!(sql, "\n  AND ({filter})");
        }
        sql.push_str(&self.sql_order_limit);
        sql
    }

    /// Bound the check to the cases a delta could have changed. Sound by
    /// widening: a binder that cannot constrain a variable widens toward
    /// full stage 1, never narrows past a touched case.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn case_filter(
        &self,
        asserted: &[ClaimInstance],
        retracted: &[ClaimInstance],
    ) -> CaseFilter {
        let mut disjuncts: BTreeSet<String> = BTreeSet::new();
        let mut touched = false;
        for claim in asserted.iter().chain(retracted) {
            for occ in &self.occurrences {
                if occ.predicate != claim.predicate {
                    continue;
                }
                if !occ.guards.iter().all(|(pos, lit)| {
                    claim
                        .args
                        .get(*pos)
                        .is_some_and(|ev| literal_matches(lit, ev))
                }) {
                    continue;
                }
                touched = true;
                if occ.var_map.is_empty() {
                    return CaseFilter::Unbounded;
                }
                let mut parts = Vec::new();
                for (pos, var) in &occ.var_map {
                    let col = &self.case_cols[var];
                    let Some(ev) = claim.args.get(*pos) else {
                        return CaseFilter::Unbounded;
                    };
                    match const_eq(col, ev) {
                        Some(sql) => parts.push(sql),
                        None => return CaseFilter::Unbounded,
                    }
                }
                disjuncts.insert(parts.join(" AND "));
            }
        }
        if !touched {
            return CaseFilter::Untouched;
        }
        let filter = disjuncts.into_iter().collect::<Vec<_>>().join(") OR (");
        CaseFilter::Bounded(format!("({filter})"))
    }
}

/// An invariant name is an opaque string in hand-built IR; neutralise
/// every sequence that could break the block comment it travels in.
/// PostgreSQL block comments NEST, so an embedded `/*` is as hostile
/// as `*/`: it opens a level our single closer would then close,
/// leaving the real comment open over the rest of the statement.
#[cfg_attr(not(test), allow(dead_code))]
fn comment_safe(name: &str) -> String {
    name.replace(['\r', '\n'], " ")
        .replace("*/", "* /")
        .replace("/*", "/ *")
}

/// Compile every invariant of a validated programme, or report every
/// refusal (whole-run: nothing compiles unless everything does).
pub(crate) fn compile_invariants(
    program: ValidatedProgram<'_>,
) -> Result<CompiledInvariantSet, Vec<CompileRefusal>> {
    let program = program.as_program();
    let decls: BTreeMap<&str, &PredicateDecl> = program
        .predicates
        .iter()
        .map(|p| (p.name.as_str(), p))
        .collect();
    let mut compiled = Vec::new();
    let mut refusals = Vec::new();
    for inv in &program.invariants {
        match compile_invariant(inv, &decls) {
            Ok(c) => compiled.push(c),
            Err(reason) => refusals.push(CompileRefusal {
                invariant: inv.name.clone(),
                reason,
            }),
        }
    }
    if refusals.is_empty() {
        Ok(CompiledInvariantSet {
            invariants: compiled,
        })
    } else {
        Err(refusals)
    }
}

type Env = BTreeMap<Var, ColRef>;

/// (predicate, literal guards, var positions) as collected during the
/// walk, before restriction to the antecedent's columns.
type RawOccurrence = (PredicateName, Vec<(usize, Value)>, Vec<(usize, Var)>);

struct Ctx<'a> {
    decls: &'a BTreeMap<&'a str, &'a PredicateDecl>,
    counter: usize,
    occurrences: Vec<RawOccurrence>,
    /// Every (predicate, position, representation) the rendered SQL
    /// filters or joins on - the index specification, collected where
    /// the extractor is emitted so both come from the same object.
    required: BTreeSet<(PredicateName, usize, Representation)>,
}

struct Rendered {
    from: Vec<(String, String)>, // (alias, from item)
    where_: Vec<String>,
    env: Env,
}

impl Rendered {
    fn conjunction(&self) -> String {
        self.where_.join(" AND ")
    }

    /// `EXISTS`-shaped rendering of this match, usable inside a WHERE.
    fn exists_sql(&self) -> String {
        if self.from.is_empty() {
            format!("({})", self.conjunction())
        } else {
            format!(
                "EXISTS (SELECT 1 FROM {} WHERE {})",
                self.from
                    .iter()
                    .map(|(_, f)| f.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                self.conjunction()
            )
        }
    }
}

fn compile_invariant(
    inv: &Invariant,
    decls: &BTreeMap<&str, &PredicateDecl>,
) -> Result<CompiledInvariant, CompileReason> {
    let mut ctx = Ctx {
        decls,
        counter: 0,
        occurrences: Vec::new(),
        required: BTreeSet::new(),
    };

    let (select_from_where, order_limit, case_cols) = match &inv.body {
        Prop::Implies { left, right } => compile_denial(left, right, &mut ctx)?,
        Prop::Forall {
            binding: _,
            source,
            body,
        } => compile_denial(source, body, &mut ctx)?,
        // Top-level Not: violated iff the inner matches; its bindings are
        // the natural witness.
        Prop::Not(inner) => {
            let r = render_prop(inner, Env::new(), &mut ctx)?;
            if r.from.is_empty() {
                generic_denial(&inv.body, &mut ctx)?
            } else {
                let (select, order) = witness_select_order(&r);
                (
                    format!(
                        "SELECT {select}\nFROM {}\nWHERE {}",
                        from_list(&r),
                        r.where_.join("\n  AND ")
                    ),
                    format!("\nORDER BY {order}\nLIMIT 1"),
                    r.env,
                )
            }
        }
        other => generic_denial(other, &mut ctx)?,
    };

    let witness_vars = case_cols.keys().cloned().collect();
    let occurrences = ctx
        .occurrences
        .into_iter()
        .map(|(predicate, guards, var_map)| OccurrenceBinder {
            predicate,
            guards,
            var_map: var_map
                .into_iter()
                .filter(|(_, v)| case_cols.contains_key(v))
                .collect(),
        })
        .collect();

    Ok(CompiledInvariant {
        name: inv.name.clone(),
        version: inv.version,
        witness_vars,
        occurrences,
        case_cols,
        sql_select_from_where: select_from_where,
        sql_order_limit: order_limit,
        required_indexes: ctx
            .required
            .iter()
            .map(|(predicate, position, repr)| IndexSpec::new(predicate.clone(), *position, *repr))
            .collect(),
    })
}

/// The dominant shape: `antecedent implies consequent`. Violation = an
/// antecedent match with no consequent match.
fn compile_denial(
    left: &Prop,
    right: &Prop,
    ctx: &mut Ctx<'_>,
) -> Result<(String, String, Env), CompileReason> {
    let ant = render_prop(left, Env::new(), ctx)?;
    if ant.from.is_empty() {
        // Filter-only antecedent: no generators to witness; use the
        // generic whole-body denial.
        return generic_denial_implies(left, right, ctx);
    }
    let cons = render_prop(right, ant.env.clone(), ctx)?;
    let not_cons = if cons.from.is_empty() {
        format!("NOT ({})", cons.conjunction())
    } else {
        format!("NOT {}", cons.exists_sql())
    };
    let (select, order) = witness_select_order(&ant);
    let mut where_ = ant.where_.clone();
    where_.push(not_cons);
    Ok((
        format!(
            "SELECT {select}\nFROM {}\nWHERE {}",
            from_list(&ant),
            where_.join("\n  AND ")
        ),
        format!("\nORDER BY {order}\nLIMIT 1"),
        ant.env,
    ))
}

/// Any other top-level shape: the invariant holds iff the body matches at
/// all, so violation is bare non-existence, with an empty witness.
fn generic_denial(body: &Prop, ctx: &mut Ctx<'_>) -> Result<(String, String, Env), CompileReason> {
    let r = render_prop(body, Env::new(), ctx)?;
    Ok((
        format!("SELECT 1 AS \"w\"\nWHERE NOT {}", r.exists_sql()),
        String::new(),
        Env::new(),
    ))
}

fn generic_denial_implies(
    left: &Prop,
    right: &Prop,
    ctx: &mut Ctx<'_>,
) -> Result<(String, String, Env), CompileReason> {
    let l = render_prop(left, Env::new(), ctx)?;
    let r = render_prop(right, l.env.clone(), ctx)?;
    let not_r = if r.from.is_empty() {
        format!("NOT ({})", r.conjunction())
    } else {
        format!("NOT {}", r.exists_sql())
    };
    let mut where_ = l.where_.clone();
    where_.push(not_r);
    let violated = Rendered {
        from: l.from,
        where_,
        env: Env::new(),
    };
    Ok((
        format!("SELECT 1 AS \"w\"\nWHERE {}", violated.exists_sql()),
        String::new(),
        Env::new(),
    ))
}

fn from_list(r: &Rendered) -> String {
    r.from
        .iter()
        .map(|(_, f)| f.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn witness_select_order(r: &Rendered) -> (String, String) {
    let select = if r.env.is_empty() {
        "1 AS \"w\"".to_string()
    } else {
        // The full tagged value, so decoding goes through EvalValue's
        // serde and needs no per-kind column logic.
        r.env
            .iter()
            .map(|(v, col)| {
                format!(
                    "({}.arguments -> {})::text AS {}",
                    col.alias,
                    col.position,
                    quote_ident(&format!("w_{v}"))
                )
            })
            .collect::<Vec<_>>()
            .join(",\n       ")
    };
    // Order by the extractor expressions, not the generators' raw
    // `arguments`: raw-argument order matches the PK, which baits the
    // planner into an early-stop scan of the whole predicate (measured
    // plan flip at N=100k in the spike); the extractor expressions match
    // the partial expression indexes rung 2 derives. Deterministic in
    // everything the row reports.
    let order = if r.env.is_empty() {
        r.from
            .iter()
            .map(|(alias, _)| format!("{alias}.arguments"))
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        r.env
            .values()
            .map(|col| format!("({})::text", col_sql(col, Representation::Text)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    (select, order)
}

/// How a claim-argument position is read in SQL so that SQL equality is
/// kernel equality - and, from the same object, how an index over that
/// position is expressed, so the index necessarily matches the
/// extractor. See the module doc for the per-kind proofs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Representation {
    Text,
    Numeric,
    /// The whole tagged value; sound only for kinds whose canonical
    /// serialisation makes structural equality semantic equality.
    Jsonb,
}

impl Representation {
    /// The extractor over `arguments` at `position`. `qualifier` is the
    /// table alias followed by a dot in a query, empty in an index
    /// expression.
    pub(crate) fn extractor(self, qualifier: &str, position: usize) -> String {
        match self {
            Representation::Text => format!("{qualifier}arguments -> {position} ->> 'value'"),
            Representation::Numeric => {
                format!("({qualifier}arguments -> {position} ->> 'value')::numeric")
            }
            Representation::Jsonb => format!("{qualifier}arguments -> {position}"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Representation::Text => "text",
            Representation::Numeric => "numeric",
            Representation::Jsonb => "jsonb",
        }
    }
}

fn repr_for(kind: &PredicateArgKind) -> Result<Representation, CompileReason> {
    match kind {
        PredicateArgKind::Decimal => Ok(Representation::Numeric),
        PredicateArgKind::Subject => Ok(Representation::Text),
        PredicateArgKind::Bool
        | PredicateArgKind::Date
        | PredicateArgKind::Timestamp
        | PredicateArgKind::Duration => Ok(Representation::Jsonb),
        PredicateArgKind::Quantity(_)
        | PredicateArgKind::Collection
        | PredicateArgKind::Any
        | PredicateArgKind::CalendarSpan => Err(CompileReason::ArgumentKind { kind: kind.clone() }),
    }
}

fn col_sql(col: &ColRef, repr: Representation) -> String {
    repr.extractor(&format!("{}.", col.alias), col.position)
}

fn col_eq(a: &ColRef, b: &ColRef) -> Result<String, CompileReason> {
    let repr = repr_for(&a.kind)?;
    Ok(format!("({}) = ({})", col_sql(a, repr), col_sql(b, repr)))
}

/// A literal filter on a claim position, or a refusal when the literal
/// kind is outside the fragment.
fn literal_sql(value: &Value) -> Result<(String, Representation), CompileReason> {
    match value {
        Value::Subject(s) => Ok((quote_literal(s.as_str()), Representation::Text)),
        Value::Decimal(d) => Ok((
            format!("{}::numeric", quote_literal(d)),
            Representation::Numeric,
        )),
        Value::Date(_) => Err(CompileReason::Literal { kind: "date" }),
        Value::Timestamp(_) => Err(CompileReason::Literal { kind: "timestamp" }),
        Value::Duration(_) => Err(CompileReason::Literal { kind: "duration" }),
        Value::CalendarSpan(_) => Err(CompileReason::Literal {
            kind: "calendar span",
        }),
        Value::Quantity { .. } => Err(CompileReason::Literal { kind: "quantity" }),
    }
}

/// Stage-2 constant equality on an antecedent column, or None when the
/// value kind cannot be rendered (widens to Unbounded). The tagged
/// tier compares the whole value as jsonb against the delta value's
/// own serialisation - the same serde every stored claim passed
/// through, so the constant and the column speak one canonical form.
#[cfg_attr(not(test), allow(dead_code))]
fn const_eq(col: &ColRef, ev: &EvalValue) -> Option<String> {
    match ev {
        EvalValue::Subject(s) => Some(format!(
            "({}) = {}",
            col_sql(col, Representation::Text),
            quote_literal(s.as_str())
        )),
        EvalValue::Decimal(d) => Some(format!(
            "({}) = {}::numeric",
            col_sql(col, Representation::Numeric),
            quote_literal(&d.to_string())
        )),
        EvalValue::Bool(_)
        | EvalValue::Date(_)
        | EvalValue::Timestamp(_)
        | EvalValue::Duration(_) => {
            let json = serde_json::to_string(ev).ok()?;
            Some(format!(
                "({}) = {}::jsonb",
                col_sql(col, Representation::Jsonb),
                quote_literal(&json)
            ))
        }
        _ => None,
    }
}

#[cfg_attr(not(test), allow(dead_code))]
fn literal_matches(lit: &Value, ev: &EvalValue) -> bool {
    match (lit, ev) {
        (Value::Subject(a), EvalValue::Subject(b)) => a == b,
        (Value::Decimal(a), EvalValue::Decimal(b)) => a.parse::<Decimal>().is_ok_and(|a| a == *b),
        // Guard kinds the fragment cannot compare: treat as matching, which
        // only widens the touched-case set.
        _ => true,
    }
}

fn render_prop(prop: &Prop, env: Env, ctx: &mut Ctx<'_>) -> Result<Rendered, CompileReason> {
    match prop {
        Prop::Claim { predicate, args } => render_claim(predicate, args, env, ctx),
        Prop::And(ps) => {
            let mut acc = Rendered {
                from: Vec::new(),
                where_: Vec::new(),
                env,
            };
            for p in ps {
                let r = render_prop(p, acc.env.clone(), ctx)?;
                acc.from.extend(r.from);
                acc.where_.extend(r.where_);
                acc.env = r.env;
            }
            Ok(acc)
        }
        Prop::Not(inner) => {
            let r = render_prop(inner, env.clone(), ctx)?;
            let clause = if r.from.is_empty() {
                format!("NOT ({})", r.conjunction())
            } else {
                format!("NOT {}", r.exists_sql())
            };
            Ok(Rendered {
                from: Vec::new(),
                where_: vec![clause],
                env,
            })
        }
        Prop::Exists { binding: _, body } => {
            let r = render_prop(body, env.clone(), ctx)?;
            Ok(Rendered {
                from: Vec::new(),
                where_: vec![r.exists_sql()],
                env,
            })
        }
        Prop::Implies { left, right } => {
            let l = render_prop(left, env.clone(), ctx)?;
            let r = render_prop(right, l.env.clone(), ctx)?;
            let not_r = if r.from.is_empty() {
                format!("NOT ({})", r.conjunction())
            } else {
                format!("NOT {}", r.exists_sql())
            };
            let mut where_ = l.where_;
            where_.push(not_r);
            let violated = Rendered {
                from: l.from,
                where_,
                env: Env::new(),
            };
            Ok(Rendered {
                from: Vec::new(),
                where_: vec![format!("NOT {}", violated.exists_sql())],
                env,
            })
        }
        Prop::Forall {
            binding: _,
            source,
            body,
        } => render_prop(
            &Prop::Implies {
                left: source.clone(),
                right: body.clone(),
            },
            env,
            ctx,
        ),
        Prop::Eq(a, b) => compare_sql(a, b, "=", &env, ctx),
        Prop::Neq(a, b) => compare_sql(a, b, "<>", &env, ctx),
        Prop::Compare {
            op,
            domain,
            left,
            right,
        } => {
            if *domain != OrderedDomain::Decimal {
                return Err(CompileReason::ComparisonDomain { domain: *domain });
            }
            let op = match op {
                morpholog_core::CompareOp::Le => "<=",
                morpholog_core::CompareOp::Lt => "<",
                morpholog_core::CompareOp::Ge => ">=",
                morpholog_core::CompareOp::Gt => ">",
            };
            compare_sql(left, right, op, &env, ctx)
        }
        Prop::Or(_) => Err(CompileReason::Construct { construct: "or" }),
        Prop::Xor(_, _) => Err(CompileReason::Construct { construct: "xor" }),
        Prop::Pre(_) => Err(CompileReason::Construct { construct: "pre" }),
        Prop::Defined { .. } => Err(CompileReason::Construct {
            construct: "defined call",
        }),
        Prop::In(_, _) => Err(CompileReason::Construct { construct: "in" }),
    }
}

fn render_claim(
    predicate: &PredicateName,
    args: &[Term],
    mut env: Env,
    ctx: &mut Ctx<'_>,
) -> Result<Rendered, CompileReason> {
    let decl =
        ctx.decls
            .get(predicate.as_str())
            .ok_or_else(|| CompileReason::UnvalidatedShape {
                detail: format!("undeclared predicate {predicate}"),
            })?;
    let alias = format!("t{}", ctx.counter);
    ctx.counter += 1;

    let mut where_ = vec![format!(
        "{alias}.predicate_name = {}",
        quote_literal(predicate.as_str())
    )];
    let mut guards = Vec::new();
    let mut var_map = Vec::new();
    for (i, term) in args.iter().enumerate() {
        let kind = decl.args.get(i).map(|a| a.kind.clone()).ok_or_else(|| {
            CompileReason::UnvalidatedShape {
                detail: format!("arity mismatch on {predicate}"),
            }
        })?;
        let col = ColRef {
            alias: alias.clone(),
            predicate: predicate.clone(),
            position: i,
            kind,
        };
        match term {
            Term::Wildcard => {}
            Term::Actor => {
                return Err(CompileReason::Construct { construct: "actor" });
            }
            Term::Literal(v) => {
                let (lit, repr) = literal_sql(v)?;
                where_.push(format!("({}) = {}", col_sql(&col, repr), lit));
                ctx.required.insert((predicate.clone(), i, repr));
                guards.push((i, v.clone()));
            }
            Term::Var(v) => {
                var_map.push((i, v.clone()));
                if let Some(bound) = env.get(v) {
                    where_.push(col_eq(bound, &col)?);
                    let repr = repr_for(&col.kind)?;
                    ctx.required
                        .insert((bound.predicate.clone(), bound.position, repr));
                    ctx.required.insert((predicate.clone(), i, repr));
                } else {
                    // Binding a variable requires the position to carry a
                    // proved equality representation NOW, not lazily: a
                    // later join or witness read must never fall back to
                    // an unsound comparison.
                    repr_for(&col.kind)?;
                    env.insert(v.clone(), col);
                }
            }
        }
    }
    ctx.occurrences.push((predicate.clone(), guards, var_map));
    Ok(Rendered {
        from: vec![(alias.clone(), format!("morpholog.claims {alias}"))],
        where_,
        env,
    })
}

fn compare_sql(
    a: &ValueExpr,
    b: &ValueExpr,
    op: &str,
    env: &Env,
    ctx: &mut Ctx<'_>,
) -> Result<Rendered, CompileReason> {
    let a_sql = value_sql(a, env, ctx)?;
    let b_sql = value_sql(b, env, ctx)?;
    Ok(Rendered {
        from: Vec::new(),
        where_: vec![format!("({a_sql}) {op} ({b_sql})")],
        env: env.clone(),
    })
}

fn value_sql(expr: &ValueExpr, env: &Env, ctx: &mut Ctx<'_>) -> Result<String, CompileReason> {
    match expr {
        ValueExpr::Term(Term::Var(v)) => {
            let col = env.get(v).ok_or_else(|| CompileReason::UnvalidatedShape {
                detail: format!("unbound variable in value position: {v}"),
            })?;
            Ok(col_sql(col, repr_for(&col.kind)?))
        }
        ValueExpr::Term(Term::Literal(v)) => {
            let (lit, _) = literal_sql(v)?;
            Ok(lit)
        }
        ValueExpr::Term(Term::Wildcard) => Err(CompileReason::Construct {
            construct: "wildcard value",
        }),
        ValueExpr::Term(Term::Actor) => Err(CompileReason::Construct { construct: "actor" }),
        ValueExpr::Sum { value, body, seed } => {
            if *seed != SumSeed::Decimal {
                return Err(CompileReason::SumShape {
                    detail: "non-decimal seed",
                });
            }
            let r = render_prop(body, env.clone(), ctx)?;
            if r.from.is_empty() {
                return Err(CompileReason::SumShape {
                    detail: "body binds no claims",
                });
            }
            // The compiled sum target is a bound decimal variable or a
            // decimal literal - the pre-expression-target shape. A
            // computed target refuses.
            let val = match value.as_ref() {
                ValueExpr::Term(Term::Var(v)) => {
                    let col = r
                        .env
                        .get(v)
                        .ok_or_else(|| CompileReason::UnvalidatedShape {
                            detail: format!("sum target unbound: {v}"),
                        })?;
                    if col.kind != PredicateArgKind::Decimal {
                        return Err(CompileReason::SumShape {
                            detail: "target is not a decimal position",
                        });
                    }
                    col_sql(col, Representation::Numeric)
                }
                ValueExpr::Term(Term::Literal(Value::Decimal(d))) => {
                    format!("{}::numeric", quote_literal(d))
                }
                _ => {
                    return Err(CompileReason::SumShape {
                        detail: "computed target",
                    });
                }
            };
            Ok(format!(
                "COALESCE((SELECT sum({val}) FROM {} WHERE {}), 0::numeric)",
                r.from
                    .iter()
                    .map(|(_, f)| f.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                r.conjunction()
            ))
        }
        ValueExpr::Arith { .. } => Err(CompileReason::Construct {
            construct: "arithmetic",
        }),
        ValueExpr::Extremum { .. } => Err(CompileReason::Construct {
            construct: "extremum",
        }),
        ValueExpr::ValueOf { .. } => Err(CompileReason::Construct {
            construct: "value lookup",
        }),
        ValueExpr::Cond { .. } => Err(CompileReason::Construct {
            construct: "conditional value",
        }),
        ValueExpr::Call { .. } => Err(CompileReason::Construct {
            construct: "builtin call",
        }),
    }
}

#[cfg(test)]
mod tests;
