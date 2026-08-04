//! Invariant evaluation and derived-claim enumeration.
//!
//! `eval_invariant` runs an invariant body against admitted state;
//! returns `Ok(true)` if it holds, `Ok(false)` if any matching binding
//! invalidates the body. `enumerate_derived` runs a `DerivedClaim`
//! against admitted state and returns the deterministically-ordered
//! sequence of resolved `ClaimInstance` output rows.
//!
//! `EvalValueOrd` is the private helper used by `enumerate_derived` to
//! deduplicate key tuples via a `BTreeSet` without committing
//! `EvalValue` itself to a sort order.

use std::collections::BTreeSet;

use crate::definitions::DefinitionTable;
use crate::eval::{EvalContext, EvalError, eval_value, find_matches};
use crate::ir::{Definition, DerivedClaim, Invariant};
use crate::propose::WitnessBinding;
use crate::state::{Bindings, ClaimInstance, EvalValue, State};

/// Evaluate an invariant against a state. Returns true if the invariant
/// holds, false if it fails.
///
/// `pre_state` is the pre-transition snapshot when the caller has one in
/// scope (the proposal path, after staging assertions / retractions and
/// before commit); pass `None` for state-only contexts (the read-side
/// `enumerate_derived` consumers, the PostgreSQL adapter's standalone
/// checks, tests that exercise an invariant against a hand-built state).
/// Invariants that do not contain [`crate::Prop::Pre`] behave identically
/// in both modes. Invariants that *do* reach for `Pre` in a `None`
/// context surface [`EvalError::PreStateUnavailable`].
/// `definitions` is the programme's definitions vocabulary, for
/// resolving `Prop::Defined` calls in the body; pass `&[]` for a
/// programme without definitions.
pub fn eval_invariant(
    inv: &Invariant,
    state: &State,
    pre_state: Option<&State>,
    definitions: &[Definition],
) -> Result<bool, EvalError> {
    in_invariant_context(state, pre_state, definitions, |ctx| {
        let matches = find_matches(&inv.body, ctx)?;
        Ok(!matches.is_empty())
    })
}

/// The binding assignment that witnesses an invariant's failure: the
/// values live where the drill-down stopped, sorted by variable
/// (`Bindings` is a `HashMap`, whose iteration order is not stable and
/// would otherwise flake the pinned envelopes).
///
/// Sorting fixes how one assignment renders, not **which** assignment is
/// chosen. When several subjects violate the same rule, the witness is
/// the first violation in state order - so the same claims in a different
/// order can name a different subject, while the verdict is unchanged.
/// The PostgreSQL path loads claims in primary-key order for exactly this
/// reason, so the same database explains a refusal the same way twice; a
/// hand-built `State` gets whatever order it was built in.
///
/// Empty exactly when the failure has no binding assignment to report -
/// which is a question about what was bound where the drill-down stopped,
/// not about which operator failed. A comparison that fails under a
/// quantifier or an implication witnesses the variables its antecedent
/// bound; the same comparison as a whole invariant body witnesses nothing,
/// because nothing was ever bound.
/// Callers ask for this only after
/// [`eval_invariant`] returned `false`; it is a diagnosis of a decided
/// rejection, never part of deciding one.
pub fn invariant_witness(
    inv: &Invariant,
    state: &State,
    pre_state: Option<&State>,
    definitions: &[Definition],
) -> Result<Vec<WitnessBinding>, EvalError> {
    in_invariant_context(state, pre_state, definitions, |ctx| {
        let Some(failure) = crate::eval::find_failure(&inv.body, ctx) else {
            return Ok(Vec::new());
        };
        let mut witness: Vec<WitnessBinding> = failure
            .bindings
            .into_iter()
            .map(|(var, value)| WitnessBinding { var, value })
            .collect();
        witness.sort_by(|a, b| a.var.cmp(&b.var));
        Ok(witness)
    })
}

/// Invariants evaluate against admitted state with no actor in scope.
/// `Term::Actor` inside an invariant body surfaces as
/// `EvalError::UnboundActor`, enforcing the doctrine that authority
/// checks live in `require`, not in invariants.
fn in_invariant_context<T>(
    state: &State,
    pre_state: Option<&State>,
    definitions: &[Definition],
    body: impl FnOnce(&EvalContext<'_>) -> Result<T, EvalError>,
) -> Result<T, EvalError> {
    let bindings = Bindings::new();
    let ctx = EvalContext::new(
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
/// Algorithm:
///
/// 1. Run `find_matches` on `derived.domain` to get every binding that
///    satisfies the domain expression.
/// 2. Project each binding onto the `derived.keys` and deduplicate.
///    The deduplication uses a `BTreeSet`, which also gives the output
///    a stable ordering by key tuple.
/// 3. For each distinct key binding, evaluate each
///    [`crate::DerivedValue::expr`] via the internal value evaluator under that
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
    definitions: &[Definition],
) -> Result<Vec<ClaimInstance>, EvalError> {
    // Derived claims, like invariants in non-proposal contexts, evaluate
    // against admitted state with no transition in scope. `Term::Actor`
    // in a derived-claim body surfaces as `EvalError::UnboundActor`;
    // `Prop::Pre` surfaces as `EvalError::PreStateUnavailable` (derived
    // claims are a function of one state).
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
            tuple.push(EvalValueOrd(v.clone()));
        }
        key_tuples.insert(tuple);
    }

    let mut out: Vec<ClaimInstance> = Vec::with_capacity(key_tuples.len());
    for tuple in key_tuples {
        let mut per_key = Bindings::new();
        for (key, v) in derived.keys.iter().zip(tuple.iter()) {
            per_key.insert(key.clone(), v.0.clone());
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
/// `EvalValue` does not derive `Ord`. Wrap it in a newtype that
/// implements `Ord` *structurally* so we can deduplicate key tuples
/// in a `BTreeSet` without committing the kernel's runtime-value
/// type to a sort order externally. Used only inside
/// [`enumerate_derived`]; not exposed.
///
/// The ordering is infallible and `Eq`-consistent:
/// - Variants order by the stable discriminant in `cmp` (`Decimal <
///   Subject < Bool < Collection < Date < Timestamp < Duration <
///   Quantity`) - arbitrary but fixed.
/// - Within `Decimal`, `Date`, `Timestamp`, and `Duration`, the
///   natural ordering of the underlying value applies (so `100`
///   sorts before `200`, not lexicographic on the string).
/// - Within `Subject`, the natural string ordering applies.
/// - Within `Bool`, `false < true` (the derived `Ord` on `bool`).
/// - Within `Quantity`, by unit first, then amount - units are
///   incomparable domains, so grouping by label is the only
///   deterministic order that never ranks across units.
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
            // Quantities order by unit first, then amount - units are
            // incomparable domains, so grouping by label is the only
            // deterministic order that never ranks across units.
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
