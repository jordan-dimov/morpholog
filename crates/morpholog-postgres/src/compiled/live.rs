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

use morpholog_core::{DerivedClaim, PredicateArgKind, PredicateDecl, PredicateName};

use super::{
    Bound, CompileReason, Ctx, Env, ErrorReport, Pending, from_list, nested_scope, quote_literal,
    render_prop, seek_sql, value_sql,
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
        if *declared != PredicateArgKind::Decimal {
            return Err(CompileReason::ArgumentKind {
                kind: declared.clone(),
            });
        }
        let operand = value_sql(&value.expr, &domain.env, &mut ctx)?;
        let Some((amount, _unit)) = operand.dd else {
            return Err(CompileReason::ComparisonShape {
                detail: "a derived value that is not a decimal amount",
            });
        };
        let mut own: Vec<String> = Vec::new();
        for pending in std::mem::take(&mut ctx.pending) {
            match pending {
                Pending::Sum(sum) => {
                    if sum.can_meet_foreign {
                        own.push(sum.kind_error.clone());
                        failures.push((sum.kind_error, "sum_kind"));
                    }
                    own.push(sum.range_error.clone());
                    failures.push((sum.range_error, "sum_range"));
                    laterals.push(sum.lateral);
                }
                Pending::Covered(lateral) => laterals.push(lateral),
                Pending::Value(probe) => {
                    let class = match probe.report {
                        ErrorReport::SumRange => "sum_range",
                        ErrorReport::SumKind => "sum_kind",
                        ErrorReport::Compare { .. } => "compare",
                        ErrorReport::Value { .. } => "value",
                        ErrorReport::Lifted { .. } => "scope",
                    };
                    own.push(probe.condition.clone());
                    failures.push((probe.condition, class));
                }
            }
        }
        let computed = if own.is_empty() {
            format!("({amount})::numeric")
        } else {
            format!(
                "CASE WHEN {} THEN NULL ELSE ({amount})::numeric END",
                own.join(" OR ")
            )
        };
        value_columns.push(format!(
            "{computed} AS {}",
            quote_ident(&head.args[position].name)
        ));
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

fn quote_ident(name: &str) -> String {
    crate::sql_quote::quote_ident(name)
}
