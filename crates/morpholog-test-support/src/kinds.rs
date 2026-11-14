//! Every value kind, and values of each, for the conformance tests that
//! hold each surface a value crosses to the same kinds: the tagged and
//! named wire, the key the database compares by, the view column and the
//! generated client. A new kind does not compile until [`kind_samples`]
//! has its samples, a test fails until [`every_kind`] lists it, and each
//! surface's test then fails until it handles the kind or refuses it by
//! name.

use morpholog_core::{EvalValue, PredicateArgKind};

use crate::{bool_, cal_span, coll, date, dec, dec_str, dur, qty, subj, ts};

/// One representative of every kind. A test holds this list to the
/// variants serde knows `PredicateArgKind` has, so a new kind fails it
/// until listed here.
pub fn every_kind() -> Vec<PredicateArgKind> {
    vec![
        PredicateArgKind::Subject,
        PredicateArgKind::Decimal,
        PredicateArgKind::Date,
        PredicateArgKind::Timestamp,
        PredicateArgKind::Duration,
        PredicateArgKind::Bool,
        PredicateArgKind::Collection,
        PredicateArgKind::Quantity("USD".into()),
        PredicateArgKind::CalendarSpan,
        PredicateArgKind::Any,
    ]
}

/// Values of one kind for the conformance tests.
#[derive(Debug, Clone)]
pub struct KindSamples {
    /// Values the kernel holds distinct from each other.
    pub distinct: Vec<EvalValue>,
    /// Pairs written differently that the kernel holds equal, so a key
    /// that compares the writing instead of the value is caught.
    pub equal_pairs: Vec<(EvalValue, EvalValue)>,
    /// For a kind no declaration may carry, a value every storage route
    /// must refuse. `None` for a declarable kind.
    pub refused: Option<EvalValue>,
}

/// The samples of `kind`. Exhaustive, so a new kind must have them.
pub fn kind_samples(kind: &PredicateArgKind) -> KindSamples {
    let declarable =
        |distinct: Vec<EvalValue>, equal_pairs: Vec<(EvalValue, EvalValue)>| KindSamples {
            distinct,
            equal_pairs,
            refused: None,
        };
    match kind {
        PredicateArgKind::Subject => {
            declarable(vec![subj("a"), subj("b"), subj("a b"), subj("é")], vec![])
        }
        PredicateArgKind::Decimal => declarable(
            vec![dec(0), dec(1), dec(-1), dec_str("1.5"), dec(100)],
            vec![
                (dec_str("1.0"), dec(1)),
                (dec_str("100.00"), dec(100)),
                (dec_str("-0.0"), dec(0)),
            ],
        ),
        PredicateArgKind::Date => declarable(vec![date("2026-01-01"), date("2026-12-31")], vec![]),
        PredicateArgKind::Timestamp => declarable(
            vec![ts("2026-01-01T00:00:00Z"), ts("2026-01-01T00:00:00.5Z")],
            vec![(ts("2026-01-01T12:00:00+01:00"), ts("2026-01-01T11:00:00Z"))],
        ),
        PredicateArgKind::Duration => declarable(
            vec![dur("PT6H"), dur("-PT1H"), dur("PT0.5S"), dur("PT0S")],
            vec![(dur("PT90M"), dur("PT1H30M"))],
        ),
        PredicateArgKind::Bool => declarable(vec![bool_(true), bool_(false)], vec![]),
        PredicateArgKind::Collection => declarable(
            vec![
                coll(vec![subj("a"), dec(1)]),
                coll(vec![dec(1), subj("a")]),
                coll(vec![]),
            ],
            vec![(coll(vec![dec_str("1.0")]), coll(vec![dec(1)]))],
        ),
        PredicateArgKind::Quantity(unit) => declarable(
            vec![
                qty("5", unit.as_str()),
                qty("2.5", unit.as_str()),
                qty("5", "EUR"),
            ],
            vec![(qty("5.0", unit.as_str()), qty("5", unit.as_str()))],
        ),
        PredicateArgKind::CalendarSpan => KindSamples {
            distinct: vec![],
            equal_pairs: vec![],
            refused: Some(cal_span("P1M")),
        },
        PredicateArgKind::Any => declarable(vec![subj("x"), dec(1), bool_(true)], vec![]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The variant names serde's derive gives `T`: it passes them to
    /// `deserialize_enum`, which this deserializer records and refuses.
    fn variant_names<T: for<'de> serde::Deserialize<'de>>() -> Vec<&'static str> {
        use serde::de::{Error, Visitor, value};
        struct Capture(std::cell::Cell<&'static [&'static str]>);
        impl<'de> serde::Deserializer<'de> for &Capture {
            type Error = value::Error;
            fn deserialize_any<V: Visitor<'de>>(self, _: V) -> Result<V::Value, value::Error> {
                Err(value::Error::custom("not an enum"))
            }
            fn deserialize_enum<V: Visitor<'de>>(
                self,
                _: &'static str,
                variants: &'static [&'static str],
                _: V,
            ) -> Result<V::Value, value::Error> {
                self.0.set(variants);
                Err(value::Error::custom("recorded"))
            }
            serde::forward_to_deserialize_any! {
                bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
                bytes byte_buf option unit unit_struct newtype_struct seq tuple
                tuple_struct map struct identifier ignored_any
            }
        }
        let capture = Capture(std::cell::Cell::new(&[]));
        assert!(T::deserialize(&capture).is_err());
        capture.0.get().to_vec()
    }

    #[test]
    fn every_kind_names_each_variant_once() {
        let mut listed: Vec<String> = every_kind()
            .iter()
            .map(|kind| match serde_json::to_value(kind).unwrap() {
                serde_json::Value::String(name) => name,
                serde_json::Value::Object(one) => one.keys().next().unwrap().clone(),
                other => panic!("{other}"),
            })
            .collect();
        listed.sort();
        let mut variants: Vec<String> = variant_names::<PredicateArgKind>()
            .into_iter()
            .map(String::from)
            .collect();
        variants.sort();
        assert_eq!(listed, variants, "every_kind lists each kind exactly once");
    }

    #[test]
    fn a_kind_has_samples_exactly_when_it_is_declarable() {
        for kind in every_kind() {
            let samples = kind_samples(&kind);
            assert_eq!(
                !samples.distinct.is_empty(),
                kind.is_declarable(),
                "{kind}: samples, or a refused value, as its declarability says"
            );
            assert_eq!(samples.refused.is_some(), !kind.is_declarable(), "{kind}");
            for (a, b) in &samples.equal_pairs {
                assert_eq!(a, b, "{kind}: an equal pair the kernel holds equal");
            }
        }
    }
}
