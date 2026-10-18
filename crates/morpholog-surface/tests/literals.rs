//! A literal is read once, at the source frontend, into a value. Two
//! spellings of one value are one programme, with one hash; two values
//! are two. A canonical spelling renders back byte for byte, and a
//! malformed literal is a diagnostic at its span, never a value.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::format::{canonical_hash, format_program};
use morpholog_core::{Program, State};
use morpholog_surface::parse_program;

fn programme(body: &str) -> Program {
    let source = format!("program p\npredicate P(x: Decimal)\n\ninvariant i:\n  {body}\n");
    parse_program(&source).unwrap_or_else(|e| panic!("`{body}` should parse: {e:?}"))
}

/// Representation against meaning: whether two spellings are the same
/// typed IR, and so the same hash, is decided by the value, not the text.
/// Decimal scale is part of a literal's identity, since a figure computed
/// from it keeps that scale.
#[test]
fn two_spellings_of_one_value_are_one_programme() {
    let cases = [
        ("span(P1Y) = span(P1Y)", "span(P12M) = span(P12M)", true),
        (
            "duration(PT90M) = duration(PT90M)",
            "duration(PT1H30M) = duration(PT1H30M)",
            true,
        ),
        (
            "@2026-10-24T14:00:00+01:00 = @2026-10-24T14:00:00+01:00",
            "@2026-10-24T13:00:00Z = @2026-10-24T13:00:00Z",
            true,
        ),
        ("1.50 = 1.50", "1.5 = 1.5", false),
        ("1.50 USD = 1.50 USD", "1.5 USD = 1.5 USD", false),
        ("span(P1M) = span(P1M)", "span(P30D) = span(P30D)", false),
    ];
    for (a, b, same) in cases {
        let (pa, pb) = (programme(a), programme(b));
        assert_eq!(pa == pb, same, "typed IR of `{a}` vs `{b}`");
        assert_eq!(
            canonical_hash(&pa) == canonical_hash(&pb),
            same,
            "hash of `{a}` vs `{b}`"
        );
    }
}

/// At runtime the kernel compares values, so the spellings that differ as
/// literals still compare equal where their values are equal.
#[test]
fn equivalent_values_decide_the_same_at_runtime() {
    for body in [
        "1.50 = 1.5",
        "1.50 USD = 1.5 USD",
        "duration(PT90M) = duration(PT1H30M)",
        "span(P1Y) = span(P12M)",
        "@2026-10-24T14:00:00+01:00 = @2026-10-24T13:00:00Z",
    ] {
        let p = programme(body);
        let holds = p
            .validated()
            .expect("validates")
            .eval_invariant("i", &State::from_claims(vec![]), None)
            .expect("evaluates");
        assert_eq!(holds, Some(true), "`{body}` should hold");
    }
}

/// A literal already in its canonical spelling renders back byte for byte.
#[test]
fn canonical_spellings_render_verbatim() {
    for literal in [
        "1.50",
        "0.00",
        "12.5 USD",
        "@2026-04-01",
        "@2026-10-24T14:00:00Z",
        "duration(PT1H30M)",
        "duration(PT0S)",
        "span(P12M)",
        "span(P45D)",
    ] {
        let rendered = format_program(&programme(&format!("{literal} = {literal}")));
        assert!(
            rendered.contains(&format!("{literal} = {literal}")),
            "`{literal}` should render verbatim, got:\n{rendered}"
        );
    }
}

/// A non-canonical spelling renders in the canonical one.
#[test]
fn other_spellings_render_canonically() {
    for (written, rendered_as) in [
        ("span(P1Y)", "span(P12M)"),
        ("duration(PT90M)", "duration(PT1H30M)"),
        ("@2026-10-24T14:00:00+01:00", "@2026-10-24T13:00:00Z"),
    ] {
        let rendered = format_program(&programme(&format!("{written} = {written}")));
        assert!(
            rendered.contains(&format!("{rendered_as} = {rendered_as}")),
            "`{written}` should render as `{rendered_as}`, got:\n{rendered}"
        );
    }
}

/// A literal the type cannot hold exactly is a diagnostic at parse time,
/// naming the text, where it once lexed and failed later or never
/// matched. Exactly or not at all: a decimal is never rounded in.
#[test]
fn a_literal_the_value_cannot_hold_is_a_parse_diagnostic() {
    for (body, text) in [
        ("@2026-02-30 = @2026-02-30", "2026-02-30"),
        (
            "1.000000000000000000000000000001 = 1",
            "1.000000000000000000000000000001",
        ),
        (
            "100000000000000000000000000000 = 1",
            "100000000000000000000000000000",
        ),
    ] {
        let source = format!("program p\npredicate P(x: Decimal)\n\ninvariant i:\n  {body}\n");
        let err = parse_program(&source).expect_err(body);
        let msg = format!("{err:?}");
        assert!(msg.contains(text), "diagnostic should name `{text}`: {msg}");
    }
}
