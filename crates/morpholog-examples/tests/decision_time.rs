//! Where `decided_at` may appear. It reads the decision time the runtime
//! gives a transformation body, so it is refused wherever nothing gives
//! one (an invariant, a derived claim, a definition), as a name a
//! programme binds, and in a pattern matched against stored claims. A
//! programme that used the name for its own variable fails `check`; it is
//! never silently rebound.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::{Program, ValidationError};
use morpholog_surface::parse_program;

const HEAD: &str = "\
program decision_time_rules
predicate Lot(lot: Subject, closes: Timestamp)
predicate Bid(lot: Subject, at: Timestamp)
predicate Opened(lot: Subject, on: Date)
intent BidPlaced(lot: Subject, at: Timestamp)
";

fn program(rest: &str) -> Program {
    parse_program(&format!("{HEAD}{rest}")).expect("parses")
}

fn errors(rest: &str) -> Vec<ValidationError> {
    program(rest).validate().err().unwrap_or_default()
}

#[test]
fn a_body_may_compare_admit_emit_and_compute_with_it() {
    let p = program(
        "\
transformation bid(lot):
    bind Lot(lot, closes)
    require decided_at strictly_before closes
    let soon = (decided_at + duration(PT1H))
    require soon at_or_before closes
    admit Bid(lot, decided_at)
    emit BidPlaced(lot, decided_at)
",
    );
    assert_eq!(p.validate().err(), None);
}

#[test]
fn nothing_but_a_transformation_body_can_read_it() {
    for rest in [
        "invariant no_clock:\n    Bid(l, at) implies at at_or_before decided_at\n",
        "define late(l):\n    Lot(l, c) and c strictly_before decided_at\n",
        "predicate Left(lot: Subject, left: Duration)\nderived Left(lot):\n    over Lot(lot, c)\n    value left = c - decided_at\n",
    ] {
        let found = errors(rest);
        assert!(
            found
                .iter()
                .any(|e| matches!(e, ValidationError::DecisionTimeNotAvailable { .. })),
            "refused outside a body: {rest}\n{found:?}"
        );
    }
}

#[test]
fn no_binding_may_take_the_name() {
    for rest in [
        "transformation t(decided_at):\n    admit Bid(decided_at, decided_at)\n",
        "transformation t(lot):\n    let decided_at = (1)\n    admit Bid(lot, decided_at)\n",
        "transformation t(lots):\n    for decided_at in lots:\n        admit Bid(decided_at, decided_at)\n",
        "transformation t(lot):\n    require exists decided_at: Lot(lot, _)\n    admit Bid(lot, decided_at)\n",
        "define d(decided_at):\n    Lot(_, decided_at)\n",
    ] {
        let found = errors(rest);
        assert!(
            found
                .iter()
                .any(|e| matches!(e, ValidationError::DecisionTimeAsName { .. })),
            "refused as a name: {rest}\n{found:?}"
        );
    }
}

#[test]
fn it_is_never_matched_against_stored_claims() {
    for rest in [
        "transformation t(lot):\n    require not Bid(lot, decided_at)\n    admit Bid(lot, decided_at)\n",
        "transformation t(lot):\n    bind Lot(lot, decided_at)\n    admit Bid(lot, decided_at)\n",
        "transformation t(lot):\n    retract Bid(lot, decided_at)\n",
    ] {
        let found = errors(rest);
        assert!(
            found
                .iter()
                .any(|e| matches!(e, ValidationError::DecisionTimeMatched { .. })),
            "refused in a pattern: {rest}\n{found:?}"
        );
    }
}

#[test]
fn it_is_an_instant_not_a_day() {
    let found = errors(
        "transformation t(lot):\n    bind Opened(lot, on)\n    require on at_or_before decided_at\n    admit Bid(lot, decided_at)\n",
    );
    assert!(
        !found.is_empty(),
        "a date compared with an instant is a kind error"
    );
}

#[test]
fn the_parser_refuses_it_as_a_const_or_a_body_let() {
    for source in [
        format!("{HEAD}const decided_at = (1)\n"),
        format!("{HEAD}const later = (decided_at)\n"),
        format!(
            "{HEAD}invariant i:\n    let decided_at = (1)\n    Bid(l, at) implies at at_or_before at\n"
        ),
    ] {
        let diagnostics = parse_program(&source).expect_err("refused at parse");
        assert!(
            diagnostics.iter().any(|d| d.message.contains("decided_at")),
            "the refusal names the keyword: {diagnostics:?}"
        );
    }
}
