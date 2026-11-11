//! The digest's equalities: two spellings of one stored value share a
//! digest, two values of one text and different kinds do not, and order
//! counts. The bytes themselves are pinned case by case in the examples
//! crate's `digest_golden` harness, in the golden the Python client's
//! test reads too.

use crate::digest::{digest, preimage};
use crate::ir::Subject;
use crate::state::EvalValue;
use rust_decimal::Decimal;
use std::str::FromStr;

fn s(x: &str) -> EvalValue {
    EvalValue::Subject(Subject::from(x))
}

fn d(x: &str) -> EvalValue {
    EvalValue::Decimal(Decimal::from_str(x).unwrap())
}

fn t(x: &str) -> EvalValue {
    EvalValue::Timestamp(x.parse().unwrap())
}

/// Two spellings of one stored value share a digest; two values of one
/// text and different kinds do not; order counts.
#[test]
fn the_digest_follows_the_stored_values_equality() {
    assert_eq!(digest(&[d("30")]), digest(&[d("30.0")]));
    assert_eq!(digest(&[d("30")]), digest(&[d("30.00")]));
    assert_eq!(digest(&[d("0")]), digest(&[d("-0.0")]));
    assert_eq!(preimage(&[d("-0.0")]), r#"[["decimal", "0"]]"#);
    assert_eq!(
        digest(&[t("2026-06-01T12:00:00Z")]),
        digest(&[t("2026-06-01T13:00:00+01:00")])
    );
    assert_ne!(digest(&[EvalValue::Bool(true)]), digest(&[s("true")]));
    assert_ne!(digest(&[d("1")]), digest(&[s("1")]));
    assert_ne!(digest(&[s("a"), s("b")]), digest(&[s("b"), s("a")]));
    assert_ne!(digest(&[s("ab"), s("c")]), digest(&[s("a"), s("bc")]));
    assert_eq!(
        preimage(&[s("r1"), d("30.0"), EvalValue::Bool(true)]),
        r#"[["subject", "r1"], ["decimal", "30"], ["bool", true]]"#
    );
    let out = digest(&[s("r1")]);
    assert!(out.starts_with("sha256:") && out.len() == 7 + 64, "{out}");
}

/// Hand-built IR with no argument is refused by the arity authority,
/// which the parser never reaches.
#[test]
fn a_digest_over_nothing_is_refused_by_arity() {
    use crate::ir::{Arity, Builtin};
    use crate::ir_builder::*;
    use crate::validate::ValidationError;
    let p = program("empty_digest")
        .predicates(vec![predicate("Seen").subject("x").build()])
        .transformations(vec![transformation(
            "see",
            params(&["x"]),
            vec![
                require(eq(call(Builtin::Digest, vec![]), term(var("x")))),
                assert_("Seen", vec![var("x")]),
            ],
        )])
        .build();
    let errors = p.validate().unwrap_err();
    assert!(
        errors.iter().any(|e| matches!(
            e,
            ValidationError::BuiltinArity {
                builtin: "digest",
                expected: Arity::AtLeast(1),
                found: 0,
                ..
            }
        )),
        "{errors:?}"
    );
}
