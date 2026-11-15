//! A derived claim rendered as the body of a live view: the kernel's
//! rows, computed by PostgreSQL from `morpholog.claims` at read time.
//!
//! The kernel's rows are the distinct key tuples of the domain's matches,
//! each value evaluated under that tuple. The rendering is `SELECT
//! DISTINCT ON` the keys' seek expressions over the domain's joins, each
//! value a computed expression with its sums as LATERAL items, the way
//! the compiled checks render a comparison's operands. Where a value's
//! probes say the kernel would raise on the row, the value is NULL and
//! `_morpholog_error` names the class of failure; everywhere else it is
//! NULL. What the renderer cannot express refuses by name, as an
//! invariant does.

use morpholog_core::{
    DerivedClaim, PredicateArgKind, PredicateDecl, PredicateName, Term, Value, ValueExpr,
};

use super::{
    Bound, CompileReason, Ctx, Env, ErrorReport, Pending, from_list, lift, nested_scope,
    quote_literal, render_prop, seek_sql, value_sql,
};
use crate::sql_views::column_sql_at;

/// One live view's body: the `SELECT` that computes the derived claim's
/// rows, and the metadata columns it leads with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LiveDerived {
    pub(crate) select: String,
    /// Every (predicate, position) the body seeks on at a join or a
    /// literal, for `provision indexes`: without them each row's lateral
    /// scans its predicate.
    pub(crate) required: std::collections::BTreeSet<(PredicateName, usize)>,
}

/// The two metadata columns every live view leads with, in order, each
/// with its PostgreSQL type.
pub(crate) const LIVE_METADATA: [(&str, &str); 2] = [
    ("_morpholog_model_hash", "text"),
    ("_morpholog_error", "text"),
];

