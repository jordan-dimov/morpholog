//! Every value kind, and values of each, for the conformance tests that
//! hold each surface a value crosses to the same kinds: the tagged and
//! named wire, the key the database compares by, the view column and the
//! generated client. A new kind does not compile until [`kind_samples`]
//! has its samples; it then belongs in [`every_kind`], and each surface's
//! test fails until it handles the kind or refuses it by name.

use morpholog_core::{EvalValue, PredicateArgKind};

use crate::{bool_, cal_span, coll, date, dec, dec_str, dur, qty, subj, ts};

/// One representative of every kind. A new kind is listed here when it
/// gets its arm in [`kind_samples`], which the compiler asks for: no test
/// can see a variant missing from a list.
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
