//! Property-based round-trip tests for the JSON codec.
//!
//! Companion to `tests/codec.rs`, which pins specific wire shapes
//! against known inputs. The example tests catch shape regressions
//! (a renamed tag, a flipped serde attribute); these property tests
//! catch *value* regressions across the space of decimals, subjects,
//! booleans, and nested collections.
//!
//! Each property runs 256 cases by default (proptest's default) with
//! shrinking on failure, so a counterexample comes back minimised.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use jiff::civil::Date;
use morpholog_core::{ClaimInstance, EvalValue, IntentInstance, PredicateName};
use proptest::prelude::*;
use rust_decimal::Decimal;

/// Generate `rust_decimal::Decimal` values from the `i64` mantissa
/// subset of the representable space, paired with scales 0..=28 (the
/// `rust_decimal` maximum). The full `rust_decimal` mantissa is 96-bit;
/// we deliberately stay inside `i64` here because the codec contract
/// being exercised (decimal → JSON string → decimal, exactness
/// preserved) does not depend on mantissa width, and `i64` keeps
/// shrinking reports small and the strategy cheap.
fn arb_decimal() -> impl Strategy<Value = Decimal> {
    (any::<i64>(), 0u32..=28u32).prop_map(|(mantissa, scale)| Decimal::new(mantissa, scale))
}

/// Generate subject identifiers from a conservative character set
/// (letters, digits, underscore). Wider character sets are unlikely
/// to surface JSON round-trip bugs the codec doesn't already handle
/// via serde's string escaping, and bounded length keeps shrinking
/// reports small.
fn arb_subject() -> impl Strategy<Value = String> {
    "[a-zA-Z][a-zA-Z0-9_]{0,16}".prop_map(|s| s)
}

/// Generate civil dates from a bounded calendar range. The codec
/// contract being exercised (date → JSON string → date, exactness
/// preserved) does not depend on extreme years; constraining to a
/// realistic window keeps shrinking reports small. February overflow
/// (day 30/31 in a 28/29-day month) is handled by `Date::new`
/// returning an error, which the strategy filters out.
fn arb_civil_date() -> impl Strategy<Value = Date> {
    (1970i16..=2100i16, 1i8..=12i8, 1i8..=31i8)
        .prop_filter_map("invalid civil date", |(y, m, d)| Date::new(y, m, d).ok())
}

/// Recursive `EvalValue` strategy. Leaves are decimals, subjects,
/// booleans, and civil dates; collections wrap an inner strategy.
/// Bounded depth keeps generation finite.
fn arb_eval_value() -> impl Strategy<Value = EvalValue> {
    let leaf = prop_oneof![
        arb_decimal().prop_map(EvalValue::Decimal),
        arb_subject().prop_map(|s| EvalValue::Subject(s.into())),
        any::<bool>().prop_map(EvalValue::Bool),
        arb_civil_date().prop_map(EvalValue::Date),
    ];
    leaf.prop_recursive(
        3, // max depth: leaf, leaf-in-collection, collection-in-collection
        8, // total target size in leaves
        4, // collection size per level
        |inner| prop::collection::vec(inner, 0..=4).prop_map(EvalValue::Collection),
    )
}

fn arb_predicate_name() -> impl Strategy<Value = PredicateName> {
    "[A-Z][a-zA-Z0-9_]{0,24}".prop_map(PredicateName::from)
}

fn arb_intent_name() -> impl Strategy<Value = String> {
    "[A-Z][a-zA-Z0-9_]{0,24}".prop_map(|s| s)
}

proptest! {
    /// Every `EvalValue` we can generate round-trips through JSON
    /// without value loss: the parsed value equals the original.
    /// Includes nested collections. Does *not* assert byte-identical
    /// JSON on re-serialisation - that is a serde-implementation
    /// property, not a Morpholog contract.
    #[test]
    fn eval_value_json_round_trip(v in arb_eval_value()) {
        let json = serde_json::to_string(&v).unwrap();
        let parsed: EvalValue = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, v);
    }

    /// `ClaimInstance` round-trips through JSON for any predicate
    /// name and argument list.
    #[test]
    fn claim_instance_json_round_trip(
        predicate in arb_predicate_name(),
        args in prop::collection::vec(arb_eval_value(), 0..6),
    ) {
        let c = ClaimInstance { predicate, args };
        let json = serde_json::to_string(&c).unwrap();
        let parsed: ClaimInstance = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, c);
    }

    /// `IntentInstance` round-trips through JSON for any intent name
    /// and argument list.
    #[test]
    fn intent_instance_json_round_trip(
        name in arb_intent_name(),
        args in prop::collection::vec(arb_eval_value(), 0..6),
    ) {
        let i = IntentInstance { name, args };
        let json = serde_json::to_string(&i).unwrap();
        let parsed: IntentInstance = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed, i);
    }

    /// Decimal values always serialise as JSON strings, never as
    /// JSON numbers. Pins the exactness contract across arbitrary
    /// decimals (the example test in `tests/codec.rs` covers only
    /// one specific high-precision value).
    #[test]
    fn decimal_eval_value_always_serialises_as_string(d in arb_decimal()) {
        let v = EvalValue::Decimal(d);
        let json = serde_json::to_string(&v).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let inner = parsed.get("value").expect("EvalValue::Decimal must have a 'value' field");
        prop_assert!(
            inner.is_string(),
            "decimal must serialise as a JSON string, got: {inner}"
        );
    }
}
