//! Tests for the JSON Schema adapter over the param-kind analysis.
//! Pins the per-kind property shapes the external embedder will
//! generate forms / request models against; the worked-example
//! integration tests over trade_lifecycle live in
//! `morpholog-examples`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::ir_builder::*;
use morpholog_core::{AnalysisError, TransformationName, transformation_arg_schema};
use serde_json::json;

/// Concrete subject parameter renders as an opaque string with NO
/// `format: "uuid"`. Subject is Morpholog's only primitive noun
/// and naturally carries both minted entity identifiers (UUIDv7
/// by runtime convention) and domain symbols (commodity codes,
/// period names, direction enums). The schema describes the
/// shape; the convention lives in the description, not as a
/// validation rule. Embedders that want UUID enforcement for a
/// specific parameter layer their own constraint on top.
#[test]
fn concrete_subject_renders_as_opaque_string() {
    let prog = program("subject_test")
        .predicates(vec![predicate("touched").subject("subj").build()])
        .transformations(vec![transformation(
            "act",
            params(&["subj"]),
            vec![assert_("touched", vec![var("subj")])],
        )])
        .build();

    let schema = transformation_arg_schema(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("act"),
    )
    .unwrap();
    let property = &schema["properties"]["subj"];
    assert_eq!(property["type"], json!("string"));
    assert!(
        property.get("format").is_none(),
        "Subject must NOT carry format: uuid - the schema would over-constrain \
         symbolic subjects like commodity codes, period names, direction enums; \
         the convention belongs in the description, not as a JSON Schema rule",
    );
    let description = property["description"].as_str().unwrap();
    assert!(
        description.contains("opaque") && description.contains("UUIDv7"),
        "description must name BOTH the opaque-string contract AND the UUIDv7 \
         convention for minted subjects; got: {description}",
    );
}

/// Concrete decimal parameter renders as a string with a strict
/// numeric pattern. The exact pattern is pinned here as the
/// external contract; whether a candidate string matches is the
/// embedder's job to apply against its own runtime regex, not this
/// test's. Pinning the pattern literally is what protects against
/// silent drift if the encoding ever moves (e.g. relaxing to allow
/// leading zeros, or switching to JSON number).
#[test]
fn concrete_decimal_renders_as_string_with_pinned_pattern() {
    let prog = program("decimal_test")
        .predicates(vec![predicate("scored").decimal("score").build()])
        .transformations(vec![transformation(
            "record",
            params(&["score"]),
            vec![assert_("scored", vec![var("score")])],
        )])
        .build();

    let schema = transformation_arg_schema(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("record"),
    )
    .unwrap();
    let property = &schema["properties"]["score"];
    assert_eq!(property["type"], json!("string"));
    assert_eq!(
        property["pattern"],
        json!(r"^-?(0|[1-9]\d*)(\.\d+)?$"),
        "decimal pattern must reject leading zeros, bare `+`, trailing-dot, etc.",
    );
}

/// Concrete date parameter renders as an ISO-8601 civil date string.
#[test]
fn concrete_date_renders_as_iso_date_string() {
    let prog = program("date_test")
        .predicates(vec![predicate("happened").date("on").build()])
        .transformations(vec![transformation(
            "log",
            params(&["on"]),
            vec![assert_("happened", vec![var("on")])],
        )])
        .build();

    let schema = transformation_arg_schema(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("log"),
    )
    .unwrap();
    let property = &schema["properties"]["on"];
    assert_eq!(property["type"], json!("string"));
    assert_eq!(property["format"], json!("date"));
}

/// Concrete bool parameter renders as a plain boolean.
#[test]
fn concrete_bool_renders_as_boolean() {
    let prog = program("bool_test")
        .predicates(vec![predicate("flagged").boolean("flag").build()])
        .transformations(vec![transformation(
            "set",
            params(&["flag"]),
            vec![assert_("flagged", vec![var("flag")])],
        )])
        .build();

    let schema = transformation_arg_schema(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("set"),
    )
    .unwrap();
    assert_eq!(schema["properties"]["flag"]["type"], json!("boolean"));
}

/// Concrete collection parameter renders as an untyped array - v0
/// does not track collection item kinds.
#[test]
fn concrete_collection_renders_as_array() {
    let prog = program("coll_test")
        .predicates(vec![predicate("bagged").collection("items").build()])
        .transformations(vec![transformation(
            "pack",
            params(&["items"]),
            vec![assert_("bagged", vec![var("items")])],
        )])
        .build();

    let schema = transformation_arg_schema(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("pack"),
    )
    .unwrap();
    assert_eq!(schema["properties"]["items"]["type"], json!("array"));
}

/// Polymorphic parameter renders as a typeless property whose
/// description carries the polymorphism signal.
#[test]
fn polymorphic_renders_as_typeless_with_polymorphic_description() {
    let prog = program("poly_test")
        .predicates(vec![predicate("audit").any("payload").build()])
        .transformations(vec![transformation(
            "log",
            params(&["payload"]),
            vec![assert_("audit", vec![var("payload")])],
        )])
        .build();

    let schema = transformation_arg_schema(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("log"),
    )
    .unwrap();
    let property = &schema["properties"]["payload"];
    assert!(property.get("type").is_none(), "should omit type");
    assert!(
        property["description"]
            .as_str()
            .unwrap()
            .contains("polymorphic"),
    );
}

