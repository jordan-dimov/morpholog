//! Invariant evaluation and derived-claim enumeration.
//!
//! `eval_invariant` checks an invariant against admitted state.
//! `enumerate_derived` computes a derived claim's rows, in a deterministic
//! order.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;

use crate::definitions::DefinitionTable;

use crate::eval::{EvalContext, EvalError, Failure, eval_value, find_failure, find_matches};
use crate::ir::{Definition, DerivedClaim, Invariant, Prop, Var};
use crate::propose::WitnessBinding;
use crate::state::{Bindings, ClaimInstance, EvalValue, State, StateRef};

/// Evaluate an invariant against a state. Returns true if the invariant
/// holds, false if it fails.
///
/// `pre_state` is the state before the transition, when the caller has one
/// (the proposal path). Pass `None` to check a single state. Only an
/// invariant using [`crate::Prop::Pre`] cares; with `None` it fails with
/// [`EvalError::PreStateUnavailable`].
///
/// `definitions` resolves `Prop::Defined` calls in the body; pass `&[]`
/// when the programme has none.
pub(crate) fn eval_invariant(
    inv: &Invariant,
    state: &State,
    pre_state: Option<&State>,
    definitions: &[Definition],
) -> Result<bool, EvalError> {
    eval_invariant_over(inv, state.into(), pre_state, definitions)
}

/// [`eval_invariant`] over whatever the evaluation reads, a candidate
/// view included.
pub(crate) fn eval_invariant_over(
    inv: &Invariant,
    state: StateRef<'_>,
    pre_state: Option<&State>,
    definitions: &[Definition],
) -> Result<bool, EvalError> {
    in_invariant_context(state, pre_state, definitions, |ctx| {
        let matches = find_matches(&inv.body, ctx)?;
        Ok(!matches.is_empty())
    })
}

