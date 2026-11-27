//! A sum over a set, asked once per row of that set's order: "the total
//! of everything before this one", and its yes-or-no form, "is anything
//! before this one".
//!
//! `sum(t | S and before(row, probe))` evaluated for many probes over the
//! same `S` rereads and resums `S` for every probe. When the order is one
//! the evaluator can sort by, the rows of `S` for one group of bound
//! values are sorted once, with running totals, and each probe is a binary
//! search. `exists x: S and before(row, probe)` is the same question
//! asked as a count, so it is answered from the same group: whether any
//! row comes before the probe. The answer is the direct path's, error for
//! error: the shape is taken only where no comparison the direct path
//! makes can raise, and anything else is left to it.
//!
//! Two orders are recognised: one comparison between a row's key and the
//! probe, and the guarded lexicographic order a book's walk uses, a call
//! to a condition of exactly this form (any parameter names, any branch
//! order):
//!
//! ```text
//! define before(side, p, s, p2, s2):
//!     (side = #buy and p < p2) or (side = #sell and p > p2) or (p = p2 and s < s2)
//! ```

use std::collections::HashSet;

use rust_decimal::Decimal;

use crate::definitions::DefinitionTable;
use crate::fold::{Node, walk_prop};
use crate::ir::{CompareOp, OrderedDomain, Prop, Subject, SumSeed, Term, Value, ValueExpr, Var};
use crate::state::{Bindings, EvalValue};

use super::{BigSum, EvalContext, EvalError, eval_value, find_matches};

/// A recognised `sum(target | source and order)`.
pub(super) struct Shape {
    /// The conjuncts before the order condition, matched as the direct
    /// path matches them.
    source: Prop,
    target: ValueExpr,
    /// The variables the source binds that the sum reads per row: the
    /// keys, and the target when it is a variable. Unbound on entry, or
    /// the shape does not apply there.
    row_vars: Vec<Var>,
    /// The probe's variables, bound on entry and read only by the order.
    pub(super) probe_vars: Vec<Var>,
    order: Order,
}

enum Order {
    /// `row op probe`, the row's side written on the left.
    Single { row: Var, probe: Var, op: CompareOp },
    /// By the first key in the direction the guard names, then by the
    /// second ascending, strictly. `row_first` when the row is the
    /// condition's first pair, so the sum counts the rows before the
    /// probe; otherwise the rows after it.
    Guarded {
        guard: Var,
        ascending: Subject,
        descending: Subject,
        row: [Var; 2],
        probe: [Var; 2],
        row_first: bool,
    },
}

/// One group's rows, sorted by their keys as the order compares them,
/// with the running totals of the target.
pub(super) enum Group {
    Sorted {
        /// Each row's keys, negated where the order runs downward, so
        /// the rows the sum counts for a probe are a prefix.
        keys: Vec<[Decimal; 2]>,
        /// `totals[i]` sums the first `i + 1` rows.
        totals: Vec<BigSum>,
    },
    /// A row the fast path cannot vouch for: a key or target that is not
    /// a decimal, or a source that raised. The direct path answers.
    Direct,
}

/// The shape of `sum`, or `None` when it is not one recognised here.
pub(super) fn recognise(sum: &ValueExpr, definitions: DefinitionTable<'_>) -> Option<Shape> {
    let ValueExpr::Sum { value, body, seed } = sum else {
        return None;
    };
    if *seed != SumSeed::Decimal {
        return None;
    }
    let conjuncts = flatten_and(body);
    let (order, source) = conjuncts.split_last()?;
    if source.is_empty() {
        return None;
    }
    let source = if source.len() == 1 {
        (*source[0]).clone()
    } else {
        Prop::And(source.iter().map(|p| (*p).clone()).collect())
    };
    let bound_by_source = mentioned(&source);
    let target_var = if let ValueExpr::Term(Term::Var(t)) = &**value
        && bound_by_source.contains(t)
    {
        Some(t.clone())
    } else if let ValueExpr::Term(Term::Literal(Value::Decimal(_))) = &**value {
        None
    } else {
        return None;
    };
    let (order, keys, probe_vars) = recognise_order(order, &bound_by_source, definitions)?;
    let mut row_vars = keys;
    row_vars.extend(target_var);
    Some(Shape {
        source,
        target: (**value).clone(),
        row_vars,
        probe_vars,
        order,
    })
}

/// `exists x: body` as the count `sum(1 | body)`, whose shape and groups
/// it shares: it holds exactly when the count is not zero. An `exists`
/// keeps no binding, so the count's rows are its witnesses.
pub(super) fn exists_as_count(exists: &Prop) -> Option<ValueExpr> {
    let Prop::Exists { body, .. } = exists else {
        return None;
    };
    Some(ValueExpr::Sum {
        value: Box::new(ValueExpr::Term(Term::Literal(Value::Decimal(
            crate::ir::LiteralDecimal::from(Decimal::ONE),
        )))),
        body: body.clone(),
        seed: SumSeed::Decimal,
    })
}