/// Unconstrained parameter renders as a typeless property whose
/// description names the modelling-smell signal.
#[test]
fn unconstrained_renders_as_typeless_with_unconstrained_description() {
    let prog = program("dead_test")
        .transformations(vec![transformation("noop", params(&["unused"]), vec![])])
        .build();

    let schema = transformation_arg_schema(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("noop"),
    )
    .unwrap();
    let property = &schema["properties"]["unused"];
    assert!(property.get("type").is_none(), "should omit type");
    assert!(
        property["description"]
            .as_str()
            .unwrap()
            .contains("unconstrained"),
    );
}

/// The top-level shape: $schema, title, type=object,
/// additionalProperties=false, required[] in declaration order. The
/// required-list order specifically pins the same declaration order
/// the analysis layer commits to.
#[test]
fn top_level_shape_carries_required_in_declaration_order() {
    let prog = program("ordering_test")
        .predicates(vec![
            predicate("triple")
                .subject("a")
                .decimal("b")
                .date("c")
                .build(),
        ])
        .transformations(vec![transformation(
            "act",
            params(&["zebra", "apple", "mango"]),
            vec![assert_(
                "triple",
                vec![var("zebra"), var("apple"), var("mango")],
            )],
        )])
        .build();

    let schema = transformation_arg_schema(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("act"),
    )
    .unwrap();
    assert_eq!(
        schema["$schema"],
        json!("https://json-schema.org/draft/2020-12/schema")
    );
    assert_eq!(schema["title"], json!("act"));
    assert_eq!(schema["type"], json!("object"));
    assert_eq!(schema["additionalProperties"], json!(false));
    let required: Vec<&str> = schema["required"]
        .as_array()
        .expect("required is array")
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(required, vec!["zebra", "apple", "mango"]);
}

/// A parameter projected as `Ambiguous` renders as `anyOf` over the
/// per-kind bare fragments (type / format / pattern only), with the
/// branch-local-observation signal carried by a property-level
/// `description`. The per-kind descriptions are deliberately
/// stripped from the alternatives - the embedder should not see
/// "opaque Morpholog subject identifier" as one of several options
/// when the parameter is not specifically a subject; the description
/// belongs at the property level, naming the ambiguity itself.
#[test]
fn ambiguous_renders_as_any_of_alternatives() {
    let prog = program("ambiguous_test")
        .predicates(vec![
            predicate("by_decimal").decimal("d").build(),
            predicate("by_subject").subject("s").build(),
        ])
        .transformations(vec![transformation(
            "either_shape",
            params(&["x"]),
            vec![require(or(vec![
                claim("by_decimal", vec![var("x")]),
                claim("by_subject", vec![var("x")]),
            ]))],
        )])
        .build();

    let schema = transformation_arg_schema(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("either_shape"),
    )
    .unwrap();
    let property = &schema["properties"]["x"];
    let alternatives = property["anyOf"].as_array().expect("anyOf present");
    let alt_types: Vec<&str> = alternatives
        .iter()
        .map(|a| a["type"].as_str().unwrap())
        .collect();
    assert_eq!(
        alt_types,
        vec!["string", "string"],
        "Subject and Decimal both render as JSON-Schema string",
    );
    assert!(
        alternatives[1].get("pattern").is_some(),
        "Decimal carries the pattern - the embedder still gets the strict \
         decimal shape on this anyOf branch",
    );
    assert!(
        alternatives[0].get("format").is_none() && alternatives[0].get("pattern").is_none(),
        "Subject carries only `type: string` - it is opaque, no format \
         constraint (the Subject contract covers domain symbols as well as \
         minted UUIDv7 identifiers); got: {:?}",
        alternatives[0],
    );
    for (i, alt) in alternatives.iter().enumerate() {
        assert!(
            alt.get("description").is_none(),
            "alternative {i} should not carry a per-kind description; \
             the property-level description names the ambiguity",
        );
    }
    assert!(
        property["description"]
            .as_str()
            .unwrap()
            .contains("ambiguous"),
    );
}

/// Unknown transformation bubbles through unchanged from the
/// analysis layer.
#[test]
fn unknown_transformation_bubbles_through() {
    let prog = program("bubble_unknown")
        .transformations(vec![transformation("declared", params(&[]), vec![])])
        .build();

    let err = transformation_arg_schema(
        &prog.validated().expect("test programme validates"),
        &TransformationName::from("ghost"),
    )
    .expect_err("expected UnknownTransformation");
    assert!(
        matches!(err, AnalysisError::UnknownTransformation { name } if name.as_str() == "ghost"),
    );
}

/// Invalid programme surfaces at the `Program::validated` gate,
/// before the schema layer is reachable. The schema function takes
/// a `ValidatedProgram`, which an invalid programme cannot
/// construct, so the validation precondition is enforced at the
/// type level instead of repeated at every accessor boundary.
#[test]
fn invalid_program_surfaces_at_the_validated_gate() {
    let prog = program("bubble_invalid")
        .transformations(vec![transformation(
            "broken",
            params(&[]),
            vec![assert_("undeclared", vec![])],
        )])
        .build();

    let errors = prog
        .validated()
        .expect_err("an invalid programme must not yield a ValidatedProgram");
    assert!(!errors.is_empty());
}
