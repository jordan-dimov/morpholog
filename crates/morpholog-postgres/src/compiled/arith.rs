//! Decimal and quantity arithmetic, and the builtins over them, rendered
//! as SQL that computes the kernel's value. Each value is an amount and a
//! unit read from the stored value's own tag (no unit for a bare decimal),
//! and the amount is NULL exactly where the kernel errs: every step is
//! the kernel's operation fitted as rust_decimal fits it
//! (`morpholog.decimal_fit_v1` and its siblings), and a kind or unit the
//! kernel refuses yields NULL. The kernel itself names the error, over
//! the bindings and sum results the probe reports.

use super::Ctx;
use super::{
    Bound, CompileReason, Env, ErrorReport, Operand, Ordered, Pending, Probe, RenderedSum, SumPart,
    quote_literal, tagged_json, tagged_sql, value_sql,
};
use morpholog_core::{ArithOp, Builtin, EvalValue, PredicateArgKind, Term, Value, ValueExpr, Var};

/// Whether an expression is a bare decimal or a quantity. Which unit is
/// read at run time, from the values.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Flavour {
    Bare,
    Unit,
}

struct Dd {
    amount: String,
    unit: String,
    flavour: Flavour,
}

const NO_UNIT: &str = "NULL::text";

fn time_arithmetic() -> CompileReason {
    CompileReason::Construct {
        construct: "time arithmetic",
    }
}

fn across_flavours(what: &str) -> CompileReason {
    CompileReason::UnvalidatedShape {
        detail: format!("{what} across value flavours"),
    }
}

/// A computed value as a comparison operand, with the probe that
/// reports where it errs queued in the kernel's evaluation order.
pub(super) fn computed_operand(
    expr: &ValueExpr,
    env: &Env,
    ctx: &mut Ctx<'_>,
) -> Result<Operand, CompileReason> {
    let start = ctx.pending.len();
    let mut sums = Vec::new();
    let dd = render(expr, env, ctx, &mut sums)?;
    // The sums inside report through this value's probe, in walk order.
    let inner: Vec<Pending> = ctx.pending.drain(start..).collect();
    for p in inner {
        match p {
            Pending::Sum(s) => ctx.pending.push(Pending::Covered(s.lateral)),
            other => ctx.pending.push(other),
        }
    }
    let index = ctx.values.len();
    ctx.values.push(expr.clone());
    let mut vars = Vec::new();
    bound_vars(expr, env, &mut vars)?;
    ctx.pending.push(Pending::Value(Probe {
        condition: format!("({}) IS NULL", dd.amount),
        report: ErrorReport::Value { index, vars, sums },
    }));
    let tagged = format!(
        "(CASE WHEN ({a}) IS NULL THEN NULL::jsonb WHEN ({u}) IS NULL THEN jsonb_build_object('type', 'decimal', 'value', trim_scale({a})::text) ELSE jsonb_build_object('type', 'quantity', 'value', jsonb_build_object('amount', trim_scale({a})::text, 'unit', {u})) END)",
        a = dd.amount,
        u = dd.unit
    );
    let key = Some(format!("morpholog.value_key_v1({tagged})"));
    let runtime = Some((dd.amount.clone(), dd.unit.clone()));
    Ok(match dd.flavour {
        Flavour::Bare => Operand {
            key,
            dd: runtime,
            static_unit: None,
            ordered: Ordered::Numeric {
                sql: format!("(CASE WHEN ({}) IS NULL THEN {} END)", dd.unit, dd.amount),
                tagged,
            },
        },
        Flavour::Unit => Operand {
            key,
            dd: runtime,
            static_unit: None,
            ordered: Ordered::Quantity { tagged },
        },
    })
}