/// Whether `exists` could be one recognised here, read without
/// allocating: a conjunction ending in a decimal comparison or a call.
/// Most `exists` are not, and this keeps them off the memo's hashed
/// lookup.
pub(super) fn might_be_ordered(exists: &Prop) -> bool {
    let Prop::Exists { body, .. } = exists else {
        return false;
    };
    let Prop::And(parts) = body.as_ref() else {
        return false;
    };
    matches!(
        parts.last(),
        Some(
            Prop::Compare {
                domain: OrderedDomain::Decimal,
                ..
            } | Prop::Defined { .. }
        )
    )
}

fn flatten_and(p: &Prop) -> Vec<&Prop> {
    if let Prop::And(ps) = p {
        ps.iter().flat_map(flatten_and).collect()
    } else {
        vec![p]
    }
}

fn flatten_or(p: &Prop) -> Vec<&Prop> {
    if let Prop::Or(ps) = p {
        ps.iter().flat_map(flatten_or).collect()
    } else {
        vec![p]
    }
}

/// Every variable the condition names, binders included.
fn mentioned(p: &Prop) -> HashSet<Var> {
    let mut out = HashSet::new();
    walk_prop(p, &mut |n| match n {
        Node::Slot(Term::Var(v)) | Node::Value(ValueExpr::Term(Term::Var(v))) | Node::Binder(v) => {
            out.insert(v.clone());
        }
        Node::Stmt(_) | Node::Prop(_) | Node::Value(_) | Node::Slot(_) => {}
    });
    out
}

fn var_of(e: &ValueExpr) -> Option<&Var> {
    if let ValueExpr::Term(Term::Var(v)) = e {
        Some(v)
    } else {
        None
    }
}

fn flip(op: CompareOp) -> CompareOp {
    match op {
        CompareOp::Lt => CompareOp::Gt,
        CompareOp::Le => CompareOp::Ge,
        CompareOp::Gt => CompareOp::Lt,
        CompareOp::Ge => CompareOp::Le,
    }
}

/// The order, the row variables it reads, and the probe's.
fn recognise_order(
    order: &Prop,
    bound_by_source: &HashSet<Var>,
    definitions: DefinitionTable<'_>,
) -> Option<(Order, Vec<Var>, Vec<Var>)> {
    let in_source = |v: &Var| bound_by_source.contains(v);
    match order {
        Prop::Compare {
            op,
            domain: OrderedDomain::Decimal,
            left,
            right,
        } => {
            let (l, r) = (var_of(left)?, var_of(right)?);
            let (row, probe, op) = match (in_source(l), in_source(r)) {
                (true, false) => (l, r, *op),
                (false, true) => (r, l, flip(*op)),
                _ => return None,
            };
            Some((
                Order::Single {
                    row: row.clone(),
                    probe: probe.clone(),
                    op,
                },
                vec![row.clone()],
                vec![probe.clone()],
            ))
        }
        Prop::Defined { name, args } => {
            let def = definitions.get(name)?;
            if crate::recursion::asks_itself(def) {
                return None;
            }
            let body = def.body.as_prop()?;
            let form = guarded_form(body)?;
            // Exactly the five-parameter form: each parameter is one of
            // its roles, and the call passes five.
            let roles: HashSet<&Var> = [
                &form.guard,
                &form.first[0],
                &form.first[1],
                &form.second[0],
                &form.second[1],
            ]
            .into_iter()
            .collect();
            let params: HashSet<&Var> = def.parameters.iter().collect();
            if def.parameters.len() != 5 || args.len() != 5 || roles != params {
                return None;
            }
            let arg = |param: &Var| -> Option<Var> {
                let i = def.parameters.iter().position(|p| p == param)?;
                if let Term::Var(v) = args.get(i)? {
                    Some(v.clone())
                } else {
                    None
                }
            };
            let guard = arg(&form.guard)?;
            let first = [arg(&form.first[0])?, arg(&form.first[1])?];
            let second = [arg(&form.second[0])?, arg(&form.second[1])?];
            let mut all = vec![guard.clone()];
            all.extend(first.iter().cloned());
            all.extend(second.iter().cloned());
            let distinct: HashSet<&Var> = all.iter().collect();
            if distinct.len() != 5 {
                return None;
            }
            let row_first = match (
                first.iter().all(in_source),
                second.iter().any(in_source),
                second.iter().all(in_source),
                first.iter().any(in_source),
            ) {
                (true, false, _, _) => true,
                (_, _, true, false) => false,
                _ => return None,
            };
            let (row, probe) = if row_first {
                (first, second)
            } else {
                (second, first)
            };
            Some((
                Order::Guarded {
                    guard,
                    ascending: form.ascending,
                    descending: form.descending,
                    row: row.clone(),
                    probe: probe.clone(),
                    row_first,
                },
                row.to_vec(),
                probe.to_vec(),
            ))
        }
        Prop::Compare { .. }
        | Prop::Claim { .. }
        | Prop::Implies { .. }
        | Prop::Exists { .. }
        | Prop::And(_)
        | Prop::Or(_)
        | Prop::Pre(_)
        | Prop::Not(_)
        | Prop::Xor(..)
        | Prop::Eq(..)
        | Prop::Neq(..)
        | Prop::Forall { .. }
        | Prop::In(..) => None,
    }
}

