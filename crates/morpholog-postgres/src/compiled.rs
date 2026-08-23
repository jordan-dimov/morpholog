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
//! are observational (a symmetric self-join names the violating pair in a
//! different order; a body minting `new Subject()` yields fresh ids).

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
pub(crate) enum CompileReason {
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
pub(crate) struct CompileRefusal {
    pub(crate) invariant: InvariantName,
    pub(crate) reason: CompileReason,
}

/// Every invariant of a programme, compiled. Programme order is preserved:
/// the runner evaluates in this order and refuses on the first violation,
/// matching the kernel's first-failure contract.
#[derive(Debug)]
pub(crate) struct CompiledInvariantSet {
    pub(crate) invariants: Vec<CompiledInvariant>,
}

/// How much of a compiled invariant a transition's delta touches.
#[derive(Debug, Clone, PartialEq, Eq)]
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
    position: usize,
    kind: PredicateArgKind,
}

/// A claim pattern occurring anywhere in the body: which delta claims can
/// affect this invariant, and how their constants bound the antecedent.
#[derive(Debug, Clone)]
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
pub(crate) struct CompiledInvariant {
    pub(crate) name: InvariantName,
    pub(crate) version: u32,
    pub(crate) footprint: BTreeSet<PredicateName>,
    /// Witness variables, sorted by name. Each violation row carries the
    /// full tagged value as `w_<var>`, decoded through `EvalValue`'s own
    /// serde - the one wire contract, no second kind decoder.
    pub(crate) witness_vars: Vec<Var>,
    occurrences: Vec<OccurrenceBinder>,
    case_cols: BTreeMap<Var, ColRef>,
    sql_select_from_where: String,
    sql_order_limit: String,
}

