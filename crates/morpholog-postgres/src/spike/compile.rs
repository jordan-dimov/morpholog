//! Spike: compile fragment invariants into SQL violation queries.
//!
//! Denial orientation: each invariant compiles to one query that returns a
//! witnessing row when the invariant FAILS over the claims table and no rows
//! when it holds. The kernel semantics being reproduced: an invariant holds
//! iff its body yields at least one binding witness; `Implies`/`Forall`/
//! `Exists`/`Not` export no bindings; `And` threads bindings left to right;
//! decimal comparison is scale-insensitive (`::numeric`, never JSON text).
//!
//! The compiler is also the fragment classifier: anything outside the
//! fragment is a `CompileRefusal`, collected whole-run like
//! `sql_views::ViewRefusal`. Callers fall back to the interpreted path.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use morpholog_core::{
    ClaimInstance, EvalValue, Invariant, InvariantName, PredicateArgKind, PredicateDecl,
    PredicateName, Program, Prop, SumSeed, Term, Value, ValueExpr, Var,
};

use crate::sql_quote::{quote_ident, quote_literal};

/// One invariant the compiler could not express in the fragment.
#[derive(Debug, Clone)]
pub struct CompileRefusal {
    pub invariant: InvariantName,
    pub reason: String,
}

/// Every invariant of a programme, compiled. Programme order is preserved:
/// the runner evaluates in this order and refuses on the first violation,
/// matching the kernel's first-failure contract.
#[derive(Debug)]
pub struct CompiledInvariantSet {
    pub invariants: Vec<CompiledInvariant>,
}

/// How much of a compiled invariant a transition's delta touches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaseFilter {
    /// Delta disjoint from the invariant's occurrences: skip it entirely.
    Untouched,
    /// The touched cases, as a SQL conjunction/disjunction over the
    /// antecedent's columns - splice into the stage-1 query.
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
pub struct CompiledInvariant {
    pub name: InvariantName,
    pub version: u32,
    pub footprint: BTreeSet<PredicateName>,
    /// Witness columns, sorted by variable name; decoded from the
    /// violation row by declared kind.
    pub witness_cols: Vec<(Var, PredicateArgKind)>,
    occurrences: Vec<OccurrenceBinder>,
    case_cols: BTreeMap<Var, ColRef>,
    sql_select_from_where: String,
    sql_order_limit: String,
}