fn render(
    expr: &ValueExpr,
    env: &Env,
    ctx: &mut Ctx<'_>,
    sums: &mut Vec<SumPart>,
) -> Result<Dd, CompileReason> {
    match expr {
        ValueExpr::Term(Term::Var(v)) => match env.get(v) {
            Some(Bound::Col(col)) => {
                let flavour = match &col.kind {
                    PredicateArgKind::Decimal => Flavour::Bare,
                    PredicateArgKind::Quantity(_) => Flavour::Unit,
                    PredicateArgKind::Timestamp
                    | PredicateArgKind::Date
                    | PredicateArgKind::Duration => return Err(time_arithmetic()),
                    other => {
                        return Err(CompileReason::ArgumentKind {
                            kind: other.clone(),
                        });
                    }
                };
                let (amount, unit) = runtime_dd(col);
                Ok(Dd {
                    amount,
                    unit,
                    flavour,
                })
            }
            Some(Bound::Lit(value)) => literal(value),
            None => Err(CompileReason::UnvalidatedShape {
                detail: format!("unbound variable in value position: {v}"),
            }),
        },
        ValueExpr::Term(Term::Literal(value)) => literal(value),
        ValueExpr::Term(Term::Wildcard) => Err(CompileReason::Construct {
            construct: "wildcard value",
        }),
        ValueExpr::Term(Term::Actor) => Err(CompileReason::Construct { construct: "actor" }),
        ValueExpr::Sum { .. } => {
            let operand = value_sql(expr, env, ctx)?;
            let Some(Pending::Sum(RenderedSum {
                lateral,
                range_error,
                kind_error,
                kind_report,
            })) = ctx.pending.last()
            else {
                return Err(CompileReason::UnvalidatedShape {
                    detail: "a sum rendered no lateral".to_string(),
                });
            };
            let Ordered::Numeric { sql: total, .. } = operand.ordered else {
                return Err(CompileReason::UnvalidatedShape {
                    detail: "a sum that is not decimal".to_string(),
                });
            };
            sums.push(SumPart {
                lateral: lateral.clone(),
                total: total.clone(),
                kind: kind_error.clone(),
                range: range_error.clone(),
                scope: kind_report.clone(),
            });
            Ok(Dd {
                amount: format!(
                    "(CASE WHEN ({kind_error}) OR ({range_error}) THEN NULL ELSE {total} END)"
                ),
                unit: NO_UNIT.to_string(),
                flavour: Flavour::Bare,
            })
        }
        ValueExpr::Arith { op, left, right } => {
            let l = render(left, env, ctx, sums)?;
            let r = render(right, env, ctx, sums)?;
            within_budget(&[&l, &r])?;
            arith(*op, &l, &r)
        }
        ValueExpr::Call { builtin, args } => {
            let mut rendered = Vec::with_capacity(args.len());
            for arg in args {
                rendered.push(render(arg, env, ctx, sums)?);
            }
            within_budget(&rendered.iter().collect::<Vec<_>>())?;
            call(*builtin, &rendered)
        }
        ValueExpr::Extremum { .. } => Err(CompileReason::Construct {
            construct: "extremum",
        }),
        ValueExpr::ValueOf { .. } => Err(CompileReason::Construct {
            construct: "value lookup",
        }),
        ValueExpr::Cond { .. } => Err(CompileReason::Construct {
            construct: "conditional value",
        }),
    }
}

fn runtime_dd(col: &super::ColRef) -> (String, String) {
    let (a, p) = (&col.alias, col.position);
    (
        format!(
            "(CASE {a}.arguments -> {p} ->> 'type' WHEN 'decimal' THEN ({a}.arguments -> {p} ->> 'value')::numeric WHEN 'quantity' THEN ({a}.arguments -> {p} -> 'value' ->> 'amount')::numeric END)"
        ),
        format!(
            "(CASE WHEN {a}.arguments -> {p} ->> 'type' = 'quantity' THEN {a}.arguments -> {p} -> 'value' ->> 'unit' END)"
        ),
    )
}

/// A decimal- or quantity-declared position's amount and the unit its
/// value carries; none for a position of another kind.
pub(super) fn column_dd(col: &super::ColRef) -> Option<(String, String)> {
    matches!(
        col.kind,
        PredicateArgKind::Decimal | PredicateArgKind::Quantity(_)
    )
    .then(|| runtime_dd(col))
}

/// A decimal or quantity literal's amount and unit; none for another.
pub(super) fn literal_dd(value: &Value) -> Option<(String, String)> {
    literal(value).ok().map(|dd| (dd.amount, dd.unit))
}

