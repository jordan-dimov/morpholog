//! A transformation that declares a parameter's kind: the contract reports
//! the declaration, the checker holds the body to it, the kernel holds the
//! argument to it, and a declaration never says less than the body shows.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

mod common;

use common::{dec, subj};
use morpholog_core::{
    EvalError, ParamKind, PredicateArgKind, Program, State, ValidationError,
    transformation_param_kinds,
};
use morpholog_surface::parse_program;
use morpholog_test_support::{must_accept, propose_with_test_actor, validated};

const CLOSING: &str = r#"
program closing

predicate Order(order: Subject)
predicate Closed(order: Subject)
predicate Tagged(order: Subject, tag: Any)

transformation open(order):
    admit Order(order)

transformation close(order, force: Bool):
    require known: Order(order)
    require force = true or Closed(order)
    admit Closed(order)

transformation close_loud(order, force):
    require known: Order(order)
    require force = true or Closed(order)
    admit Closed(order)

transformation tag(order, tag: Any):
    require known: Order(order)
    admit Tagged(order, tag)

transformation tag_quiet(order, tag: Any):
    require known: Order(order)
    admit Closed(order)

transformation tag_subject(order, tag: Any):
    require known: Order(order)
    admit Tagged(order, tag)
    admit Closed(tag)

transformation close_all(orders: Collection):
    for o in orders:
        require known: Order(o)
        admit Closed(o)
"#;

fn parsed(source: &str) -> Program {
    let program = parse_program(source).expect("parses");
    program.validate().expect("validates");
    program
}

fn contract(program: &Program, name: &str) -> Vec<(String, ParamKind)> {
    transformation_param_kinds(&validated(program), &name.into())
        .unwrap()
        .into_iter()
        .map(|(v, k)| (v.to_string(), k))
        .collect()
}

/// A flag tested only by equality has no kind of its own; declared, it
/// has one for the schema and the client.
#[test]
fn a_declared_kind_is_the_contract_where_the_body_gives_none() {
    let p = parsed(CLOSING);
    assert_eq!(
        contract(&p, "close")[1],
        (
            "force".to_string(),
            ParamKind::Concrete(PredicateArgKind::Bool)
        )
    );
    assert_eq!(
        contract(&p, "close_loud")[1],
        ("force".to_string(), ParamKind::Unconstrained)
    );
}

/// `Any` declared is the parameter at an `Any` slot: polymorphic with a
/// silent body, concrete when the body uses it as one kind.
#[test]
fn any_declared_never_says_less_than_the_body_shows() {
    let p = parsed(CLOSING);
    assert_eq!(contract(&p, "tag")[1].1, ParamKind::Polymorphic);
    assert_eq!(contract(&p, "tag_quiet")[1].1, ParamKind::Polymorphic);
    assert_eq!(
        contract(&p, "tag_subject")[1].1,
        ParamKind::Concrete(PredicateArgKind::Subject)
    );
}

/// `Collection` declared keeps the element kind the body infers.
#[test]
fn collection_declared_keeps_the_elements_the_body_infers() {
    let p = parsed(CLOSING);
    assert_eq!(
        contract(&p, "close_all")[0].1,
        ParamKind::Collection(Box::new(ParamKind::Concrete(PredicateArgKind::Subject)))
    );
}

/// A use at another kind contradicts the declaration, by name.
#[test]
fn a_use_against_the_declaration_is_refused() {
    let errors = parse_program(&CLOSING.replace(
        "    require force = true or Closed(order)\n    admit Closed(order)\n\ntransformation close_loud",
        "    require force = true or Closed(order)\n    admit Closed(force)\n\ntransformation close_loud",
    ))
    .unwrap()
    .validate()
    .unwrap_err();
    assert!(
        errors.iter().any(|e| matches!(
            e,
            ValidationError::VariableKindConflict { variable, .. } if variable == "force"
        )),
        "{errors:?}"
    );
}

/// Hand-built IR whose declared kinds do not line up with its parameters
/// is refused by name.
#[test]
fn misaligned_declared_kinds_are_refused() {
    let mut p = parse_program(CLOSING).unwrap();
    let close = p
        .transformations
        .iter()
        .position(|t| t.name.as_str() == "close")
        .unwrap();
    p.transformations[close].declared_kinds.pop();
    let errors = p.validate().unwrap_err();
    assert!(
        errors.iter().any(|e| matches!(
            e,
            ValidationError::DeclaredKindsMisaligned { transformation, parameters: 2, declared: 1 }
                if transformation == "close"
        )),
        "{errors:?}"
    );
}

/// The kernel holds an argument to its declared kind before the body
/// runs: a decimal where a Bool is declared is an error that stages
/// nothing, where the undeclared act reaches its gate.
#[test]
fn an_argument_of_another_kind_is_an_error_before_the_body_runs() {
    let p = parsed(CLOSING);
    let state = must_accept(
        p.transformation("open").unwrap(),
        vec![subj("o1")],
        State::default(),
        &p,
    );
    let outcome = propose_with_test_actor(
        p.transformation("close").unwrap(),
        vec![subj("o1"), dec(1)],
        &state,
        &p,
    );
    assert!(
        matches!(
            &outcome,
            Err(EvalError::ArgumentKindMismatch { transformation, parameter, declared: PredicateArgKind::Bool, found: PredicateArgKind::Decimal })
                if transformation == "close" && parameter == "force"
        ),
        "{outcome:?}"
    );
    let undeclared = propose_with_test_actor(
        p.transformation("close_loud").unwrap(),
        vec![subj("o1"), dec(1)],
        &state,
        &p,
    );
    assert!(
        matches!(undeclared, Ok(morpholog_core::Outcome::Rejected { .. })),
        "the undeclared act reaches its gate and is refused there: {undeclared:?}"
    );
}