/// The guarded lexicographic form, in the condition's own parameters:
/// `(g = A and a < c) or (g = D and a > c) or (a = c and b < d)`, with
/// `first` = (a, b) and `second` = (c, d).
struct Form {
    guard: Var,
    ascending: Subject,
    descending: Subject,
    first: [Var; 2],
    second: [Var; 2],
}

fn guarded_form(body: &Prop) -> Option<Form> {
    let branches = flatten_or(body);
    if branches.len() != 3 {
        return None;
    }
    let mut guarded: Vec<(Var, Subject, CompareOp, Var, Var)> = Vec::new();
    let mut tie: Option<([Var; 2], [Var; 2])> = None;
    for branch in branches {
        let parts = flatten_and(branch);
        let [left, right] = parts.as_slice() else {
            return None;
        };
        match (left, right) {
            (
                Prop::Eq(x, y),
                Prop::Compare {
                    op,
                    domain: OrderedDomain::Decimal,
                    left: a,
                    right: c,
                },
            ) => {
                let (a, c) = (var_of(a)?.clone(), var_of(c)?.clone());
                if let Some((g, lit)) = guard_test(x, y) {
                    if !matches!(op, CompareOp::Lt | CompareOp::Gt) {
                        return None;
                    }
                    guarded.push((g, lit, *op, a, c));
                } else {
                    let (x, y) = (var_of(x)?.clone(), var_of(y)?.clone());
                    if *op != CompareOp::Lt || tie.is_some() {
                        return None;
                    }
                    tie = Some(([x, a], [y, c]));
                }
            }
            _ => return None,
        }
    }
    let [up, down] = guarded.as_slice() else {
        return None;
    };
    let (up, down) = match (up.2, down.2) {
        (CompareOp::Lt, CompareOp::Gt) => (up, down),
        (CompareOp::Gt, CompareOp::Lt) => (down, up),
        _ => return None,
    };
    if up.0 != down.0 || up.1 == down.1 || up.3 != down.3 || up.4 != down.4 {
        return None;
    }
    let ([x, b], [y, d]) = tie?;
    // The tie branch compares the first keys for equality, in either
    // order, and the second keys strictly.
    let same_pair = (x == up.3 && y == up.4) || (x == up.4 && y == up.3);
    if !same_pair {
        return None;
    }
    Some(Form {
        guard: up.0.clone(),
        ascending: up.1.clone(),
        descending: down.1.clone(),
        first: [up.3.clone(), b],
        second: [up.4.clone(), d],
    })
}

/// `g = #literal`, either way round.
fn guard_test(x: &ValueExpr, y: &ValueExpr) -> Option<(Var, Subject)> {
    match (x, y) {
        (ValueExpr::Term(Term::Var(g)), ValueExpr::Term(Term::Literal(Value::Subject(s))))
        | (ValueExpr::Term(Term::Literal(Value::Subject(s))), ValueExpr::Term(Term::Var(g))) => {
            Some((g.clone(), s.clone()))
        }
        _ => None,
    }
}

/// How the keys run for this entry: per key, whether it is negated, and
/// whether a row equal to the probe counts. `None` when the guard names
/// neither direction, which leaves only the tie branch: not this order.
fn direction(order: &Order, bindings: &Bindings) -> Option<([bool; 2], bool)> {
    match order {
        Order::Single { op, .. } => Some(match op {
            CompareOp::Lt => ([false, false], false),
            CompareOp::Le => ([false, false], true),
            CompareOp::Gt => ([true, false], false),
            CompareOp::Ge => ([true, false], true),
        }),
        Order::Guarded {
            guard,
            ascending,
            descending,
            row_first,
            ..
        } => {
            let EvalValue::Subject(g) = &**bindings.get(guard)? else {
                return None;
            };
            let up = if g == ascending {
                true
            } else if g == descending {
                false
            } else {
                return None;
            };
            // Rows before the probe: the first key in the guard's
            // direction, the second ascending. Rows after it: both
            // reversed.
            Some(if *row_first {
                ([!up, false], false)
            } else {
                ([up, true], false)
            })
        }
    }
}

fn keys_of(order: &Order, row: bool) -> Vec<&Var> {
    match order {
        Order::Single { row: r, probe, .. } => vec![if row { r } else { probe }],
        Order::Guarded { row: r, probe, .. } => {
            let pair = if row { r } else { probe };
            pair.iter().collect()
        }
    }
}

fn decimal_keys(vars: &[&Var], bindings: &Bindings, negate: [bool; 2]) -> Option<[Decimal; 2]> {
    let mut out = [Decimal::ZERO; 2];
    for (i, v) in vars.iter().enumerate() {
        let EvalValue::Decimal(d) = &**bindings.get(*v)? else {
            return None;
        };
        out[i] = if negate[i] { -*d } else { *d };
    }
    Some(out)
}

