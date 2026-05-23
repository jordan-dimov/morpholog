//! Layer 1 of enriched `morpholog check`: kind/type compatibility.
//!
//! Walks every expression in every invariant, transformation, and
//! derived claim, checking that each value flows into a slot of a
//! compatible kind. Predicate declarations carry the expected kind
//! per arg position; comparators, arithmetic, and aggregators have
//! fixed expected kinds; variables are inferred-and-refined.
//!
//! The kernel raises these problems at runtime as
//! `EvalError::TypeMismatch`; this layer surfaces them at
//! authoring time so a faulty `.morph` file fails `morpholog check`
//! before any state is touched.
//!
//! Diagnostics ship without source spans for v0. The IR drops
//! parser spans on lowering today; threading spans through the IR
//! is its own design conversation. The existing `ValidationError`
//! shape (no spans, just a `ValidationContext`) is matched here.
//!
//! `Any` is treated as **unconstrained**, not as "compatible with
//! everything forever once attached to a variable." First use in an
//! `Any` slot leaves a variable `UnknownOrAny`; later use in a
//! specific slot refines it to that specific kind. This keeps `Any`
//! as an honest escape hatch without making it a kind-eraser.

// The scaffolding lands in one commit; the per-pass walkers that
// consume `InferredKind` / `KindEnv` arrive in following commits.
// The unit tests below already exercise the types, so the
// scaffolding is not load-bearing-untested - just not yet wired
// into `validate_program`.
#![allow(dead_code)]

use std::collections::HashMap;

use crate::ir::{PredicateArgKind, Program};
use crate::validate::ValidationError;

/// Inferred kind of a value during static analysis. Distinct from
/// [`PredicateArgKind`] (which is the *declared* kind on a predicate
/// position) because variables can be observed-but-not-yet-pinned -
/// the `UnknownOrAny` state. A variable seen only through an `Any`
/// slot stays unconstrained and refines to a specific kind when
/// later observed in a specific slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InferredKind {
    /// Either an `Any`-declared slot or a variable not yet observed
    /// in any specific slot. Compatible with every other kind and
    /// refinable to a specific kind on first specific observation.
    UnknownOrAny,
    /// A specific kind learned from a literal, a specific-kind
    /// declaration, or a prior refinement.
    Known(PredicateArgKind),
}

impl InferredKind {
    /// True when two inferred kinds can co-exist on the same value
    /// slot. The rules:
    ///
    /// - `UnknownOrAny` is compatible with everything.
    /// - Two `Known` kinds are compatible only if they are equal, or
    ///   one of them is `Any` (the declaration-side escape hatch).
    pub(crate) fn compatible(self, other: InferredKind) -> bool {
        match (self, other) {
            (InferredKind::UnknownOrAny, _) | (_, InferredKind::UnknownOrAny) => true,
            (InferredKind::Known(a), InferredKind::Known(b)) => kinds_compatible(a, b),
        }
    }

    /// Combine an existing inferred kind with a new observation.
    /// Returns `Ok(refined)` when compatible; `Err((prev, new))`
    /// when the two specific kinds genuinely conflict. The refined
    /// kind is whichever side is more specific (a `Known(X)` always
    /// wins over an `UnknownOrAny`).
    pub(crate) fn refine(
        self,
        observed: InferredKind,
    ) -> Result<InferredKind, (PredicateArgKind, PredicateArgKind)> {
        match (self, observed) {
            (InferredKind::UnknownOrAny, observed) => Ok(observed),
            (existing, InferredKind::UnknownOrAny) => Ok(existing),
            (InferredKind::Known(prev), InferredKind::Known(new)) => {
                if kinds_compatible(prev, new) {
                    // Prefer the more specific of the two: `Any` on
                    // either side loses to a concrete kind.
                    if matches!(prev, PredicateArgKind::Any) {
                        Ok(InferredKind::Known(new))
                    } else {
                        Ok(InferredKind::Known(prev))
                    }
                } else {
                    Err((prev, new))
                }
            }
        }
    }
}

/// Compatibility rule for two specific declared kinds. `Any` on
/// either side is the declaration-level escape hatch; otherwise
/// strict equality is required.
fn kinds_compatible(a: PredicateArgKind, b: PredicateArgKind) -> bool {
    a == PredicateArgKind::Any || b == PredicateArgKind::Any || a == b
}

/// Scope-local map from variable name to inferred kind. Mutable
/// during expression and statement walks; passed by `&mut` through
/// the recursive checker. Distinct kind environments live per
/// invariant body, per derived-claim body, per transformation
/// (extended statement-by-statement following the runtime quartet
/// doctrine).
#[derive(Debug, Default, Clone)]
pub(crate) struct KindEnv {
    bindings: HashMap<String, InferredKind>,
}

impl KindEnv {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Look up a variable's current inferred kind. Returns
    /// `UnknownOrAny` for variables never observed before - that
    /// matches how an unconstrained slot would treat them.
    pub(crate) fn lookup(&self, name: &str) -> InferredKind {
        self.bindings
            .get(name)
            .copied()
            .unwrap_or(InferredKind::UnknownOrAny)
    }

