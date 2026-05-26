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

use crate::eval::{EvalContext, EvalError, eval_value, find_matches};
use crate::ir::{DerivedClaim, Invariant};
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
pub fn eval_invariant(
    inv: &Invariant,
    state: &State,
    pre_state: Option<&State>,
) -> Result<bool, EvalError> {
    let bindings = Bindings::new();
    // Invariants evaluate against admitted state with no actor in
    // scope. `Term::Actor` inside an invariant body surfaces as
    // `EvalError::UnboundActor`, enforcing the doctrine that authority
    // checks live in `require`, not in invariants.
    let ctx = EvalContext::new(state, pre_state, &bindings, None);
    let matches = find_matches(&inv.body, &ctx)?;
    Ok(!matches.is_empty())
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
) -> Result<Vec<ClaimInstance>, EvalError> {
    // Derived claims, like invariants in non-proposal contexts, evaluate
    // against admitted state with no transition in scope. `Term::Actor`
    // in a derived-claim body surfaces as `EvalError::UnboundActor`;
    // `Prop::Pre` surfaces as `EvalError::PreStateUnavailable` (derived
    // claims are a function of one state).
    let empty_bindings = Bindings::new();
    let domain_ctx = EvalContext::new(state, None, &empty_bindings, None);
    let raw_bindings = find_matches(&derived.domain, &domain_ctx)?;

    let mut key_tuples: BTreeSet<Vec<EvalValueOrd>> = BTreeSet::new();
    for b in &raw_bindings {
        let mut tuple = Vec::with_capacity(derived.keys.len());
        for k in &derived.keys {
            let v = b.get(k).ok_or_else(|| {
                EvalError::UnboundVariable(format!(
                    "derived claim `{}`: key `{}` not bound by domain expression",
                    derived.predicate, k
                ))
            })?;
            tuple.push(EvalValueOrd(v.clone()));
        }
        key_tuples.insert(tuple);
    }

    let mut out: Vec<ClaimInstance> = Vec::with_capacity(key_tuples.len());
    for tuple in key_tuples {
        let mut per_key = Bindings::new();
        for (k, v) in derived.keys.iter().zip(tuple.iter()) {
            per_key.insert(k.clone(), v.0.clone());
        }
        let mut args: Vec<EvalValue> = tuple.iter().map(|w| w.0.clone()).collect();
        let value_ctx = EvalContext::new(state, None, &per_key, None);
        for value_def in &derived.values {
            let v = eval_value(&value_def.expr, &value_ctx)?;
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
/// - Variants order as `Decimal < Subject < Bool < Collection`.
/// - Within `Decimal`, the natural decimal ordering applies (so
///   `100` sorts before `200`, not lexicographic on the string).
/// - Within `Subject`, the natural string ordering applies.
/// - Within `Bool`, `false < true` (the derived `Ord` on `bool`).
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
            }
        }

        match (&self.0, &other.0) {
            (EvalValue::Decimal(a), EvalValue::Decimal(b)) => a.cmp(b),
            (EvalValue::Subject(a), EvalValue::Subject(b)) => a.as_str().cmp(b.as_str()),
            (EvalValue::Bool(a), EvalValue::Bool(b)) => a.cmp(b),
            (EvalValue::Date(a), EvalValue::Date(b)) => a.cmp(b),
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