/// Where `program` sums over a set in an order, or asks whether anything
/// in it comes first, that the evaluator answers from one sorted group
/// per evaluation: the definition, invariant, transformation or derived
/// claim each is written in, and the order, `one comparison` or `a
/// guarded order`, prefixed `exists, ` for an `exists`. Recognised from
/// the text; a call whose bindings differ from the ones assumed still
/// takes the direct path.
pub fn ordered_sums(program: &crate::ir::Program) -> Vec<(String, &'static str)> {
    use crate::fold::{walk_stmt, walk_value};
    use crate::ir::DefinitionBody;
    let definitions = DefinitionTable::new(&program.definitions);
    let mut out: Vec<(String, &'static str)> = Vec::new();
    let mut note = |place: String, node: Node<'_>| {
        let (shape, exists) = match node {
            Node::Value(sum @ ValueExpr::Sum { .. }) => (recognise(sum, definitions), false),
            Node::Prop(p @ Prop::Exists { .. }) => (
                exists_as_count(p).and_then(|count| recognise(&count, definitions)),
                true,
            ),
            Node::Stmt(_) | Node::Prop(_) | Node::Value(_) | Node::Slot(_) | Node::Binder(_) => {
                return;
            }
        };
        if let Some(shape) = shape {
            let kind = match (&shape.order, exists) {
                (Order::Single { .. }, false) => "one comparison",
                (Order::Guarded { .. }, false) => "a guarded order",
                (Order::Single { .. }, true) => "exists, one comparison",
                (Order::Guarded { .. }, true) => "exists, a guarded order",
            };
            if !out.iter().any(|(p, k)| *p == place && *k == kind) {
                out.push((place, kind));
            }
        }
    };
    for d in &program.definitions {
        let place = format!("definition {}", d.name);
        match &d.body {
            DefinitionBody::Prop(p) => walk_prop(p, &mut |n| note(place.clone(), n)),
            DefinitionBody::Value(v) => walk_value(v, &mut |n| note(place.clone(), n)),
        }
    }
    for i in &program.invariants {
        let place = format!("invariant {}", i.name);
        walk_prop(&i.body, &mut |n| note(place.clone(), n));
    }
    for t in &program.transformations {
        let place = format!("transformation {}", t.name);
        for stmt in &t.body {
            walk_stmt(stmt, &mut |n| note(place.clone(), n));
        }
    }
    for d in &program.derived_claims {
        let place = format!("derived {}", d.predicate);
        walk_prop(&d.domain, &mut |n| note(place.clone(), n));
        for v in &d.values {
            walk_value(&v.expr, &mut |n| note(place.clone(), n));
        }
    }
    out
}

impl Shape {
    /// Whether the bindings at this entry are the ones the shape was
    /// recognised for: every probe variable bound, every row variable
    /// left for the source to bind.
    pub(super) fn applies(&self, bindings: &Bindings) -> bool {
        self.probe_vars.iter().all(|v| bindings.get(v).is_some())
            && self.row_vars.iter().all(|v| bindings.get(v).is_none())
            && direction(&self.order, bindings).is_some()
    }

    /// The group's rows, sorted, for the bindings at this entry.
    pub(super) fn build(&self, ctx: &EvalContext<'_>) -> Group {
        let Some((negate, _)) = direction(&self.order, ctx.bindings) else {
            return Group::Direct;
        };
        let Ok(rows) = find_matches(&self.source, ctx) else {
            return Group::Direct;
        };
        let key_vars = keys_of(&self.order, true);
        let mut keyed: Vec<([Decimal; 2], Decimal)> = Vec::with_capacity(rows.len());
        for row in &rows {
            let Some(keys) = decimal_keys(&key_vars, row, negate) else {
                return Group::Direct;
            };
            let Ok(target) = eval_value(&self.target, &ctx.with_bindings(row)) else {
                return Group::Direct;
            };
            let EvalValue::Decimal(t) = target else {
                return Group::Direct;
            };
            keyed.push((keys, t));
        }
        keyed.sort_by_key(|row| row.0);
        let mut totals: Vec<BigSum> = Vec::with_capacity(keyed.len());
        for (_, t) in &keyed {
            let next = match totals.last() {
                Some(prev) => {
                    let mut next = prev.clone();
                    next.add(*t);
                    next
                }
                None => BigSum::new(*t),
            };
            totals.push(next);
        }
        Group::Sorted {
            keys: keyed.into_iter().map(|(k, _)| k).collect(),
            totals,
        }
    }

    /// The sum for this entry's probe, or `None` for the direct path.
    pub(super) fn answer(
        &self,
        group: &Group,
        bindings: &Bindings,
    ) -> Option<Result<EvalValue, EvalError>> {
        let Group::Sorted { totals, .. } = group else {
            return None;
        };
        Some(match self.counted(group, bindings)? {
            0 => Ok(EvalValue::Decimal(Decimal::ZERO)),
            n => totals[n - 1]
                .clone()
                .into_decimal()
                .map(EvalValue::Decimal)
                .ok_or_else(EvalError::sum_out_of_decimal_range),
        })
    }

    /// Whether any row comes before this entry's probe, or `None` for
    /// the direct path.
    pub(super) fn any_before(&self, group: &Group, bindings: &Bindings) -> Option<bool> {
        Some(self.counted(group, bindings)? > 0)
    }

    /// How many of the group's rows the order counts for this entry's
    /// probe, or `None` for the direct path.
    fn counted(&self, group: &Group, bindings: &Bindings) -> Option<usize> {
        let Group::Sorted { keys, .. } = group else {
            return None;
        };
        // Against no rows the direct path compares nothing, so the
        // probe is not looked at.
        if keys.is_empty() {
            return Some(0);
        }
        let (negate, inclusive) = direction(&self.order, bindings)?;
        let probe = decimal_keys(&keys_of(&self.order, false), bindings, negate)?;
        let single = matches!(self.order, Order::Single { .. });
        let counted = |k: &[Decimal; 2]| {
            if single {
                if inclusive {
                    k[0] <= probe[0]
                } else {
                    k[0] < probe[0]
                }
            } else {
                k < &probe
            }
        };
        Some(keys.partition_point(counted))
    }
}