fn literal(value: &Value) -> Result<Dd, CompileReason> {
    match value {
        Value::Decimal(d) => Ok(Dd {
            amount: format!("{}::numeric", quote_literal(&d.to_string())),
            unit: NO_UNIT.to_string(),
            flavour: Flavour::Bare,
        }),
        Value::Quantity { amount, unit } => Ok(Dd {
            amount: format!("{}::numeric", quote_literal(&amount.to_string())),
            unit: format!("{}::text", quote_literal(unit.as_str())),
            flavour: Flavour::Unit,
        }),
        Value::Timestamp(_) | Value::Date(_) | Value::Duration(_) | Value::CalendarSpan(_) => {
            Err(time_arithmetic())
        }
        Value::Subject(_) => Err(CompileReason::UnvalidatedShape {
            detail: "a literal of no decimal kind in arithmetic".to_string(),
        }),
    }
}

/// The most SQL one computed value may take before it is combined again.
/// Each operator repeats its operands' text a bounded number of times,
/// so a deep expression grows its SQL; past this it stays interpreted.
const SQL_BUDGET: usize = 64 * 1024;

fn within_budget(operands: &[&Dd]) -> Result<(), CompileReason> {
    let size: usize = operands.iter().map(|d| d.amount.len() + d.unit.len()).sum();
    if size > SQL_BUDGET {
        return Err(CompileReason::Construct {
            construct: "arithmetic too large to compile",
        });
    }
    Ok(())
}

fn fit(sql: &str) -> String {
    format!("morpholog.decimal_fit_v1({sql})")
}

/// The kernel's operator table over decimals and quantities, with the
/// units the values carry.
fn arith(op: ArithOp, l: &Dd, r: &Dd) -> Result<Dd, CompileReason> {
    use Flavour::{Bare, Unit};
    let (xa, xu, ya, yu) = (&l.amount, &l.unit, &r.amount, &r.unit);
    let flavour = match (op, l.flavour, r.flavour) {
        (_, Bare, Bare) => Bare,
        (ArithOp::Add | ArithOp::Sub, Unit, Unit) | (ArithOp::Mul | ArithOp::Div, Unit, Bare) => {
            Unit
        }
        (ArithOp::Mul, Bare, Unit) => Unit,
        (ArithOp::Div, Unit, Unit) => Bare,
        _ => return Err(across_flavours("arithmetic")),
    };
    let (amount, unit) = match op {
        ArithOp::Add | ArithOp::Sub => {
            let sign = if op == ArithOp::Add { "+" } else { "-" };
            (
                format!(
                    "(CASE WHEN ({xu}) IS NOT DISTINCT FROM ({yu}) THEN {} END)",
                    fit(&format!("({xa}) {sign} ({ya})"))
                ),
                xu.clone(),
            )
        }
        ArithOp::Mul => (
            format!(
                "(CASE WHEN ({xu}) IS NULL OR ({yu}) IS NULL THEN {} END)",
                fit(&format!("({xa}) * ({ya})"))
            ),
            format!("COALESCE({xu}, {yu})"),
        ),
        ArithOp::Div => (
            format!(
                "(CASE WHEN ({yu}) IS NULL OR ({xu}) = ({yu}) THEN morpholog.decimal_div_v1({xa}, {ya}) END)"
            ),
            format!("(CASE WHEN ({yu}) IS NULL THEN {xu} END)"),
        ),
        ArithOp::Mod => (
            format!(
                "(CASE WHEN ({xu}) IS NULL AND ({yu}) IS NULL AND ({ya}) <> 0 THEN {} END)",
                fit(&format!("mod({xa}, {ya})"))
            ),
            NO_UNIT.to_string(),
        ),
    };
    Ok(Dd {
        amount,
        unit,
        flavour,
    })
}