impl CompiledInvariant {
    /// The violation query. `case_filter` is a stage-2 bound produced by
    /// [`Self::case_filter`]; `None` is the full stage-1 check.
    pub fn violation_sql(&self, case_filter: Option<&str>) -> String {
        let stage = if case_filter.is_some() { 2 } else { 1 };
        let mut sql = format!(
            "/* morpholog-spike invariant {} v{} stage{} */\n{}",
            self.name, self.version, stage, self.sql_select_from_where
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
    pub fn case_filter(&self, asserted: &[ClaimInstance], retracted: &[ClaimInstance]) -> CaseFilter {
        let mut disjuncts: BTreeSet<String> = BTreeSet::new();
        let mut touched = false;
        for claim in asserted.iter().chain(retracted) {
            for occ in &self.occurrences {
                if occ.predicate != claim.predicate {
                    continue;
                }
                if !occ
                    .guards
                    .iter()
                    .all(|(pos, lit)| claim.args.get(*pos).is_some_and(|ev| literal_matches(lit, ev)))
                {
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

/// Compile every invariant of a validated programme, or report every
/// refusal (whole-run: nothing compiles unless everything does).
pub fn compile_invariants(program: &Program) -> Result<CompiledInvariantSet, Vec<CompileRefusal>> {
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

struct Ctx<'a> {
    decls: &'a BTreeMap<&'a str, &'a PredicateDecl>,
    counter: usize,
    footprint: BTreeSet<PredicateName>,
    occurrences: Vec<(PredicateName, Vec<(usize, Value)>, Vec<(usize, Var)>)>,
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
) -> Result<CompiledInvariant, String> {
    let mut ctx = Ctx {
        decls,
        counter: 0,
        footprint: BTreeSet::new(),
        occurrences: Vec::new(),
    };

    let (select_from_where, order_limit, case_cols) = match &inv.body {
        Prop::Implies { left, right } => {
            compile_denial(left, right, &mut ctx)?
        }
        Prop::Forall { binding: _, source, body } => {
            compile_denial(source, body, &mut ctx)?
        }
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

    let witness_cols = case_cols
        .iter()
        .map(|(v, c)| (v.clone(), c.kind.clone()))
        .collect();
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
        witness_cols,
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
) -> Result<(String, String, Env), String> {
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
fn generic_denial(body: &Prop, ctx: &mut Ctx<'_>) -> Result<(String, String, Env), String> {
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
) -> Result<(String, String, Env), String> {
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
        r.env
            .iter()
            .map(|(v, col)| {
                format!(
                    "({})::text AS {}",
                    col_sql(col, Repr::Text),
                    quote_ident(&format!("w_{v}"))
                )
            })
            .collect::<Vec<_>>()
            .join(",\n       ")
    };
    let order = r
        .from
        .iter()
        .map(|(alias, _)| format!("{alias}.arguments"))
        .collect::<Vec<_>>()
        .join(", ");
    (select, order)
}

#[derive(Clone, Copy, PartialEq)]
enum Repr {
    Text,
    Numeric,
    Jsonb,
}

fn repr_for(kind: &PredicateArgKind) -> Repr {
    match kind {
        PredicateArgKind::Decimal => Repr::Numeric,
        PredicateArgKind::Subject => Repr::Text,
        _ => Repr::Jsonb,
    }
}

fn col_sql(col: &ColRef, repr: Repr) -> String {
    let ColRef { alias, position, .. } = col;
    match repr {
        Repr::Text => format!("{alias}.arguments -> {position} ->> 'value'"),
        Repr::Numeric => format!("({alias}.arguments -> {position} ->> 'value')::numeric"),
        Repr::Jsonb => format!("{alias}.arguments -> {position}"),
    }
}

fn col_eq(a: &ColRef, b: &ColRef) -> String {
    let repr = repr_for(&a.kind);
    format!("({}) = ({})", col_sql(a, repr), col_sql(b, repr))
}

/// A literal filter on a claim position, or None when the literal kind is
/// outside the fragment.
fn literal_sql(value: &Value) -> Result<(String, Repr), String> {
    match value {
        Value::Subject(s) => Ok((quote_literal(s.as_str()), Repr::Text)),
        Value::Decimal(d) => Ok((format!("{}::numeric", quote_literal(d)), Repr::Numeric)),
        other => Err(format!("literal kind outside fragment: {other:?}")),
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
        (Value::Decimal(a), EvalValue::Decimal(b)) => {
            a.parse::<rust_decimal::Decimal>().is_ok_and(|a| a == *b)
        }
        // Guard kinds the fragment cannot compare: treat as matching, which
        // only widens the touched-case set.
        _ => true,
    }
}

fn render_prop(prop: &Prop, env: Env, ctx: &mut Ctx<'_>) -> Result<Rendered, String> {
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
        Prop::Forall { binding: _, source, body } => render_prop(
            &Prop::Implies {
                left: source.clone(),
                right: body.clone(),
            },
            env,
            ctx,
        ),
        Prop::Eq(a, b) => compare_sql(a, b, "=", &env, ctx),
        Prop::Neq(a, b) => compare_sql(a, b, "<>", &env, ctx),
        Prop::Compare { op, domain, left, right } => {
            if *domain != morpholog_core::OrderedDomain::Decimal {
                return Err(format!("comparison domain outside fragment: {domain:?}"));
            }
            let op = match op {
                morpholog_core::CompareOp::Le => "<=",
                morpholog_core::CompareOp::Lt => "<",
                morpholog_core::CompareOp::Ge => ">=",
                morpholog_core::CompareOp::Gt => ">",
            };
            compare_sql(left, right, op, &env, ctx)
        }
        Prop::Or(_) => Err("or outside fragment".to_string()),
        Prop::Xor(_, _) => Err("xor outside fragment".to_string()),
        Prop::Pre(_) => Err("pre outside fragment".to_string()),
        Prop::Defined { name, .. } => Err(format!("defined call outside fragment: {name}")),
        Prop::In(_, _) => Err("in outside fragment".to_string()),
    }
}

fn render_claim(
    predicate: &PredicateName,
    args: &[Term],
    mut env: Env,
    ctx: &mut Ctx<'_>,
) -> Result<Rendered, String> {
    let decl = ctx
        .decls
        .get(predicate.as_str())
        .ok_or_else(|| format!("undeclared predicate {predicate}"))?;
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
        let kind = decl
            .args
            .get(i)
            .map(|a| a.kind.clone())
            .ok_or_else(|| format!("arity mismatch on {predicate}"))?;
        let col = ColRef {
            alias: alias.clone(),
            position: i,
            kind,
        };
        match term {
            Term::Wildcard => {}
            Term::Actor => return Err("actor in invariant body".to_string()),
            Term::Literal(v) => {
                let (lit, repr) = literal_sql(v)?;
                where_.push(format!("({}) = {}", col_sql(&col, repr), lit));
                guards.push((i, v.clone()));
            }
            Term::Var(v) => {
                var_map.push((i, v.clone()));
                if let Some(bound) = env.get(v) {
                    where_.push(col_eq(bound, &col));
                } else {
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
) -> Result<Rendered, String> {
    let (a_sql, _) = value_sql(a, env, ctx)?;
    let (b_sql, _) = value_sql(b, env, ctx)?;
    Ok(Rendered {
        from: Vec::new(),
        where_: vec![format!("({a_sql}) {op} ({b_sql})")],
        env: env.clone(),
    })
}

fn value_sql(
    expr: &ValueExpr,
    env: &Env,
    ctx: &mut Ctx<'_>,
) -> Result<(String, PredicateArgKind), String> {
    match expr {
        ValueExpr::Term(Term::Var(v)) => {
            let col = env
                .get(v)
                .ok_or_else(|| format!("unbound variable in value position: {v}"))?;
            Ok((col_sql(col, repr_for(&col.kind)), col.kind.clone()))
        }
        ValueExpr::Term(Term::Literal(v)) => {
            let (lit, repr) = literal_sql(v)?;
            let kind = if repr == Repr::Numeric {
                PredicateArgKind::Decimal
            } else {
                PredicateArgKind::Subject
            };
            Ok((lit, kind))
        }
        ValueExpr::Term(Term::Wildcard) => Err("wildcard in value position".to_string()),
        ValueExpr::Term(Term::Actor) => Err("actor in invariant body".to_string()),
        ValueExpr::Sum { value, body, seed } => {
            if *seed != SumSeed::Decimal {
                return Err("non-decimal sum seed outside fragment".to_string());
            }
            let r = render_prop(body, env.clone(), ctx)?;
            if r.from.is_empty() {
                return Err("sum body binds no claims".to_string());
            }
            let val = match value {
                Term::Var(v) => {
                    let col = r
                        .env
                        .get(v)
                        .ok_or_else(|| format!("sum target unbound: {v}"))?;
                    if col.kind != PredicateArgKind::Decimal {
                        return Err(format!("sum over non-decimal kind: {}", col.kind));
                    }
                    col_sql(col, Repr::Numeric)
                }
                Term::Literal(Value::Decimal(d)) => format!("{}::numeric", quote_literal(d)),
                other => return Err(format!("sum target outside fragment: {other:?}")),
            };
            Ok((
                format!(
                    "COALESCE((SELECT sum({val}) FROM {} WHERE {}), 0::numeric)",
                    r.from
                        .iter()
                        .map(|(_, f)| f.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    r.conjunction()
                ),
                PredicateArgKind::Decimal,
            ))
        }
        ValueExpr::Arith { .. } => Err("arithmetic outside fragment".to_string()),
        ValueExpr::Extremum { .. } => Err("extremum outside fragment".to_string()),
        ValueExpr::ValueOf { .. } => Err("value lookup outside fragment".to_string()),
        ValueExpr::Abs(_) => Err("abs outside fragment".to_string()),
        ValueExpr::Round { .. } => Err("round outside fragment".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use morpholog_test_support::{claim_instance, dec, subj};

    fn ledger() -> CompiledInvariantSet {
        compile_invariants(&morpholog_examples::double_entry_ledger::program())
            .expect("every ledger invariant is in the fragment")
    }

    #[test]
    fn ledger_compiles_fully_in_programme_order() {
        let set = ledger();
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
        let set = ledger();
        let sql = set.invariants[1].violation_sql(None);
        assert_eq!(
            sql,
            r#"/* morpholog-spike invariant balanced_posted_entry v1 stage1 */
SELECT (t0.arguments -> 0 ->> 'value')::text AS "w_entry"
FROM morpholog.claims t0
WHERE t0.predicate_name = 'JournalEntry'
  AND NOT ((COALESCE((SELECT sum((t1.arguments -> 2 ->> 'value')::numeric) FROM morpholog.claims t1 WHERE t1.predicate_name = 'JournalLine' AND (t0.arguments -> 0 ->> 'value') = (t1.arguments -> 0 ->> 'value')), 0::numeric)) = (COALESCE((SELECT sum((t2.arguments -> 3 ->> 'value')::numeric) FROM morpholog.claims t2 WHERE t2.predicate_name = 'JournalLine' AND (t0.arguments -> 0 ->> 'value') = (t2.arguments -> 0 ->> 'value')), 0::numeric)))
ORDER BY t0.arguments
LIMIT 1"#
        );
    }

    #[test]
    fn supersedes_uniqueness_sql_is_pinned() {
        let set = ledger();
        let sql = set.invariants[0].violation_sql(None);
        assert_eq!(
            sql,
            r#"/* morpholog-spike invariant supersedes_unique_by_prior_entry_id v1 stage1 */
SELECT (t0.arguments -> 0 ->> 'value')::text AS "w_new_entry_id_a",
       (t1.arguments -> 0 ->> 'value')::text AS "w_new_entry_id_b",
       (t0.arguments -> 1 ->> 'value')::text AS "w_prior_entry_id"
FROM morpholog.claims t0, morpholog.claims t1
WHERE t0.predicate_name = 'Supersedes'
  AND t1.predicate_name = 'Supersedes'
  AND (t0.arguments -> 1 ->> 'value') = (t1.arguments -> 1 ->> 'value')
  AND NOT ((t0.arguments -> 0 ->> 'value') = (t1.arguments -> 0 ->> 'value'))
ORDER BY t0.arguments, t1.arguments
LIMIT 1"#
        );
    }

    #[test]
    fn journal_entry_has_lines_sql_is_pinned() {
        let set = ledger();
        let sql = set.invariants[2].violation_sql(None);
        assert_eq!(
            sql,
            r#"/* morpholog-spike invariant journal_entry_has_lines v1 stage1 */
SELECT (t0.arguments -> 0 ->> 'value')::text AS "w_entry"
FROM morpholog.claims t0
WHERE t0.predicate_name = 'JournalEntry'
  AND NOT EXISTS (SELECT 1 FROM morpholog.claims t1 WHERE t1.predicate_name = 'JournalLine' AND (t0.arguments -> 0 ->> 'value') = (t1.arguments -> 0 ->> 'value'))
ORDER BY t0.arguments
LIMIT 1"#
        );
    }

    #[test]
    fn post_simple_entry_delta_bounds_every_ledger_invariant_to_the_entry() {
        let set = ledger();
        let asserted = vec![
            claim_instance("JournalEntry", &[subj("e42"), subj("d1"), subj("p1")]),
            claim_instance("JournalLine", &[subj("e42"), subj("cash"), dec(100), dec(0)],
            ),
            claim_instance("JournalLine", &[subj("e42"), subj("rev"), dec(0), dec(100)],
            ),
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
        let set = ledger();
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
        let set = ledger();
        let retracted = vec![claim_instance("JournalLine", &[subj("e7"), subj("cash"), dec(5), dec(0)],
        )];
        assert_eq!(
            set.invariants[1].case_filter(&[], &retracted),
            CaseFilter::Bounded("((t0.arguments -> 0 ->> 'value') = 'e7')".to_string())
        );
    }

    #[test]
    fn out_of_fragment_constructs_refuse_by_name() {
        let source = r#"program spike_refusals

predicate A(x: Subject)
predicate B(x: Subject)
predicate C(x: Subject, d: Date)

invariant uses_or:
    A(x) implies (B(x) or A(x))

invariant uses_pre:
    pre(A(x)) implies A(x)

invariant in_fragment:
    A(x) implies B(x)
"#;
        let program = morpholog_surface::parse_program(source).expect("parses");
        let refusals = compile_invariants(&program).expect_err("or and pre refuse");
        let refused: Vec<&str> = refusals.iter().map(|r| r.invariant.as_str()).collect();
        assert_eq!(refused, ["uses_or", "uses_pre"], "whole-run refusal names each offender, in-fragment invariant not blamed");
        assert!(refusals[0].reason.contains("or"));
        assert!(refusals[1].reason.contains("pre"));
    }
}
