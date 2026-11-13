//! `digest(a, b, ...)` at the surface: it parses and round-trips with any
//! number of arguments from one; its result is a subject, so ordering it
//! is a kind error; a collection or a calendar span is refused by name;
//! a definition cannot take its name.

use morpholog_core::ValidationError;
use morpholog_core::format::format_program;
use morpholog_surface::parse_program;

const DECLS: &str = "program sealed
predicate Sealed(lot: Subject, bidder: Subject, commitment: Subject)
predicate Bid(lot: Subject, bidder: Subject, amount: Decimal, salt: Subject)
";

fn programme(rest: &str) -> Result<morpholog_core::Program, Vec<ValidationError>> {
    let source = format!("{DECLS}{rest}");
    let p = parse_program(&source).unwrap_or_else(|e| panic!("should parse: {e:?}\n{source}"));
    p.validate().map(|()| p)
}

#[test]
fn a_digest_in_a_rule_and_in_a_gate_validates_and_round_trips() {
    let p = programme(
        "invariant a_bid_hashes_to_its_commitment:
    Bid(lot, bidder, amount, salt) and Sealed(lot, bidder, commitment)
    implies digest(#sealed_bid_v1, lot, bidder, amount, salt) = commitment
transformation reveal(lot, amount, salt, commitment):
    require matches: digest(lot) != commitment and digest(amount, salt) = commitment
    admit Bid(lot, actor, amount, salt)
",
    )
    .expect("validates");
    assert_eq!(parse_program(&format_program(&p)).unwrap(), p);
}

#[test]
fn a_digest_takes_at_least_one_argument() {
    let source = format!(
        "{DECLS}transformation reveal(lot, commitment):\n    require matches: digest() = commitment\n    admit Sealed(lot, actor, commitment)\n"
    );
    assert!(parse_program(&source).is_err(), "`digest()` does not parse");
}

#[test]
fn a_digest_is_a_subject_so_ordering_it_is_refused() {
    let errs = programme(
        "transformation reveal(lot, commitment):
    require matches: digest(lot) <= commitment
    admit Sealed(lot, actor, commitment)
",
    )
    .expect_err("a subject has no order");
    assert!(!errs.is_empty(), "{errs:?}");
}

#[test]
fn a_collection_or_a_calendar_span_is_refused_by_name() {
    let errs = programme(
        "transformation reveal(lot, lots: Collection, commitment):
    require matches: digest(lots) = commitment
    admit Sealed(lot, actor, commitment)
",
    )
    .expect_err("a collection has no stored key");
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::DigestArgumentKind { position: 1, .. })),
        "{errs:?}"
    );
    let errs = programme(
        "transformation reveal(lot, commitment):
    require matches: digest(span(P1M)) = commitment
    admit Sealed(lot, actor, commitment)
",
    )
    .expect_err("a calendar span is never stored");
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::DigestArgumentKind { .. })),
        "{errs:?}"
    );
}

#[test]
fn a_definition_cannot_be_named_digest() {
    let errs = programme(
        "define digest(x):
    Sealed(x, _, _)
",
    )
    .expect_err("the builtin's name is taken");
    assert!(
        errs.iter()
            .any(|e| matches!(e, ValidationError::DefinitionNameIsBuiltin { .. })),
        "{errs:?}"
    );
}
