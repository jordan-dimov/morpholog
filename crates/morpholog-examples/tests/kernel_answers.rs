//! Answers the kernel gives an embedder directly, which no decision of its
//! own reads, so only a test holds them (found by the mutation census,
//! #359): whether two states differ, whether a state is
//! empty, and whether a reported comparison held.

use morpholog_core::{Compared, EvalValue, State};
use morpholog_test_support::{claim_instance, date, dec, dur, qty, subj, ts};

#[test]
fn states_with_other_claims_are_not_equal() {
    let a = claim_instance("Flag", &[subj("a")]);
    let b = claim_instance("Flag", &[subj("b")]);
    assert_eq!(
        State::from_claims(vec![a.clone(), b.clone()]),
        State::from_claims(vec![a.clone(), b.clone()])
    );
    assert_ne!(
        State::from_claims(vec![a.clone()]),
        State::from_claims(vec![b])
    );
    assert_ne!(State::from_claims(vec![a]), State::from_claims(vec![]));
}

#[test]
fn a_state_is_empty_exactly_when_it_holds_no_claim() {
    let empty = State::from_claims(vec![]);
    let one = State::from_claims(vec![claim_instance("Flag", &[subj("a")])]);
    assert!(empty.is_empty() && empty.claims().is_empty());
    assert!(!one.is_empty() && !one.claims().is_empty());
}

#[test]
fn a_reported_comparison_says_whether_it_held() {
    let compared = |op: &str, left: EvalValue, right: EvalValue| {
        Compared {
            op: op.to_owned(),
            left,
            right,
        }
        .holds()
    };
    for (op, left, right, held) in [
        ("=", dec(1), dec(1), true),
        ("=", dec(1), dec(2), false),
        ("!=", dec(1), dec(2), true),
        ("!=", dec(1), dec(1), false),
        ("<=", dec(1), dec(1), true),
        ("<=", dec(2), dec(1), false),
        ("<", dec(1), dec(2), true),
        ("<", dec(1), dec(1), false),
        (">=", dec(1), dec(1), true),
        (">=", dec(1), dec(2), false),
        (">", dec(2), dec(1), true),
        (">", dec(1), dec(1), false),
        ("<", qty("1", "kg"), qty("2", "kg"), true),
        ("before", date("2026-01-01"), date("2026-01-02"), true),
        (
            "strictly_after",
            ts("2026-01-01T00:00:00Z"),
            ts("2026-01-01T00:00:01Z"),
            false,
        ),
        ("shorter_than", dur("PT1H"), dur("PT2H"), true),
    ] {
        assert_eq!(
            compared(op, left.clone(), right.clone()),
            Some(held),
            "{left:?} {op} {right:?}"
        );
    }
    // Operands it cannot order, and an operator it does not know.
    for (op, left, right) in [
        ("<", qty("1", "kg"), qty("1", "t")),
        ("<", dec(1), date("2026-01-01")),
        ("<", subj("a"), subj("b")),
        ("~", dec(1), dec(1)),
    ] {
        assert_eq!(
            compared(op, left.clone(), right.clone()),
            None,
            "{left:?} {op} {right:?}"
        );
    }
}