/// The builtins the kernel defines over decimals and quantities: `abs`
/// keeps the unit, `min` and `max` need one unit and never skip a failed
/// argument, `round` takes bare decimals only.
fn call(builtin: Builtin, args: &[Dd]) -> Result<Dd, CompileReason> {
    match (builtin, args) {
        (Builtin::Abs, [x]) => Ok(Dd {
            amount: format!("abs({})", x.amount),
            unit: x.unit.clone(),
            flavour: x.flavour,
        }),
        (Builtin::Min | Builtin::Max, [x, y]) => {
            if x.flavour != y.flavour {
                return Err(across_flavours("an extremum"));
            }
            let function = if builtin == Builtin::Min {
                "morpholog.decimal_min_v1"
            } else {
                "morpholog.decimal_max_v1"
            };
            Ok(Dd {
                amount: format!(
                    "(CASE WHEN ({}) IS NOT DISTINCT FROM ({}) THEN {function}({}, {}) END)",
                    x.unit, y.unit, x.amount, y.amount
                ),
                unit: x.unit.clone(),
                flavour: x.flavour,
            })
        }
        (Builtin::Round, [v, q]) => {
            if v.flavour != Flavour::Bare || q.flavour != Flavour::Bare {
                return Err(across_flavours("round"));
            }
            Ok(Dd {
                amount: format!(
                    "(CASE WHEN ({}) IS NULL AND ({}) IS NULL THEN morpholog.decimal_round_v1({}, {}) END)",
                    v.unit, q.unit, v.amount, q.amount
                ),
                unit: NO_UNIT.to_string(),
                flavour: Flavour::Bare,
            })
        }
        (Builtin::PeriodIndex | Builtin::PeriodStartOf | Builtin::Periods, _) => {
            Err(time_arithmetic())
        }
        _ => Err(CompileReason::UnvalidatedShape {
            detail: format!("builtin `{}` with {} arguments", builtin.name(), args.len()),
        }),
    }
}

/// The variables a computed value reads outside its sums, as tagged
/// values the kernel evaluates the expression over.
fn bound_vars(
    expr: &ValueExpr,
    env: &Env,
    out: &mut Vec<(String, String)>,
) -> Result<(), CompileReason> {
    match expr {
        ValueExpr::Term(Term::Var(v)) => {
            if out.iter().any(|(name, _)| name == v.as_str()) {
                return Ok(());
            }
            let tagged = match env.get(v) {
                Some(Bound::Col(col)) => tagged_sql(col),
                Some(Bound::Lit(value)) => format!(
                    "{}::jsonb",
                    quote_literal(&tagged_json(&EvalValue::from(value))?)
                ),
                None => {
                    return Err(CompileReason::UnvalidatedShape {
                        detail: format!("unbound variable in value position: {v}"),
                    });
                }
            };
            out.push((v.as_str().to_string(), tagged));
            Ok(())
        }
        ValueExpr::Arith { left, right, .. } => {
            bound_vars(left, env, out)?;
            bound_vars(right, env, out)
        }
        ValueExpr::Call { args, .. } => {
            for arg in args {
                bound_vars(arg, env, out)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

type Bindings = std::collections::HashMap<Var, EvalValue>;
type SumResult = Result<EvalValue, morpholog_core::EvalError>;

/// The bindings a reported value is evaluated over, from the probe's
/// payload, and each sum's result in walk order.
pub(super) fn decode(payload: &serde_json::Value) -> Option<(Bindings, Vec<SumResult>)> {
    let vars = payload.get("vars")?.as_object()?;
    let mut bindings = std::collections::HashMap::new();
    for (name, value) in vars {
        bindings.insert(
            Var::from(name.as_str()),
            serde_json::from_value(value.clone()).ok()?,
        );
    }
    let mut sums = Vec::new();
    for sum in payload.get("sums")?.as_array()? {
        let result = if sum.get("kind")?.as_bool()? {
            let value: EvalValue = serde_json::from_value(sum.get("value")?.clone()).ok()?;
            let first = sum.get("first")?.as_bool()?;
            Err(morpholog_core::EvalError::sum_cannot_take(first, &value))
        } else if sum.get("range")?.as_bool()? {
            Err(morpholog_core::EvalError::sum_out_of_decimal_range())
        } else {
            Ok(EvalValue::Decimal(
                sum.get("total")?.as_str()?.parse().ok()?,
            ))
        };
        sums.push(result);
    }
    Some((bindings, sums))
}
