//! One evaluation's memo of the aggregates it has computed.
//!
//! A body `let` is substituted before the IR exists, so a figure named
//! once and used three times is three copies of one subtree. Evaluation
//! is pure, so within one evaluation a copy read under the same values,
//! over the same state, gives what the first gave. The memo keeps that
//! result: the rule means what substitution says, and computes each
//! figure once.
//!
//! Only `sum` and `max`/`min` are kept, the forms that scan state. The
//! key is the node's structure, the exact values of the variables it
//! mentions, and the state it reads. Exact means as written: `7` and
//! `7.0` are equal to the kernel and different keys here, so a result
//! never carries another binding's scale.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use crate::fold::{Node, walk_value};
use crate::ir::{Prop, Term, Unit, ValueExpr, Var};
use crate::state::EvalValue;

use super::prefix::{Group, Shape, exists_as_count, might_be_ordered, recognise};
use super::{EvalContext, EvalError};

#[cfg(feature = "memo-differential")]
static OFF: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// For the memo differential only, built with `memo-differential`:
/// evaluations started after `set_memo(false)` run without a memo until
/// `set_memo(true)`. Process-wide, so that test runs alone in its binary.
#[cfg(feature = "memo-differential")]
pub fn set_memo(on: bool) {
    OFF.store(!on, std::sync::atomic::Ordering::SeqCst);
}

#[cfg(feature = "memo-differential")]
pub(crate) fn memo_on() -> bool {
    !OFF.load(std::sync::atomic::Ordering::SeqCst)
}

#[cfg(not(feature = "memo-differential"))]
pub(crate) fn memo_on() -> bool {
    true
}

/// Created where one evaluation starts and dropped where it ends: a
/// transformation's body, an invariant's judgement, a derived claim's rows.
#[derive(Default)]
pub(crate) struct Memo {
    /// Keyed by structure, not address: a node built during evaluation
    /// (an `xor` lowered on the fly) can reuse a freed address.
    entries: RefCell<HashMap<ValueExpr, Entry>>,
    /// Each `exists` met, as the count it is answered through when that
    /// count is a sum over a set in an order.
    exists: RefCell<HashMap<Prop, Option<Rc<ValueExpr>>>>,
    computed: Cell<usize>,
    groups_built: Cell<usize>,
    probes_answered: Cell<usize>,
}

struct Entry {
    mentions: Rc<[Var]>,
    results: HashMap<Key, Result<EvalValue, EvalError>>,
    /// The node as a sum over a set in an order, when it is one.
    shape: Option<Rc<Shape>>,
    /// Its sorted groups, keyed as `results` is with the probe left out.
    groups: HashMap<Key, Rc<Group>>,
}

#[derive(PartialEq, Eq, Hash)]
struct Key {
    /// The state read, by address: the context's own, or the pre-state
    /// inside `pre(...)`.
    state: usize,
    transition: bool,
    values: Vec<Option<Exact>>,
}

/// A value's identity as written. `EvalValue`'s own `Eq` and `Hash`
/// follow numeric equality, so they are never used here for a decimal.
#[derive(PartialEq, Eq, Hash)]
enum Exact {
    Decimal([u8; 16]),
    Quantity([u8; 16], Unit),
    Collection(Vec<Exact>),
    /// A kind whose equality is already exact.
    Other(EvalValue),
}

fn exact(value: &EvalValue) -> Exact {
    match value {
        EvalValue::Decimal(d) => Exact::Decimal(d.serialize()),
        EvalValue::Quantity { amount, unit } => Exact::Quantity(amount.serialize(), unit.clone()),
        EvalValue::Collection(items) => Exact::Collection(items.iter().map(exact).collect()),
        other @ (EvalValue::Subject(_)
        | EvalValue::Bool(_)
        | EvalValue::Date(_)
        | EvalValue::Timestamp(_)
        | EvalValue::Duration(_)
        | EvalValue::CalendarSpan(_)) => Exact::Other(other.clone()),
    }
}

/// Every variable the node names, in its own order, once.
fn mentions(node: &ValueExpr) -> Rc<[Var]> {
    let mut out: Vec<Var> = Vec::new();
    walk_value(node, &mut |n| {
        let var = match n {
            Node::Slot(Term::Var(v)) | Node::Value(ValueExpr::Term(Term::Var(v))) => v,
            Node::Stmt(_) | Node::Prop(_) | Node::Value(_) | Node::Slot(_) | Node::Binder(_) => {
                return;
            }
        };
        if !out.contains(var) {
            out.push(var.clone());
        }
    });
    out.into()
}