#[cfg(test)]
mod tests {
    use crate::definitions::DefinitionTable;
    use crate::eval::memo::Memo;
    use crate::eval::{EvalContext, eval_value, find_matches};
    use crate::ir::{CompareOp, Definition, OrderedDomain, Prop, ValueExpr};
    use crate::ir_builder::{
        and, claim, defined, definition, eq, exists, not, or, params, subj, sum, term, var,
    };
    use crate::state::{Bindings, Bound, ClaimInstance, EvalValue, State};

    fn decimal(text: &str) -> EvalValue {
        EvalValue::Decimal(rust_decimal::Decimal::from_str_exact(text).unwrap())
    }

    fn subject(text: &str) -> EvalValue {
        EvalValue::Subject(text.into())
    }

    fn row(predicate: &str, args: Vec<EvalValue>) -> ClaimInstance {
        ClaimInstance {
            predicate: predicate.into(),
            args,
        }
    }

    fn cmp(op: CompareOp, left: &str, right: &str) -> Prop {
        Prop::Compare {
            op,
            domain: OrderedDomain::Decimal,
            left: Box::new(term(var(left))),
            right: Box::new(term(var(right))),
        }
    }

    fn bind(pairs: &[(&str, EvalValue)]) -> Bindings {
        pairs
            .iter()
            .map(|(k, v)| (crate::ir::Var::from(*k), Bound::from(v.clone())))
            .collect()
    }

    /// The sum's answer, as text so a decimal's scale counts, through
    /// `memo` or through the direct path.
    fn answer(
        figure: &ValueExpr,
        state: &State,
        definitions: &[Definition],
        bindings: &Bindings,
        memo: Option<&Memo>,
    ) -> String {
        let ctx = EvalContext::new(
            state,
            None,
            bindings,
            None,
            DefinitionTable::new(definitions),
        );
        let ctx = match memo {
            Some(memo) => ctx.with_memo(memo),
            None => ctx,
        };
        format!("{:?}", eval_value(figure, &ctx))
    }

    /// Lots in two groups, with equal keys, mixed scales and a lot
    /// matched twice (two claims, same key and quantity).
    fn lots() -> State {
        let mut claims = Vec::new();
        for (group, key, q, tag) in [
            ("c1", "1", "10", "a"),
            ("c1", "2", "5.50", "b"),
            ("c1", "2", "5.5", "c"),
            ("c1", "3.0", "1", "d"),
            ("c1", "3", "1", "e"),
            ("c1", "7", "100", "f"),
            ("c2", "2", "4", "g"),
            ("c2", "9", "8", "h"),
        ] {
            claims.push(row(
                "Lot",
                vec![subject(group), decimal(key), decimal(q), subject(tag)],
            ));
        }
        State::from_claims(claims)
    }

    /// Every comparison, the probe on either side: the same answer for
    /// every probe, and one sorted group per group of bound values.
    #[test]
    fn one_comparison_answers_as_the_direct_path_with_one_group_each() {
        let state = lots();
        let probes = ["0", "1", "2", "2.00", "2.5", "3", "7", "8"];
        for op in [CompareOp::Lt, CompareOp::Le, CompareOp::Gt, CompareOp::Ge] {
            for probe_left in [false, true] {
                let order = if probe_left {
                    cmp(op, "k", "k2")
                } else {
                    cmp(op, "k2", "k")
                };
                let figure = sum(
                    var("q"),
                    and(vec![
                        claim("Lot", vec![var("c"), var("k2"), var("q"), var("t")]),
                        order,
                    ]),
                );
                let memo = Memo::default();
                let mut asked = 0;
                for c in ["c1", "c2", "c3"] {
                    for k in probes {
                        let b = bind(&[("c", subject(c)), ("k", decimal(k))]);
                        let direct = answer(&figure, &state, &[], &b, None);
                        let fast = answer(&figure, &state, &[], &b, Some(&memo));
                        assert_eq!(fast, direct, "{op:?} probe_left={probe_left} c={c} k={k}");
                        asked += 1;
                    }
                }
                assert_eq!(
                    memo.prefix_work(),
                    (3, asked),
                    "{op:?} probe_left={probe_left}"
                );
            }
        }
    }

    fn before() -> Definition {
        definition(
            "before",
            params(&["side", "p", "s", "p2", "s2"]),
            or(vec![
                and(vec![
                    eq(term(var("side")), term(subj("buy"))),
                    cmp(CompareOp::Lt, "p", "p2"),
                ]),
                and(vec![
                    eq(term(var("side")), term(subj("sell"))),
                    cmp(CompareOp::Gt, "p", "p2"),
                ]),
                and(vec![
                    eq(term(var("p")), term(var("p2"))),
                    cmp(CompareOp::Lt, "s", "s2"),
                ]),
            ]),
        )
    }