/// Render `derived` as a live view body over `morpholog.claims`. `head`
/// is the declared predicate the derived claim computes, whose kinds type
/// the key columns.
pub(crate) fn compile_derived(
    derived: &DerivedClaim,
    head: &PredicateDecl,
    decls: &std::collections::BTreeMap<&str, &PredicateDecl>,
    defs: &std::collections::BTreeMap<&str, &morpholog_core::Definition>,
    model_hash: &str,
) -> Result<LiveDerived, CompileReason> {
    let mut ctx = Ctx::fresh(decls, defs);
    ctx.binding_calls = true;
    let domain = nested_scope(render_prop(&derived.domain, Env::new(), &mut ctx)?)?;
    if domain.has_errors() {
        return Err(CompileReason::ComparisonShape {
            detail: "a derived domain that can raise",
        });
    }
    if domain.disjoins {
        return Err(CompileReason::DisjunctionShape {
            detail: "a derived domain ending in `or`",
        });
    }
    if domain.from.is_empty() {
        return Err(CompileReason::UnvalidatedShape {
            detail: "a derived domain binding no claims".to_string(),
        });
    }

    let mut seeks = Vec::with_capacity(derived.keys.len());
    let mut columns = vec![format!(
        "{}::text AS \"_morpholog_model_hash\"",
        quote_literal(model_hash)
    )];
    let mut key_columns = Vec::with_capacity(derived.keys.len());
    for (i, key) in derived.keys.iter().enumerate() {
        let Some(Bound::Col(col)) = domain.env.get(key) else {
            return Err(CompileReason::UnvalidatedShape {
                detail: format!("the domain binds no key {key}"),
            });
        };
        let kind = &head
            .args
            .get(i)
            .ok_or_else(|| CompileReason::UnvalidatedShape {
                detail: format!("head arity under the keys of {}", head.name),
            })?
            .kind;
        seeks.push(seek_sql(col));
        key_columns.push(format!(
            "{} AS {}",
            column_sql_at(kind, &format!("{}.", col.alias), col.position).expr,
            quote_ident(&head.args[i].name)
        ));
    }

    // Each value: its expression under the keys, its sums as LATERAL
    // items, and the conditions under which the kernel would raise on
    // the row, classed for the error column.
    let mut laterals: Vec<String> = Vec::new();
    let mut failures: Vec<(String, &'static str)> = Vec::new();
    let mut value_columns = Vec::with_capacity(derived.values.len());
    for (j, value) in derived.values.iter().enumerate() {
        let position = derived.keys.len() + j;
        let declared = head.args.get(position).map(|a| &a.kind).ok_or_else(|| {
            CompileReason::UnvalidatedShape {
                detail: format!("head arity under the values of {}", head.name),
            }
        })?;
        let rendered = live_value(&value.expr, declared, &domain.env, &mut ctx, &mut laterals)?;
        let computed = if rendered.failures.is_empty() {
            rendered.sql
        } else {
            let own: Vec<&str> = rendered.failures.iter().map(|(c, _)| c.as_str()).collect();
            format!(
                "CASE WHEN {} THEN NULL ELSE {} END",
                own.join(" OR "),
                rendered.sql
            )
        };
        failures.extend(rendered.failures);
        value_columns.push(format!(
            "{computed} AS {}",
            quote_ident(&head.args[position].name)
        ));
    }
    // Every value closes what it queued; anything left would name a
    // LATERAL the view does not have.
    if !ctx.pending.is_empty() {
        return Err(CompileReason::UnvalidatedShape {
            detail: "a derived domain leaving a value unclosed".to_string(),
        });
    }

    let error = if failures.is_empty() {
        "NULL::text".to_string()
    } else {
        let arms: Vec<String> = failures
            .iter()
            .map(|(condition, class)| format!("WHEN {condition} THEN {}", quote_literal(class)))
            .collect();
        format!("CASE {} END", arms.join(" "))
    };
    columns.push(format!("{error} AS \"_morpholog_error\""));
    columns.extend(key_columns);
    columns.extend(value_columns);

    let mut from = vec![from_list(&domain)];
    from.extend(laterals);
    let select = format!(
        "SELECT DISTINCT ON ({seeks})\n    {columns}\nFROM {from}\nWHERE {where_}\nORDER BY {seeks}",
        seeks = seeks.join(", "),
        columns = columns.join(",\n    "),
        from = from.join(", "),
        where_ = domain.conjunction(),
    );
    // A synthetic call table's columns are no predicate's positions.
    let required = ctx
        .required
        .into_iter()
        .filter(|(predicate, _)| !predicate.as_str().starts_with(super::CALL_TABLE_PREFIX))
        .collect();
    Ok(LiveDerived { select, required })
}

/// One value of a live row: its SQL, of the column's type, and the
/// conditions under which the kernel would raise on the row, each with
/// its class, in the kernel's order.
struct LiveValue {
    sql: String,
    failures: Vec<(String, &'static str)>,
}

/// A value of kind `kind` under the row's bindings. A decimal is any
/// value the compiled fragment computes. A subject or a flag is a
/// literal, a key, or an `if(...)` choosing between them. `if(...)` is a
/// `CASE`: the kernel evaluates the test, then the chosen branch alone, so
/// the test's ways of raising always count and each branch's only where
/// that branch is chosen.
fn live_value(
    expr: &ValueExpr,
    kind: &PredicateArgKind,
    env: &Env,
    ctx: &mut Ctx<'_>,
    laterals: &mut Vec<String>,
) -> Result<LiveValue, CompileReason> {
    if let ValueExpr::Cond {
        when,
        then,
        otherwise,
    } = expr
    {
        let queued = ctx.pending.len();
        let test = render_prop(when, env.clone(), ctx)?;
        if ctx.pending.len() != queued {
            return Err(CompileReason::UnvalidatedShape {
                detail: "an `if` test leaving a value unclosed".to_string(),
            });
        }
        if test.disjoins {
            return Err(CompileReason::DisjunctionShape {
                detail: "an `if` test ending in `or`",
            });
        }
        // A test that binds no claim, a comparison over sums, reads its
        // sums from the row itself, where its probes read them too.
        let lifted = lift(&test.probes);
        let holds = if test.from.is_empty() {
            for lateral in test.laterals.iter().chain(&lifted.laterals) {
                if !laterals.contains(lateral) {
                    laterals.push(lateral.clone());
                }
            }
            format!("({})", test.conjunction())
        } else if lifted.laterals.is_empty() {
            test.exists_sql()
        } else {
            return Err(CompileReason::UnvalidatedShape {
                detail: "an `if` test raising before it binds a claim".to_string(),
            });
        };
        let mut failures: Vec<(String, &'static str)> = lifted
            .probes
            .into_iter()
            .map(|probe| {
                let class = class_of(&probe.report);
                (probe.condition, class)
            })
            .collect();
        let chosen = live_value(then, kind, env, ctx, laterals)?;
        let other = live_value(otherwise, kind, env, ctx, laterals)?;
        for (condition, class) in chosen.failures {
            failures.push((format!("({holds} AND {condition})"), class));
        }
        for (condition, class) in other.failures {
            failures.push((format!("(NOT {holds} AND {condition})"), class));
        }
        return Ok(LiveValue {
            sql: format!(
                "CASE WHEN {holds} THEN {} ELSE {} END",
                chosen.sql, other.sql
            ),
            failures,
        });
    }
    if *kind == PredicateArgKind::Decimal {
        return decimal_value(expr, env, ctx, laterals);
    }
    let refused = || CompileReason::ArgumentKind { kind: kind.clone() };
    if *kind != PredicateArgKind::Subject && *kind != PredicateArgKind::Bool {
        return Err(refused());
    }
    let sql = if let ValueExpr::Term(Term::Literal(Value::Subject(s))) = expr
        && *kind == PredicateArgKind::Subject
    {
        format!("{}::text", quote_literal(s.as_str()))
    } else if let ValueExpr::Term(Term::Literal(Value::Bool(b))) = expr
        && *kind == PredicateArgKind::Bool
    {
        b.to_string()
    } else if let ValueExpr::Term(Term::Var(v)) = expr
        && let Some(Bound::Col(col)) = env.get(v)
        && col.kind == *kind
    {
        column_sql_at(kind, &format!("{}.", col.alias), col.position).expr
    } else {
        return Err(refused());
    };
    Ok(LiveValue {
        sql,
        failures: Vec::new(),
    })
}

/// A decimal value the compiled fragment computes, its sums as LATERAL
/// items and its ways of raising as conditions on the row.
fn decimal_value(
    expr: &ValueExpr,
    env: &Env,
    ctx: &mut Ctx<'_>,
    laterals: &mut Vec<String>,
) -> Result<LiveValue, CompileReason> {
    let start = ctx.pending.len();
    let operand = value_sql(expr, env, ctx)?;
    let Some((amount, _unit)) = operand.dd else {
        return Err(CompileReason::ComparisonShape {
            detail: "a derived value that is not a decimal amount",
        });
    };
    let mut failures = Vec::new();
    for pending in ctx.pending.drain(start..).collect::<Vec<_>>() {
        match pending {
            Pending::Sum(sum) => {
                for probe in sum.body {
                    failures.push((probe.condition, class_of(&probe.report)));
                }
                if sum.can_meet_foreign {
                    failures.push((sum.kind_error, "sum_kind"));
                }
                failures.push((sum.range_error, "sum_range"));
                laterals.push(sum.lateral);
            }
            Pending::Covered(lateral) => laterals.push(lateral),
            Pending::Value(probe) => {
                failures.push((probe.condition, class_of(&probe.report)));
            }
        }
    }
    Ok(LiveValue {
        sql: format!("({amount})::numeric"),
        failures,
    })
}

fn quote_ident(name: &str) -> String {
    crate::sql_quote::quote_ident(name)
}

/// The class a live row names for a way of raising.
fn class_of(report: &ErrorReport) -> &'static str {
    match report {
        ErrorReport::SumRange => "sum_range",
        ErrorReport::SumKind => "sum_kind",
        ErrorReport::Compare { .. } => "compare",
        ErrorReport::Value { .. } => "value",
        ErrorReport::Lifted { .. } => "scope",
    }
}