    /// Observe a variable at the given inferred kind. Refines the
    /// stored kind if compatible; reports a conflict otherwise.
    ///
    /// The conflict tuple is `(previous, new)` so the caller can
    /// emit a `VariableKindConflict` diagnostic with both kinds
    /// named.
    pub(crate) fn observe(
        &mut self,
        name: &str,
        observed: InferredKind,
    ) -> Result<(), (PredicateArgKind, PredicateArgKind)> {
        let existing = self.lookup(name);
        let refined = existing.refine(observed)?;
        self.bindings.insert(name.to_string(), refined);
        Ok(())
    }
}

/// Run the kind checker over the whole programme. Currently a
/// stub returning no errors; per-pass logic lands in following
/// commits (predicate arg checking, comparators / arithmetic, Sum
/// / ValueOf, statement flow). Wired into `validate_program` once
/// the per-pass logic is in place.
pub(crate) fn kindcheck_program(_program: &Program) -> Vec<ValidationError> {
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_is_compatible_with_everything() {
        assert!(InferredKind::UnknownOrAny.compatible(InferredKind::UnknownOrAny));
        assert!(
            InferredKind::UnknownOrAny.compatible(InferredKind::Known(PredicateArgKind::Decimal))
        );
        assert!(
            InferredKind::Known(PredicateArgKind::Subject).compatible(InferredKind::UnknownOrAny)
        );
    }

    #[test]
    fn same_known_kinds_are_compatible() {
        let dec = InferredKind::Known(PredicateArgKind::Decimal);
        assert!(dec.compatible(dec));
    }

    #[test]
    fn distinct_known_kinds_conflict() {
        let dec = InferredKind::Known(PredicateArgKind::Decimal);
        let sub = InferredKind::Known(PredicateArgKind::Subject);
        assert!(!dec.compatible(sub));
        assert!(!sub.compatible(dec));
    }

    #[test]
    fn any_is_compatible_with_anything_declared() {
        let any = InferredKind::Known(PredicateArgKind::Any);
        let dec = InferredKind::Known(PredicateArgKind::Decimal);
        let sub = InferredKind::Known(PredicateArgKind::Subject);
        assert!(any.compatible(dec));
        assert!(any.compatible(sub));
        assert!(dec.compatible(any));
        assert!(sub.compatible(any));
    }

    #[test]
    fn refine_unknown_to_known_yields_known() {
        let refined = InferredKind::UnknownOrAny
            .refine(InferredKind::Known(PredicateArgKind::Decimal))
            .expect("compatible");
        assert_eq!(refined, InferredKind::Known(PredicateArgKind::Decimal));
    }

    #[test]
    fn refine_known_then_unknown_keeps_known() {
        let refined = InferredKind::Known(PredicateArgKind::Decimal)
            .refine(InferredKind::UnknownOrAny)
            .expect("compatible");
        assert_eq!(refined, InferredKind::Known(PredicateArgKind::Decimal));
    }

    #[test]
    fn refine_any_then_decimal_yields_decimal() {
        // The `Any`-declared slot was the first observation; a
        // later specific use refines the variable to that specific
        // kind rather than leaving it permissive.
        let refined = InferredKind::Known(PredicateArgKind::Any)
            .refine(InferredKind::Known(PredicateArgKind::Decimal))
            .expect("compatible");
        assert_eq!(refined, InferredKind::Known(PredicateArgKind::Decimal));
    }

    #[test]
    fn refine_decimal_then_any_keeps_decimal() {
        let refined = InferredKind::Known(PredicateArgKind::Decimal)
            .refine(InferredKind::Known(PredicateArgKind::Any))
            .expect("compatible");
        assert_eq!(refined, InferredKind::Known(PredicateArgKind::Decimal));
    }

    #[test]
    fn refine_decimal_then_subject_conflicts() {
        let err = InferredKind::Known(PredicateArgKind::Decimal)
            .refine(InferredKind::Known(PredicateArgKind::Subject))
            .expect_err("conflict");
        assert_eq!(err, (PredicateArgKind::Decimal, PredicateArgKind::Subject));
    }

    #[test]
    fn kindenv_observe_then_lookup_returns_refined_kind() {
        let mut env = KindEnv::new();
        env.observe("amount", InferredKind::Known(PredicateArgKind::Decimal))
            .expect("first observation always succeeds against UnknownOrAny");
        assert_eq!(
            env.lookup("amount"),
            InferredKind::Known(PredicateArgKind::Decimal)
        );
    }

    #[test]
    fn kindenv_observe_refines_through_any() {
        let mut env = KindEnv::new();
        env.observe("x", InferredKind::Known(PredicateArgKind::Any))
            .unwrap();
        env.observe("x", InferredKind::Known(PredicateArgKind::Decimal))
            .unwrap();
        assert_eq!(
            env.lookup("x"),
            InferredKind::Known(PredicateArgKind::Decimal)
        );
    }

    #[test]
    fn kindenv_observe_reports_conflict_with_previous_kinds() {
        let mut env = KindEnv::new();
        env.observe("x", InferredKind::Known(PredicateArgKind::Decimal))
            .unwrap();
        let err = env
            .observe("x", InferredKind::Known(PredicateArgKind::Subject))
            .expect_err("conflict");
        assert_eq!(err, (PredicateArgKind::Decimal, PredicateArgKind::Subject));
    }
}