impl Memo {
    /// `node`'s result under `ctx`, computed by `compute` only if this
    /// evaluation has not computed it already. No borrow is held while
    /// computing: a miss may evaluate another aggregate.
    pub(crate) fn get_or_compute(
        &self,
        node: &ValueExpr,
        ctx: &EvalContext<'_>,
        compute: impl FnOnce() -> Result<EvalValue, EvalError>,
    ) -> Result<EvalValue, EvalError> {
        let (mentions, shape) = self.entry(node, ctx);
        let key = key(&mentions, ctx, &[]);
        if let Some(found) = self
            .entries
            .borrow()
            .get(node)
            .and_then(|entry| entry.results.get(&key))
        {
            return found.clone();
        }
        if let Some(answer) = shape.and_then(|shape| {
            let group = self.group(node, &mentions, &shape, ctx)?;
            let answer = shape.answer(&group, ctx.bindings)?;
            self.probes_answered.set(self.probes_answered.get() + 1);
            Some(answer)
        }) {
            return answer;
        }
        let result = compute();
        self.computed.set(self.computed.get() + 1);
        if let Some(entry) = self.entries.borrow_mut().get_mut(node) {
            entry.results.insert(key, result.clone());
        }
        result
    }

    /// The node's mentions and shape, recorded on first sight.
    /// The node is cloned only the first time it is seen.
    fn entry(&self, node: &ValueExpr, ctx: &EvalContext<'_>) -> (Rc<[Var]>, Option<Rc<Shape>>) {
        if let Some(entry) = self.entries.borrow().get(node) {
            return (Rc::clone(&entry.mentions), entry.shape.clone());
        }
        let entry = Entry {
            mentions: mentions(node),
            results: HashMap::new(),
            shape: recognise(node, ctx.definitions).map(Rc::new),
            groups: HashMap::new(),
        };
        let found = (Rc::clone(&entry.mentions), entry.shape.clone());
        self.entries.borrow_mut().insert(node.clone(), entry);
        found
    }

    /// Whether `exists` holds, answered from the sorted group of the
    /// count it asks, or `None` for the direct path.
    pub(crate) fn exists(&self, exists: &Prop, ctx: &EvalContext<'_>) -> Option<bool> {
        if !might_be_ordered(exists) {
            return None;
        }
        let known = self.exists.borrow().get(exists).cloned();
        let count = match known {
            Some(count) => count?,
            None => {
                let count = exists_as_count(exists)
                    .filter(|count| recognise(count, ctx.definitions).is_some())
                    .map(Rc::new);
                self.exists
                    .borrow_mut()
                    .insert(exists.clone(), count.clone());
                count?
            }
        };
        let (mentions, shape) = self.entry(&count, ctx);
        let shape = shape?;
        let group = self.group(&count, &mentions, &shape, ctx)?;
        let holds = shape.any_before(&group, ctx.bindings)?;
        self.probes_answered.set(self.probes_answered.get() + 1);
        Some(holds)
    }

    /// The sorted group for this entry, built once per group, or `None`
    /// when the shape does not apply here.
    fn group(
        &self,
        node: &ValueExpr,
        mentions: &[Var],
        shape: &Shape,
        ctx: &EvalContext<'_>,
    ) -> Option<Rc<Group>> {
        if !shape.applies(ctx.bindings) {
            return None;
        }
        let group_key = key(mentions, ctx, &shape.probe_vars);
        let known = self
            .entries
            .borrow()
            .get(node)
            .and_then(|entry| entry.groups.get(&group_key).cloned());
        let group = match known {
            Some(group) => group,
            None => {
                // No borrow is held while building: the source may
                // evaluate other aggregates.
                let group = Rc::new(shape.build(ctx));
                self.groups_built.set(self.groups_built.get() + 1);
                if let Some(entry) = self.entries.borrow_mut().get_mut(node) {
                    entry.groups.insert(group_key, Rc::clone(&group));
                }
                group
            }
        };
        Some(group)
    }

    /// How many aggregates this evaluation actually computed.
    #[cfg(test)]
    pub(crate) fn computed(&self) -> usize {
        self.computed.get()
    }

    /// How many sorted groups it built, and how many sums they answered.
    #[cfg(test)]
    pub(crate) fn prefix_work(&self) -> (usize, usize) {
        (self.groups_built.get(), self.probes_answered.get())
    }
}