    /// A book with ties in price and in sequence.
    fn book() -> State {
        let mut claims = Vec::new();
        for (i, (p, s, q)) in [
            ("100", "1", "5"),
            ("100", "2", "3"),
            ("99.5", "3", "2"),
            ("101", "4", "7"),
            ("100.0", "5", "1"),
            ("99.5", "6", "4"),
            ("101", "4", "2"),
        ]
        .into_iter()
        .enumerate()
        {
            claims.push(row(
                "Rest",
                vec![
                    subject(&format!("r{i}")),
                    decimal(p),
                    decimal(s),
                    decimal(q),
                ],
            ));
        }
        State::from_claims(claims)
    }

    /// The walk's order, in both directions and with the row on either
    /// side of the call: the same answer for every probe. A guard that
    /// names neither direction is the direct path's.
    #[test]
    fn the_guarded_order_answers_as_the_direct_path() {
        let state = book();
        let definitions = [before()];
        for row_first in [true, false] {
            let order = if row_first {
                defined(
                    "before",
                    vec![var("g"), var("p2"), var("s2"), var("p"), var("s")],
                )
            } else {
                defined(
                    "before",
                    vec![var("g"), var("p"), var("s"), var("p2"), var("s2")],
                )
            };
            let figure = sum(
                var("q2"),
                and(vec![
                    claim("Rest", vec![var("r2"), var("p2"), var("s2"), var("q2")]),
                    order,
                ]),
            );
            let memo = Memo::default();
            let mut asked = 0;
            for g in ["buy", "sell"] {
                for p in ["99", "99.5", "100", "100.00", "100.5", "101", "102"] {
                    for s in ["0", "1", "2", "4", "5", "9"] {
                        let b = bind(&[("g", subject(g)), ("p", decimal(p)), ("s", decimal(s))]);
                        let direct = answer(&figure, &state, &definitions, &b, None);
                        let fast = answer(&figure, &state, &definitions, &b, Some(&memo));
                        assert_eq!(fast, direct, "row_first={row_first} g={g} p={p} s={s}");
                        asked += 1;
                    }
                }
            }
            assert_eq!(memo.prefix_work(), (2, asked), "row_first={row_first}");

            let other = bind(&[
                ("g", subject("hold")),
                ("p", decimal("100")),
                ("s", decimal("3")),
            ]);
            let memo = Memo::default();
            assert_eq!(
                answer(&figure, &state, &definitions, &other, Some(&memo)),
                answer(&figure, &state, &definitions, &other, None),
            );
            assert_eq!(
                memo.prefix_work(),
                (0, 0),
                "an unguarded order is not this one"
            );
        }
    }

    /// What the direct path raises, the memo's path raises, and where the
    /// direct path compares nothing it raises nothing.
    #[test]
    fn errors_and_empty_groups_are_the_direct_paths() {
        let figure = sum(
            var("q"),
            and(vec![
                claim("Lot", vec![var("c"), var("k2"), var("q"), var("t")]),
                cmp(CompareOp::Lt, "k2", "k"),
            ]),
        );
        let max = "79228162514264337593543950335";
        let state = State::from_claims(vec![
            row(
                "Lot",
                vec![subject("big"), decimal("1"), decimal(max), subject("a")],
            ),
            row(
                "Lot",
                vec![subject("big"), decimal("2"), decimal(max), subject("b")],
            ),
            row(
                "Lot",
                vec![
                    subject("big"),
                    decimal("3"),
                    decimal(&format!("-{max}")),
                    subject("c"),
                ],
            ),
            row(
                "Lot",
                vec![subject("odd"), decimal("1"), decimal("1"), subject("a")],
            ),
            row(
                "Lot",
                vec![subject("odd"), subject("two"), decimal("1"), subject("b")],
            ),
        ]);
        let cases: Vec<(EvalValue, EvalValue)> = vec![
            // An intermediate total past the range, and the final one
            // back inside it.
            (subject("big"), decimal("2.5")),
            (subject("big"), decimal("4")),
            // A key that is not a decimal: the direct path's error.
            (subject("odd"), decimal("5")),
            // A probe that is not a decimal, over rows and over none.
            (subject("big"), subject("x")),
            (subject("none"), subject("x")),
        ];
        for (c, k) in cases {
            let b = bind(&[("c", c.clone()), ("k", k.clone())]);
            let memo = Memo::default();
            assert_eq!(
                answer(&figure, &state, &[], &b, Some(&memo)),
                answer(&figure, &state, &[], &b, None),
                "c={c:?} k={k:?}"
            );
        }
    }