impl CompiledInvariant {
    /// The violation query. `case_filter` is a stage-2 bound produced by
    /// [`Self::case_filter`]; `None` is the full stage-1 check.
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

/// An invariant name is an opaque string in hand-built IR; neutralise the
/// sequences that could close or break the block comment it travels in.
fn comment_safe(name: &str) -> String {
    name.replace(['\r', '\n'], " ").replace("*/", "* /")
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
    footprint: BTreeSet<PredicateName>,
    occurrences: Vec<RawOccurrence>,
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
        footprint: BTreeSet::new(),
        occurrences: Vec::new(),
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
        footprint: ctx.footprint,
        witness_vars,
        occurrences,
        case_cols,
        sql_select_from_where: select_from_where,
        sql_order_limit: order_limit,
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
            .map(|col| format!("({})::text", col_sql(col, Repr::Text)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    (select, order)
}

/// How a claim-argument position is read in SQL so that SQL equality is
/// kernel equality. See the module doc for the per-kind proofs.
#[derive(Clone, Copy, PartialEq)]
enum Repr {
    Text,
    Numeric,
    /// The whole tagged value; sound only for kinds whose canonical
    /// serialisation makes structural equality semantic equality.
    Jsonb,
}

fn repr_for(kind: &PredicateArgKind) -> Result<Repr, CompileReason> {
    match kind {
        PredicateArgKind::Decimal => Ok(Repr::Numeric),
        PredicateArgKind::Subject => Ok(Repr::Text),
        PredicateArgKind::Bool
        | PredicateArgKind::Date
        | PredicateArgKind::Timestamp
        | PredicateArgKind::Duration => Ok(Repr::Jsonb),
        PredicateArgKind::Quantity(_)
        | PredicateArgKind::Collection
        | PredicateArgKind::Any
        | PredicateArgKind::CalendarSpan => Err(CompileReason::ArgumentKind { kind: kind.clone() }),
    }
}

fn col_sql(col: &ColRef, repr: Repr) -> String {
    let ColRef {
        alias, position, ..
    } = col;
    match repr {
        Repr::Text => format!("{alias}.arguments -> {position} ->> 'value'"),
        Repr::Numeric => format!("({alias}.arguments -> {position} ->> 'value')::numeric"),
        Repr::Jsonb => format!("{alias}.arguments -> {position}"),
    }
}

fn col_eq(a: &ColRef, b: &ColRef) -> Result<String, CompileReason> {
    let repr = repr_for(&a.kind)?;
    Ok(format!("({}) = ({})", col_sql(a, repr), col_sql(b, repr)))
}

/// A literal filter on a claim position, or a refusal when the literal
/// kind is outside the fragment.
fn literal_sql(value: &Value) -> Result<(String, Repr), CompileReason> {
    match value {
        Value::Subject(s) => Ok((quote_literal(s.as_str()), Repr::Text)),
        Value::Decimal(d) => Ok((format!("{}::numeric", quote_literal(d)), Repr::Numeric)),
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
/// value kind cannot be rendered (widens to Unbounded).
fn const_eq(col: &ColRef, ev: &EvalValue) -> Option<String> {
    match ev {
        EvalValue::Subject(s) => Some(format!(
            "({}) = {}",
            col_sql(col, Repr::Text),
            quote_literal(s.as_str())
        )),
        EvalValue::Decimal(d) => Some(format!(
            "({}) = {}::numeric",
            col_sql(col, Repr::Numeric),
            quote_literal(&d.to_string())
        )),
        _ => None,
    }
}

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
    ctx.footprint.insert(predicate.clone());

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
                guards.push((i, v.clone()));
            }
            Term::Var(v) => {
                var_map.push((i, v.clone()));
                if let Some(bound) = env.get(v) {
                    where_.push(col_eq(bound, &col)?);
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
                    col_sql(col, Repr::Numeric)
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
mod tests {
    use super::*;
    use morpholog_core::ir_builder as b;
    use morpholog_test_support::{claim_instance, dec, subj};

    fn ledger_program() -> morpholog_core::Program {
        morpholog_examples::double_entry_ledger::program()
    }

    fn compiled(program: &morpholog_core::Program) -> CompiledInvariantSet {
        compile_invariants(program.validated().expect("gallery programme validates"))
            .expect("every invariant is in the fragment")
    }

    fn refusals(program: &morpholog_core::Program) -> Vec<CompileRefusal> {
        compile_invariants(program.validated().expect("programme validates"))
            .expect_err("expected at least one refusal")
    }

    #[test]
    fn ledger_compiles_fully_in_programme_order() {
        let program = ledger_program();
        let set = compiled(&program);
        let names: Vec<&str> = set.invariants.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "supersedes_unique_by_prior_entry_id",
                "balanced_posted_entry",
                "journal_entry_has_lines"
            ],
            "generated discipline invariant first, then authored order"
        );
    }

    #[test]
    fn balanced_posted_entry_sql_is_pinned() {
        let program = ledger_program();
        let set = compiled(&program);
        let sql = set.invariants[1].violation_sql(None);
        assert_eq!(
            sql,
            r#"/* morpholog compiled invariant balanced_posted_entry v1 stage1 */
SELECT (t0.arguments -> 0)::text AS "w_entry"
FROM morpholog.claims t0
WHERE t0.predicate_name = 'JournalEntry'
  AND NOT ((COALESCE((SELECT sum((t1.arguments -> 2 ->> 'value')::numeric) FROM morpholog.claims t1 WHERE t1.predicate_name = 'JournalLine' AND (t0.arguments -> 0 ->> 'value') = (t1.arguments -> 0 ->> 'value')), 0::numeric)) = (COALESCE((SELECT sum((t2.arguments -> 3 ->> 'value')::numeric) FROM morpholog.claims t2 WHERE t2.predicate_name = 'JournalLine' AND (t0.arguments -> 0 ->> 'value') = (t2.arguments -> 0 ->> 'value')), 0::numeric)))
ORDER BY (t0.arguments -> 0 ->> 'value')::text
LIMIT 1"#
        );
    }

    #[test]
    fn supersedes_uniqueness_sql_is_pinned() {
        let program = ledger_program();
        let set = compiled(&program);
        let sql = set.invariants[0].violation_sql(None);
        assert_eq!(
            sql,
            r#"/* morpholog compiled invariant supersedes_unique_by_prior_entry_id v1 stage1 */
SELECT (t0.arguments -> 0)::text AS "w_new_entry_id_a",
       (t1.arguments -> 0)::text AS "w_new_entry_id_b",
       (t0.arguments -> 1)::text AS "w_prior_entry_id"
FROM morpholog.claims t0, morpholog.claims t1
WHERE t0.predicate_name = 'Supersedes'
  AND t1.predicate_name = 'Supersedes'
  AND (t0.arguments -> 1 ->> 'value') = (t1.arguments -> 1 ->> 'value')
  AND NOT ((t0.arguments -> 0 ->> 'value') = (t1.arguments -> 0 ->> 'value'))
ORDER BY (t0.arguments -> 0 ->> 'value')::text, (t1.arguments -> 0 ->> 'value')::text, (t0.arguments -> 1 ->> 'value')::text
LIMIT 1"#
        );
    }

    #[test]
    fn journal_entry_has_lines_sql_is_pinned() {
        let program = ledger_program();
        let set = compiled(&program);
        let sql = set.invariants[2].violation_sql(None);
        assert_eq!(
            sql,
            r#"/* morpholog compiled invariant journal_entry_has_lines v1 stage1 */
SELECT (t0.arguments -> 0)::text AS "w_entry"
FROM morpholog.claims t0
WHERE t0.predicate_name = 'JournalEntry'
  AND NOT EXISTS (SELECT 1 FROM morpholog.claims t1 WHERE t1.predicate_name = 'JournalLine' AND (t0.arguments -> 0 ->> 'value') = (t1.arguments -> 0 ->> 'value'))
ORDER BY (t0.arguments -> 0 ->> 'value')::text
LIMIT 1"#
        );
    }

    #[test]
    fn post_simple_entry_delta_bounds_every_ledger_invariant_to_the_entry() {
        let program = ledger_program();
        let set = compiled(&program);
        let asserted = vec![
            claim_instance("JournalEntry", &[subj("e42"), subj("d1"), subj("p1")]),
            claim_instance(
                "JournalLine",
                &[subj("e42"), subj("cash"), dec(100), dec(0)],
            ),
            claim_instance("JournalLine", &[subj("e42"), subj("rev"), dec(0), dec(100)]),
        ];
        let balanced = &set.invariants[1];
        assert_eq!(
            balanced.case_filter(&asserted, &[]),
            CaseFilter::Bounded("((t0.arguments -> 0 ->> 'value') = 'e42')".to_string())
        );
        // Supersedes is untouched by this delta entirely.
        assert_eq!(
            set.invariants[0].case_filter(&asserted, &[]),
            CaseFilter::Untouched
        );
    }

    #[test]
    fn close_period_delta_touches_no_ledger_invariant() {
        let program = ledger_program();
        let set = compiled(&program);
        let asserted = vec![claim_instance("PeriodClosed", &[subj("p1")])];
        for inv in &set.invariants {
            assert_eq!(
                inv.case_filter(&asserted, &[]),
                CaseFilter::Untouched,
                "{} should be skipped for close_period",
                inv.name
            );
        }
    }

    #[test]
    fn retraction_also_touches_cases() {
        let program = ledger_program();
        let set = compiled(&program);
        let retracted = vec![claim_instance(
            "JournalLine",
            &[subj("e7"), subj("cash"), dec(5), dec(0)],
        )];
        assert_eq!(
            set.invariants[1].case_filter(&[], &retracted),
            CaseFilter::Bounded("((t0.arguments -> 0 ->> 'value') = 'e7')".to_string())
        );
    }

    #[test]
    fn out_of_fragment_constructs_refuse_by_name_and_variant() {
        let source = "\
program compiled_refusals

predicate A(x: Subject)
predicate B(x: Subject)

invariant uses_or:
    A(x) implies (B(x) or A(x))

invariant uses_pre:
    pre(A(x)) implies A(x)

invariant in_fragment:
    A(x) implies B(x)
";
        let program = morpholog_surface::parse_program(source).expect("parses");
        let refused = refusals(&program);
        let names: Vec<&str> = refused.iter().map(|r| r.invariant.as_str()).collect();
        assert_eq!(
            names,
            ["uses_or", "uses_pre"],
            "whole-run refusal names each offender, in-fragment invariant not blamed"
        );
        assert_eq!(
            refused[0].reason,
            CompileReason::Construct { construct: "or" }
        );
        assert_eq!(
            refused[1].reason,
            CompileReason::Construct { construct: "pre" }
        );
    }

    /// One refusing programme per out-of-fragment family the current IR
    /// can spell, each pinned to its typed reason - so the fragment
    /// boundary is enforced by variant, never by message text.
    #[test]
    fn every_out_of_fragment_family_refuses_with_its_typed_reason() {
        let cases: Vec<(&str, &str, CompileReason)> = vec![
            (
                "xor",
                "invariant r:\n    A(x) implies (B(x) xor A(x))\n",
                CompileReason::Construct { construct: "xor" },
            ),
            (
                // Membership needs a bound collection, and binding a
                // Collection-kinded position refuses first - the kind
                // tier owns this family; the `Prop::In` arm is defence.
                "in",
                "invariant r:\n    Cx(x, c) implies x in c\n",
                CompileReason::ArgumentKind {
                    kind: PredicateArgKind::Collection,
                },
            ),
            (
                "defined",
                "define d(x):\n    B(x)\n\ninvariant r:\n    A(x) implies d(x)\n",
                CompileReason::Construct {
                    construct: "defined call",
                },
            ),
            (
                "temporal comparison",
                "invariant r:\n    Dated(x, d1, d2) implies d1 on_or_before d2\n",
                CompileReason::ComparisonDomain {
                    domain: OrderedDomain::Date,
                },
            ),
            (
                "arithmetic",
                "invariant r:\n    Amount(x, n) implies 0 <= n + 1\n",
                CompileReason::Construct {
                    construct: "arithmetic",
                },
            ),
            (
                "extremum",
                "invariant r:\n    Amount(x, n) implies max(m | Amount(_, m)) <= 100\n",
                CompileReason::Construct {
                    construct: "extremum",
                },
            ),
            (
                "value lookup",
                "invariant r:\n    Amount(x, n) implies n <= value Cap(_)\n",
                CompileReason::Construct {
                    construct: "value lookup",
                },
            ),
            (
                "conditional value",
                "invariant r:\n    Amount(x, n) implies n <= if(A(x), 1, 2)\n",
                CompileReason::Construct {
                    construct: "conditional value",
                },
            ),
            (
                "builtin call",
                "invariant r:\n    Amount(x, n) implies abs(n) <= 100\n",
                CompileReason::Construct {
                    construct: "builtin call",
                },
            ),
            (
                "quantity kind",
                "invariant r:\n    Qty(x, q) implies Qty2(x, q)\n",
                CompileReason::ArgumentKind {
                    kind: PredicateArgKind::Quantity(morpholog_core::Unit::from("t".to_string())),
                },
            ),
            (
                "collection kind",
                "invariant r:\n    Cx(x, c) implies Cx2(x, c)\n",
                CompileReason::ArgumentKind {
                    kind: PredicateArgKind::Collection,
                },
            ),
            (
                "date literal",
                "invariant r:\n    Dated(x, @2026-01-01, _) implies A(x)\n",
                CompileReason::Literal { kind: "date" },
            ),
            (
                "computed sum target",
                "invariant r:\n    Cap(cap) implies sum(n * 2 | Amount(_, n)) <= cap\n",
                CompileReason::SumShape {
                    detail: "computed target",
                },
            ),
        ];
        let decls = "\
program family_refusals

predicate A(x: Subject)
predicate B(x: Subject)
predicate Amount(x: Subject, n: Decimal)
predicate Cap(cap: Decimal)
predicate Dated(x: Subject, opened: Date, closed: Date)
predicate Qty(x: Subject, q: Decimal[t])
predicate Qty2(x: Subject, q: Decimal[t])
predicate Cx(x: Subject, c: Collection)
predicate Cx2(x: Subject, c: Collection)

";
        for (family, invariant_src, expected) in cases {
            let source = format!("{decls}{invariant_src}");
            let program = morpholog_surface::parse_program(&source)
                .unwrap_or_else(|e| panic!("{family}: parse failed: {e:?}"));
            let refused = refusals(&program);
            assert_eq!(
                refused.len(),
                1,
                "{family}: expected exactly the one refusal, got {refused:?}"
            );
            assert_eq!(refused[0].reason, expected, "family: {family}");
        }
    }

    #[test]
    fn a_date_position_joins_as_the_tagged_value() {
        // Date/Timestamp/Bool/Duration positions are lawful join keys:
        // equality runs over the whole tagged value, whose canonical
        // serialisation makes structural equality semantic equality.
        let source = "\
program tagged_join

predicate Opened(x: Subject, on: Date)
predicate Closed(x: Subject, on: Date)

invariant closed_on_the_open_date:
    Closed(x, d) implies Opened(x, d)
";
        let program = morpholog_surface::parse_program(source).expect("parses");
        let set = compiled(&program);
        let sql = set.invariants[0].violation_sql(None);
        assert!(
            sql.contains("(t0.arguments -> 1) = (t1.arguments -> 1)"),
            "date join compares tagged jsonb, got:\n{sql}"
        );
    }

    #[test]
    fn the_provenance_comment_neutralises_a_hostile_invariant_name() {
        let program = b::program("hostile")
            .predicates(vec![b::predicate("A").subject("x").build()])
            .invariants(vec![b::invariant(
                "evil */ DROP TABLE morpholog.claims; /*\nline",
                b::implies(
                    b::claim("A", vec![b::var("x")]),
                    b::claim("A", vec![b::var("x")]),
                ),
            )])
            .build();
        let set = compile_invariants(program.validated().expect("validates"))
            .expect("in-fragment body compiles");
        let sql = set.invariants[0].violation_sql(None);
        let comment_end = sql.find("*/\n").expect("comment closes once");
        assert!(
            !sql[..comment_end].contains("DROP TABLE") || sql[..comment_end].contains("* /"),
            "the name's comment-closer must be neutralised"
        );
        assert!(
            !sql[..comment_end].contains('\n') || sql.starts_with("/*"),
            "no raw newline inside the comment head"
        );
        assert!(sql.contains("* /"), "escaped closer present, got:\n{sql}");
    }
}