/// The key a result is kept under: the state, whether a transition is in
/// context, and the exact value of each mentioned variable, `None` for
/// one unbound or left out.
fn key(mentions: &[Var], ctx: &EvalContext<'_>, leave_out: &[Var]) -> Key {
    Key {
        state: ctx.state.address(),
        transition: ctx.transition.is_some(),
        values: mentions
            .iter()
            .map(|v| {
                if leave_out.contains(v) {
                    None
                } else {
                    ctx.bindings.get(v).map(|b| exact(b))
                }
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::Memo;
    use crate::definitions::DefinitionTable;
    use crate::eval::{EvalContext, eval_value, find_matches};
    use crate::ir::{Definition, Prop, ValueExpr};
    use crate::ir_builder::{
        add, and, claim, definition, eq, forall, le, or, params, pre, sum, term, var, wildcard,
    };
    use crate::state::{Bindings, Bound, ClaimInstance, EvalValue, State};

    fn decimal(text: &str) -> EvalValue {
        EvalValue::Decimal(rust_decimal::Decimal::from_str_exact(text).unwrap())
    }

    fn row(predicate: &str, args: Vec<EvalValue>) -> ClaimInstance {
        ClaimInstance {
            predicate: predicate.into(),
            args,
        }
    }

    fn subject(text: &str) -> EvalValue {
        EvalValue::Subject(text.into())
    }

    /// An hour's traded total, as one copy of a substituted figure.
    fn hour_total() -> ValueExpr {
        sum(var("q"), claim("Trade", vec![var("h"), var("q")]))
    }

    fn hours(count: usize) -> State {
        let mut claims = Vec::new();
        for h in 0..count {
            let hour = subject(&format!("h{h}"));
            claims.push(row("Hour", vec![hour.clone()]));
            claims.push(row("Trade", vec![hour.clone(), decimal("2")]));
            claims.push(row("Trade", vec![hour, decimal("3")]));
        }
        State::from_claims(claims)
    }

    /// Whether `prop` holds over `state`, with or without a memo, and
    /// how many aggregates the memo computed.
    fn holds(prop: &Prop, state: &State, definitions: &[Definition], memo: bool) -> (bool, usize) {
        let bindings = Bindings::new();
        let keep = Memo::default();
        let ctx = EvalContext::new(
            state,
            None,
            &bindings,
            None,
            DefinitionTable::new(definitions),
        );
        let ctx = if memo { ctx.with_memo(&keep) } else { ctx };
        let held = !find_matches(prop, &ctx).unwrap().is_empty();
        (held, keep.computed())
    }

    /// Three copies of one hour's figure, and one figure that reads no
    /// hour: per hour the copies are computed once, and the figure that
    /// reads no hour once in all.
    #[test]
    fn copies_of_a_figure_are_computed_once_per_binding() {
        let state = hours(24);
        let everything = sum(var("q"), claim("Trade", vec![wildcard(), var("q")]));
        let rule = forall(
            "h",
            claim("Hour", vec![var("h")]),
            le(
                add(
                    add(add(hour_total(), hour_total()), hour_total()),
                    everything,
                ),
                term(crate::ir_builder::dec("1000")),
            ),
        );
        assert_eq!(holds(&rule, &state, &[], true), (true, 24 + 1));
        assert!(holds(&rule, &state, &[], false).0);
    }

    /// Where nothing inside a point ranges over claims, what `check -v`
    /// prints for a grid is what a full grid computes: 25 points, each
    /// computing the copied figure once and the other figure once. A window off the grid by half an hour meets every
    /// period its bound allows.
    #[test]
    fn a_grid_computes_what_its_cost_line_says() {
        use crate::ir_builder::{
            duration, duration_le, in_grid, invariant, periods, program, sub, timestamp,
        };
        let fills = sum(var("q"), claim("Fill", vec![var("h"), var("q")]));
        let rule = and(vec![
            claim("Window", vec![var("from"), var("until")]),
            duration_le(
                sub(term(var("until")), term(var("from"))),
                term(duration("PT24H")),
            ),
            forall(
                "h",
                in_grid(
                    var("h"),
                    periods(
                        term(var("from")),
                        term(var("until")),
                        term(duration("PT1H")),
                    ),
                ),
                le(
                    add(add(hour_total(), hour_total()), fills),
                    term(crate::ir_builder::dec("1000")),
                ),
            ),
        ]);
        let p = program("grid")
            .invariants(vec![invariant("per_hour", rule.clone())])
            .build();
        assert_eq!(
            crate::grid_costs(&p),
            [(
                "invariant `per_hour`".to_string(),
                "up to 25 points x 2 sums over Fill, Trade".to_string()
            )]
        );
        let at = |t: &str| {
            let crate::ir::Term::Literal(v) = timestamp(t) else {
                unreachable!()
            };
            EvalValue::from(&v)
        };
        let state = State::from_claims(vec![row(
            "Window",
            vec![at("2026-03-01T00:30:00Z"), at("2026-03-02T00:30:00Z")],
        )]);
        assert_eq!(holds(&rule, &state, &[], true), (true, 25 * 2));
    }

    /// The same figure under `7` and then `7.0` keeps each binding's own
    /// scale: the key is the value as written, not as compared. A `max`
    /// gives its value as written, where a sum's total is normalised.
    #[test]
    fn a_result_keeps_its_own_bindings_scale() {
        let state = State::from_claims(vec![row("One", vec![subject("x")])]);
        let figure = ValueExpr::Extremum {
            op: crate::ir::ExtremumOp::Max,
            value: var("k"),
            body: Box::new(claim("One", vec![var("x")])),
        };
        let memo = Memo::default();
        let mut seen = Vec::new();
        for k in ["7", "7.0", "7"] {
            let bindings: Bindings = [(crate::ir::Var::from("k"), Bound::from(decimal(k)))].into();
            let ctx = EvalContext::new(&state, None, &bindings, None, DefinitionTable::new(&[]))
                .with_memo(&memo);
            let EvalValue::Decimal(d) = eval_value(&figure, &ctx).unwrap() else {
                panic!("a decimal");
            };
            seen.push(d.to_string());
        }
        assert_eq!(seen, ["7", "7.0", "7"]);
        assert_eq!(memo.computed(), 2);
    }

    /// One figure inside and outside `pre(...)` reads two states.
    #[test]
    fn a_figure_inside_pre_reads_the_pre_state() {
        let after = hours(1);
        let before = State::from_claims(vec![row("Trade", vec![subject("h0"), decimal("1")])]);
        let bindings: Bindings = [(crate::ir::Var::from("h"), Bound::from(subject("h0")))].into();
        let rule = and(vec![
            eq(hour_total(), term(crate::ir_builder::dec("5"))),
            pre(eq(hour_total(), term(crate::ir_builder::dec("1")))),
        ]);
        let memo = Memo::default();
        let ctx = EvalContext::new(
            &after,
            Some(&before),
            &bindings,
            None,
            DefinitionTable::new(&[]),
        )
        .with_memo(&memo);
        assert!(!find_matches(&rule, &ctx).unwrap().is_empty());
        assert_eq!(memo.computed(), 2);
    }

    /// A figure that raises raises the same error on every copy, with or
    /// without the memo.
    #[test]
    fn a_raising_figure_raises_the_same_error_with_the_memo() {
        let state = State::from_claims(vec![
            row("Hour", vec![subject("h0")]),
            row("Trade", vec![subject("h0"), decimal("2")]),
            row("Trade", vec![subject("h0"), subject("not_a_number")]),
        ]);
        let rule = forall(
            "h",
            claim("Hour", vec![var("h")]),
            le(
                add(hour_total(), hour_total()),
                term(crate::ir_builder::dec("1000")),
            ),
        );
        let bindings = Bindings::new();
        let errors: Vec<_> = [true, false]
            .into_iter()
            .map(|memo| {
                let keep = Memo::default();
                let ctx =
                    EvalContext::new(&state, None, &bindings, None, DefinitionTable::new(&[]));
                let ctx = if memo { ctx.with_memo(&keep) } else { ctx };
                find_matches(&rule, &ctx).unwrap_err()
            })
            .collect();
        assert_eq!(errors[0], errors[1]);
    }

    /// A recursive condition whose body counts its own answers: the count
    /// grows each round, and `d` joins only once three others have. A
    /// memo kept inside the fixpoint would keep the first round's count,
    /// and `d` would never join.
    #[test]
    fn a_fixpoint_sees_its_answers_grow() {
        let reach = definition(
            "reach",
            params(&["x"]),
            or(vec![
                claim("Start", vec![var("x")]),
                and(vec![
                    claim("Edge", vec![var("y"), var("x")]),
                    crate::ir_builder::defined("reach", vec![var("y")]),
                ]),
                and(vec![
                    claim("Late", vec![var("x")]),
                    le(
                        term(crate::ir_builder::dec("3")),
                        sum(
                            crate::ir_builder::dec("1"),
                            crate::ir_builder::defined("reach", vec![var("z")]),
                        ),
                    ),
                ]),
            ]),
        );
        let state = State::from_claims(vec![
            row("Start", vec![subject("a")]),
            row("Edge", vec![subject("a"), subject("b")]),
            row("Edge", vec![subject("b"), subject("c")]),
            row("Late", vec![subject("d")]),
        ]);
        let rule = crate::ir_builder::defined("reach", vec![crate::ir_builder::subj("d")]);
        let definitions = [reach];
        assert!(holds(&rule, &state, &definitions, false).0);
        assert!(holds(&rule, &state, &definitions, true).0);
    }
}