    /// A source that raises raises the same error through the memo, for
    /// every probe, and the order's other shapes are not recognised.
    #[test]
    fn a_source_that_raises_is_the_direct_paths() {
        let state = State::from_claims(vec![
            row(
                "Lot",
                vec![subject("c1"), decimal("1"), decimal("1"), subject("a")],
            ),
            row(
                "Lot",
                vec![subject("c1"), decimal("2"), decimal("1"), subject("b")],
            ),
        ]);
        // `t < 5` compares a subject: the source itself raises.
        let figure = sum(
            var("q"),
            and(vec![
                claim("Lot", vec![var("c"), var("k2"), var("q"), var("t")]),
                Prop::Compare {
                    op: CompareOp::Lt,
                    domain: OrderedDomain::Decimal,
                    left: Box::new(term(var("t"))),
                    right: Box::new(term(crate::ir_builder::dec("5"))),
                },
                cmp(CompareOp::Lt, "k2", "k"),
            ]),
        );
        let memo = Memo::default();
        for k in ["0", "1.5", "9"] {
            let b = bind(&[("c", subject("c1")), ("k", decimal(k))]);
            let direct = answer(&figure, &state, &[], &b, None);
            assert!(direct.starts_with("Err("), "the source raises: {direct}");
            assert_eq!(
                answer(&figure, &state, &[], &b, Some(&memo)),
                direct,
                "k={k}"
            );
        }
        assert_eq!(
            memo.prefix_work(),
            (1, 0),
            "built once, answered by the direct path"
        );
    }

    /// A condition of the guarded form with a sixth parameter, or a
    /// parameter in two roles, is not the order recognised.
    #[test]
    fn only_the_five_parameter_guarded_form_is_recognised() {
        let state = book();
        let mut six = before();
        six.parameters.push(crate::ir::Var::from("extra"));
        let figure = |args: Vec<crate::ir::Term>| {
            sum(
                var("q2"),
                and(vec![
                    claim("Rest", vec![var("r2"), var("p2"), var("s2"), var("q2")]),
                    defined("before", args),
                ]),
            )
        };
        let six_args = figure(vec![
            var("g"),
            var("p2"),
            var("s2"),
            var("p"),
            var("s"),
            var("p"),
        ]);
        let b = bind(&[
            ("g", subject("buy")),
            ("p", decimal("100")),
            ("s", decimal("3")),
        ]);
        let memo = Memo::default();
        assert_eq!(
            answer(&six_args, &state, &[six.clone()], &b, Some(&memo)),
            answer(&six_args, &state, &[six], &b, None)
        );
        assert_eq!(memo.prefix_work(), (0, 0), "six parameters: not this order");
    }

    /// A row variable already bound at entry filters the source rather
    /// than ranging over it: the direct path answers.
    #[test]
    fn a_row_variable_bound_at_entry_takes_the_direct_path() {
        let state = lots();
        let figure = sum(
            var("q"),
            and(vec![
                claim("Lot", vec![var("c"), var("k2"), var("q"), var("t")]),
                cmp(CompareOp::Lt, "k2", "k"),
            ]),
        );
        let b = bind(&[
            ("c", subject("c1")),
            ("k", decimal("5")),
            ("k2", decimal("2")),
        ]);
        let memo = Memo::default();
        assert_eq!(
            answer(&figure, &state, &[], &b, Some(&memo)),
            answer(&figure, &state, &[], &b, None)
        );
        assert_eq!(memo.prefix_work(), (0, 0));
    }

    /// Whether `test` holds, as text so an error counts, through `memo`
    /// or through the direct path.
    fn holds(
        test: &Prop,
        state: &State,
        definitions: &[Definition],
        bindings: &Bindings,
        memo: Option<&Memo>,
    ) -> String {
        let ctx = EvalContext::new(
            state,
            None,
            bindings,
            None,
            DefinitionTable::new(definitions),
        );
        let ctx = match memo {
            Some(memo) => ctx.with_memo(memo),
            None => ctx,
        };
        format!("{:?}", find_matches(test, &ctx).map(|m| !m.is_empty()))
    }

    /// "Is any lot before this key", under every comparison and with the
    /// probe on either side, plain and negated: the direct path's answer
    /// for every probe, one sorted group per group of bound values.
    #[test]
    fn exists_in_one_comparison_answers_as_the_direct_path() {
        let state = lots();
        let probes = ["0", "1", "2", "2.00", "2.5", "3", "7", "8"];
        for op in [CompareOp::Lt, CompareOp::Le, CompareOp::Gt, CompareOp::Ge] {
            for probe_left in [false, true] {
                let order = if probe_left {
                    cmp(op, "k", "k2")
                } else {
                    cmp(op, "k2", "k")
                };
                let some = exists(
                    "t",
                    and(vec![
                        claim("Lot", vec![var("c"), var("k2"), var("q"), var("t")]),
                        order,
                    ]),
                );
                for test in [some.clone(), not(some)] {
                    let memo = Memo::default();
                    let mut asked = 0;
                    for c in ["c1", "c2", "c3"] {
                        for k in probes {
                            let b = bind(&[("c", subject(c)), ("k", decimal(k))]);
                            let direct = holds(&test, &state, &[], &b, None);
                            let fast = holds(&test, &state, &[], &b, Some(&memo));
                            assert_eq!(fast, direct, "{op:?} probe_left={probe_left} c={c} k={k}");
                            asked += 1;
                        }
                    }
                    assert_eq!(
                        memo.prefix_work(),
                        (3, asked),
                        "{op:?} probe_left={probe_left}"
                    );
                }
            }
        }
    }