/// Visit the antecedent's matches in the touched cases, and only those.
/// Each case's bindings are in the context before the antecedent is
/// matched, so the matcher seeks through the state's index and never
/// meets a claim outside the case: the work follows the cases, not the
/// state, and a case the transition did not reach can raise no error.
/// Cases are visited in the plan's order and matches in state order
/// within a case; a match in several overlapping cases is visited under
/// the earliest. `visit` returns whether to go on.
fn for_each_case_match(
    antecedent: &Prop,
    ctx: &EvalContext<'_>,
    cases: &[BTreeMap<Var, EvalValue>],
    mut visit: impl FnMut(&Bindings) -> Result<bool, EvalError>,
) -> Result<(), EvalError> {
    let mut visited: HashSet<Vec<(Var, Arc<EvalValue>)>> = HashSet::new();
    for case in cases {
        let seed: Bindings = case
            .iter()
            .map(|(v, ev)| (v.clone(), Arc::new(ev.clone())))
            .collect();
        let seeded = ctx.with_bindings(&seed);
        for m in find_matches(antecedent, &seeded)? {
            if cases.len() > 1 {
                let mut key: Vec<(Var, Arc<EvalValue>)> = m
                    .iter()
                    .map(|(v, ev)| (v.clone(), Arc::clone(ev)))
                    .collect();
                key.sort_by(|a, b| a.0.cmp(&b.0));
                if !visited.insert(key) {
                    continue;
                }
            }
            if !visit(&m)? {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// [`eval_invariant`] over the touched cases only: the antecedent's
/// bindings restricted to `cases`. Every case is evaluated, so an error in
/// any of them wins over `false`. This is what admission checks, not the
/// invariant's whole-state meaning. Shapes the impact plan never bounds
/// evaluate whole.
pub(crate) fn eval_invariant_cases(
    inv: &Invariant,
    state: &State,
    pre_state: Option<&State>,
    definitions: &[Definition],
    cases: &[BTreeMap<Var, EvalValue>],
) -> Result<bool, EvalError> {
    eval_invariant_cases_over(inv, state.into(), pre_state, definitions, cases)
}

pub(crate) fn eval_invariant_cases_over(
    inv: &Invariant,
    state: StateRef<'_>,
    pre_state: Option<&State>,
    definitions: &[Definition],
    cases: &[BTreeMap<Var, EvalValue>],
) -> Result<bool, EvalError> {
    in_invariant_context(state, pre_state, definitions, |ctx| match &inv.body {
        Prop::Implies { left, right }
        | Prop::Forall {
            source: left,
            body: right,
            ..
        } => {
            let mut holds = true;
            for_each_case_match(left, ctx, cases, |m| {
                if find_matches(right, &ctx.with_bindings(m))?.is_empty() {
                    holds = false;
                }
                Ok(true)
            })?;
            Ok(holds)
        }
        // Every case is evaluated before the answer, so an error in a
        // later case wins over a match in an earlier one.
        Prop::Not(inner) => {
            let mut matched = false;
            for_each_case_match(inner, ctx, cases, |_| {
                matched = true;
                Ok(true)
            })?;
            Ok(!matched)
        }
        _ => Ok(!find_matches(&inv.body, ctx)?.is_empty()),
    })
}

/// [`invariant_witness`] drawn from the touched cases only, so a
/// bounded refusal never blames a case the transition did not reach.
/// The witness is the first failing match of the earliest touched case,
/// in state order within it. A negated top-level body witnesses
/// nothing, as it does whole.
#[cfg(test)]
pub(crate) fn invariant_witness_cases(
    inv: &Invariant,
    state: &State,
    pre_state: Option<&State>,
    definitions: &[Definition],
    cases: &[BTreeMap<Var, EvalValue>],
) -> Result<Diagnosis, EvalError> {
    invariant_witness_cases_over(inv, state.into(), pre_state, definitions, cases)
}

pub(crate) fn invariant_witness_cases_over(
    inv: &Invariant,
    state: StateRef<'_>,
    pre_state: Option<&State>,
    definitions: &[Definition],
    cases: &[BTreeMap<Var, EvalValue>],
) -> Result<Diagnosis, EvalError> {
    in_invariant_context(state, pre_state, definitions, |ctx| {
        let (left, right) = match &inv.body {
            Prop::Implies { left, right } => (left, right),
            Prop::Forall { source, body, .. } => (source, body),
            Prop::Not(_) => return Ok(Diagnosis::default()),
            // A body with nothing to descend into, a comparison for one,
            // is its own leaf: no bindings, and its operands if it is one.
            _ => {
                return Ok(Diagnosis::of(
                    find_failure(&inv.body, ctx).unwrap_or_else(|| Failure::here(&inv.body, ctx)),
                ));
            }
        };
        let mut diagnosis = Diagnosis::default();
        for_each_case_match(left, ctx, cases, |m| {
            let ext = ctx.with_bindings(m);
            if find_matches(right, &ext)?.is_empty() {
                let failure =
                    find_failure(right, &ext).unwrap_or_else(|| Failure::here(right, &ext));
                diagnosis = Diagnosis::of(failure);
                return Ok(false);
            }
            Ok(true)
        })?;
        Ok(diagnosis)
    })
}

/// What a refusal says beyond the rule: the bindings where it failed and
/// the comparison it blames, from one descent, so they describe one
/// failing case.
#[derive(Debug, Default)]
pub(crate) struct Diagnosis {
    pub(crate) witness: Vec<WitnessBinding>,
    pub(crate) compared: Option<crate::propose::Compared>,
}

impl Diagnosis {
    fn of(failure: Failure) -> Self {
        Self {
            witness: sorted_witness(failure.bindings),
            compared: failure.compared,
        }
    }
}

pub(crate) fn sorted_witness(bindings: Bindings) -> Vec<WitnessBinding> {
    let mut witness: Vec<WitnessBinding> = bindings
        .into_iter()
        .map(|(var, value)| WitnessBinding {
            var,
            value: Arc::unwrap_or_clone(value),
        })
        .collect();
    witness.sort_by(|a, b| a.var.cmp(&b.var));
    witness
}

/// The variable bindings that show why an invariant failed, sorted by
/// variable so the output is stable.
///
/// When several subjects break the same rule, the witness is the first
/// violation in state order, so the same claims in another order can name
/// a different subject. The PostgreSQL path loads claims in primary-key
/// order so the same database explains a refusal the same way twice.
///
/// Empty when nothing was bound where the failure was found. A comparison
/// under a quantifier or implication reports what its antecedent bound;
/// the same comparison as the whole invariant body reports nothing.
///
/// Call this only after [`eval_invariant`] returned `false`. It explains a
/// rejection; it never decides one.
#[cfg(test)]
pub(crate) fn invariant_witness(
    inv: &Invariant,
    state: &State,
    pre_state: Option<&State>,
    definitions: &[Definition],
) -> Result<Diagnosis, EvalError> {
    invariant_witness_over(inv, state.into(), pre_state, definitions)
}

pub(crate) fn invariant_witness_over(
    inv: &Invariant,
    state: StateRef<'_>,
    pre_state: Option<&State>,
    definitions: &[Definition],
) -> Result<Diagnosis, EvalError> {
    in_invariant_context(state, pre_state, definitions, |ctx| {
        // A negated body witnesses nothing, as the case-bound path says;
        // any other body with nothing to descend into is its own leaf.
        if matches!(inv.body, Prop::Not(_)) {
            return Ok(Diagnosis::default());
        }
        Ok(Diagnosis::of(
            crate::eval::find_failure(&inv.body, ctx)
                .unwrap_or_else(|| Failure::here(&inv.body, ctx)),
        ))
    })
}

/// Invariants evaluate with no actor in scope, so `Term::Actor` in a body
/// is `EvalError::UnboundActor`. Authority checks belong in `require`.
fn in_invariant_context<T>(
    state: StateRef<'_>,
    pre_state: Option<&State>,
    definitions: &[Definition],
    body: impl FnOnce(&EvalContext<'_>) -> Result<T, EvalError>,
) -> Result<T, EvalError> {
    let bindings = Bindings::new();
    let ctx = EvalContext::over(
        state,
        pre_state,
        &bindings,
        None,
        DefinitionTable::new(definitions),
    );
    body(&ctx)
}

/// Enumerate a derived claim against current admitted state. Returns
/// one [`ClaimInstance`] per distinct key tuple, in deterministic key
/// order.
///
/// Each row is a distinct key tuple from the domain's matches, followed by
/// each [`crate::DerivedValue::expr`] evaluated under that key.
///
/// Evaluation errors propagate, as does a key the domain leaves unbound.
/// The rows are *not* added to `state.claims`.
pub(crate) fn enumerate_derived(
    derived: &DerivedClaim,
    state: &State,
    definitions: &[Definition],
) -> Result<Vec<ClaimInstance>, EvalError> {
    // A derived claim reads one state with no transition in scope, so
    // `actor` is `UnboundActor` and `pre(...)` is `PreStateUnavailable`.
    let empty_bindings = Bindings::new();
    let index = DefinitionTable::new(definitions);
    let domain_ctx = EvalContext::new(state, None, &empty_bindings, None, index);
    let raw_bindings = find_matches(&derived.domain, &domain_ctx)?;

    let mut key_tuples: BTreeSet<Vec<EvalValueOrd>> = BTreeSet::new();
    for b in &raw_bindings {
        let mut tuple = Vec::with_capacity(derived.keys.len());
        for key in &derived.keys {
            let v = b.get(key).ok_or_else(|| {
                EvalError::UnboundVariable(format!(
                    "derived claim `{}`: key `{}` not bound by domain expression",
                    derived.predicate, key
                ))
            })?;
            tuple.push(EvalValueOrd((**v).clone()));
        }
        key_tuples.insert(tuple);
    }

    let mut out: Vec<ClaimInstance> = Vec::with_capacity(key_tuples.len());
    for tuple in key_tuples {
        let mut per_key = Bindings::new();
        for (key, v) in derived.keys.iter().zip(tuple.iter()) {
            per_key.insert(key.clone(), Arc::new(v.0.clone()));
        }
        let mut args: Vec<EvalValue> = tuple.iter().map(|w| w.0.clone()).collect();
        let value_ctx = EvalContext::new(state, None, &per_key, None, index);
        for value_def in &derived.values {
            let v = eval_value(&value_def.expr, &value_ctx)?;
            if v.contains_calendar_span() {
                return Err(EvalError::TypeMismatch(format!(
                    "a calendar span cannot be a derived value of `{}`: it shifts \
                     a date inside an expression and is never itself a governed value",
                    derived.predicate
                )));
            }
            args.push(v);
        }
        out.push(ClaimInstance {
            predicate: derived.predicate.clone(),
            args,
        });
    }
    Ok(out)
}
/// A total, `Eq`-consistent order on `EvalValue`, used only to dedupe and
/// sort key tuples in [`enumerate_derived`]. `EvalValue` itself stays
/// unordered.
///
/// Different variants order by a fixed but arbitrary rank. Within a
/// variant, values use their natural order (decimals numerically, not as
/// strings). Collections compare element by element, shorter first on a
/// shared prefix.
///
/// The only promise about output order is that it is deterministic.
/// Callers wanting a business order sort the result themselves.
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
                EvalValue::Timestamp(_) => 5,
                EvalValue::Duration(_) => 6,
                EvalValue::Quantity { .. } => 7,
                EvalValue::CalendarSpan(_) => 8,
            }
        }

        match (&self.0, &other.0) {
            (EvalValue::Decimal(a), EvalValue::Decimal(b)) => a.cmp(b),
            (EvalValue::Subject(a), EvalValue::Subject(b)) => a.as_str().cmp(b.as_str()),
            (EvalValue::Bool(a), EvalValue::Bool(b)) => a.cmp(b),
            (EvalValue::Date(a), EvalValue::Date(b)) => a.cmp(b),
            (EvalValue::Timestamp(a), EvalValue::Timestamp(b)) => a.cmp(b),
            (EvalValue::Duration(a), EvalValue::Duration(b)) => a.cmp(b),
            // Unit first, then amount: amounts in different units never
            // compare.
            (
                EvalValue::Quantity { amount: a, unit: u },
                EvalValue::Quantity { amount: b, unit: v },
            ) => u.cmp(v).then_with(|| a.cmp(b)),
            (EvalValue::CalendarSpan(a), EvalValue::CalendarSpan(b)) => {
                (a.months, a.days).cmp(&(b.months, b.days))
            }
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