    /// "Is any resting order ahead of this one" in the walk's order, both
    /// directions and the row on either side of the call. A guard that
    /// names neither direction is the direct path's.
    #[test]
    fn exists_in_the_guarded_order_answers_as_the_direct_path() {
        let state = book();
        let definitions = [before()];
        for row_first in [true, false] {
            let order = if row_first {
                defined(
                    "before",
                    vec![var("g"), var("p2"), var("s2"), var("p"), var("s")],
                )
            } else {
                defined(
                    "before",
                    vec![var("g"), var("p"), var("s"), var("p2"), var("s2")],
                )
            };
            let test = not(exists(
                "r2",
                and(vec![
                    claim("Rest", vec![var("r2"), var("p2"), var("s2"), var("q2")]),
                    order,
                ]),
            ));
            let memo = Memo::default();
            let mut asked = 0;
            for g in ["buy", "sell"] {
                for p in ["99", "99.5", "100", "100.00", "100.5", "101", "102"] {
                    for s in ["0", "1", "2", "4", "5", "9"] {
                        let b = bind(&[("g", subject(g)), ("p", decimal(p)), ("s", decimal(s))]);
                        let direct = holds(&test, &state, &definitions, &b, None);
                        let fast = holds(&test, &state, &definitions, &b, Some(&memo));
                        assert_eq!(fast, direct, "row_first={row_first} g={g} p={p} s={s}");
                        asked += 1;
                    }
                }
            }
            assert_eq!(memo.prefix_work(), (2, asked), "row_first={row_first}");

            let other = bind(&[
                ("g", subject("hold")),
                ("p", decimal("100")),
                ("s", decimal("3")),
            ]);
            let memo = Memo::default();
            assert_eq!(
                holds(&test, &state, &definitions, &other, Some(&memo)),
                holds(&test, &state, &definitions, &other, None),
            );
            assert_eq!(
                memo.prefix_work(),
                (0, 0),
                "an unguarded order is not this one"
            );
        }
    }

    /// A key or a probe that is not a decimal, a source that raises, and
    /// a row variable bound at entry: the direct path's answer, error
    /// included, and no probe answered from a group, except over no rows,
    /// where neither path looks at the probe.
    #[test]
    fn exists_off_the_fast_path_is_the_direct_paths() {
        let state = State::from_claims(vec![
            row(
                "Lot",
                vec![subject("odd"), decimal("1"), decimal("1"), subject("a")],
            ),
            row(
                "Lot",
                vec![subject("odd"), subject("two"), decimal("1"), subject("b")],
            ),
            row(
                "Lot",
                vec![subject("ok"), decimal("1"), decimal("1"), subject("a")],
            ),
        ]);
        let some = |extra: Option<Prop>| {
            let mut parts = vec![claim("Lot", vec![var("c"), var("k2"), var("q"), var("t")])];
            parts.extend(extra);
            parts.push(cmp(CompareOp::Lt, "k2", "k"));
            exists("t", and(parts))
        };
        // `t < 5` compares a subject: the source itself raises.
        let raising = Prop::Compare {
            op: CompareOp::Lt,
            domain: OrderedDomain::Decimal,
            left: Box::new(term(var("t"))),
            right: Box::new(term(crate::ir_builder::dec("5"))),
        };
        let cases: Vec<(Prop, Bindings, usize, &str)> = vec![
            (
                some(None),
                bind(&[("c", subject("odd")), ("k", decimal("5"))]),
                0,
                "a key that is not a decimal",
            ),
            (
                some(None),
                bind(&[("c", subject("ok")), ("k", subject("x"))]),
                0,
                "a probe that is not a decimal",
            ),
            (
                some(None),
                bind(&[("c", subject("none")), ("k", subject("x"))]),
                1,
                "a probe that is not a decimal, over no rows",
            ),
            (
                some(Some(raising)),
                bind(&[("c", subject("ok")), ("k", decimal("5"))]),
                0,
                "a source that raises",
            ),
            (
                some(None),
                bind(&[
                    ("c", subject("ok")),
                    ("k", decimal("5")),
                    ("k2", decimal("1")),
                ]),
                0,
                "a row variable bound at entry",
            ),
        ];
        for (test, b, from_group, why) in cases {
            let memo = Memo::default();
            assert_eq!(
                holds(&test, &state, &[], &b, Some(&memo)),
                holds(&test, &state, &[], &b, None),
                "{why}"
            );
            assert_eq!(memo.prefix_work().1, from_group, "{why}");
        }
    }

    /// One memo, a different `exists` at one address each time round: a
    /// node built on the stack in a loop reuses its slot, so the memo
    /// must not answer one from another's group.
    #[test]
    fn a_reused_address_is_not_taken_for_the_same_exists() {
        let state = lots();
        let memo = Memo::default();
        // At 1, `<` finds nothing and `<=` finds the lot keyed 1, so an
        // answer from another comparison's group shows.
        let b = bind(&[("c", subject("c1")), ("k", decimal("1"))]);
        for op in [
            CompareOp::Lt,
            CompareOp::Le,
            CompareOp::Gt,
            CompareOp::Ge,
            CompareOp::Lt,
        ] {
            let test = exists(
                "t",
                and(vec![
                    claim("Lot", vec![var("c"), var("k2"), var("q"), var("t")]),
                    cmp(op, "k2", "k"),
                ]),
            );
            assert_eq!(
                holds(&test, &state, &[], &b, Some(&memo)),
                holds(&test, &state, &[], &b, None),
                "{op:?}"
            );
        }
    }
}
