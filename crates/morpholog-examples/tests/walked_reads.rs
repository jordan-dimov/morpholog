//! A condition that calls itself is read keyed only when it walks one
//! link up from an argument it knows: the loader then follows the link
//! through the database instead of reading the predicate whole. Every
//! other shape is read whole, which is always safe and only slower; a
//! shape taken for a walk when it is not one would load too few rows.
//! Found untested by the mutation census (#359).

use morpholog_core::{ReadFilter, ReadPlan};
use morpholog_surface::parse_program;

/// How `fill` reads `Link`, with `reaches` defined as `body`.
fn link_read(body: &str) -> ReadFilter {
    let source = format!(
        "program walked
predicate Link(from: Subject, to: Subject)
predicate Other(from: Subject, to: Subject)
predicate Link3(from: Subject, to: Subject, via: Subject)
predicate Filled(part: Subject)
define reaches(x, y):
    {body}
transformation fill(part, top):
    require reaches(part, top)
    admit Filled(part)
"
    );
    let program = parse_program(&source).expect("parses");
    let fill = program.transformation("fill").expect("transformation");
    let plan = ReadPlan::of(fill, &program.definitions);
    plan.reads
        .get(&"Link".into())
        .or_else(|| plan.reads.get(&"Link3".into()))
        .cloned()
        .expect("reads the link")
}

#[test]
fn a_walk_up_one_link_is_read_keyed_with_its_branches_in_either_order() {
    for body in [
        "Link(x, y) or (Link(x, next) and reaches(next, y))",
        "(Link(x, next) and reaches(next, y)) or Link(x, y)",
    ] {
        assert!(matches!(link_read(body), ReadFilter::Keyed(_)), "{body}");
    }
}

#[test]
fn every_other_shape_is_read_whole() {
    for (shape, body) in [
        (
            "a step over another predicate",
            "Link(x, y) or (Other(x, next) and reaches(next, y))",
        ),
        (
            "a link of three places",
            "Link3(x, y, v) or (Link3(x, next, v) and reaches(next, y))",
        ),
        (
            "a step that does not start from the walked argument",
            "Link(x, y) or (Link(next, x) and reaches(next, y))",
        ),
        (
            "a step to a parameter, not a fresh variable",
            "Link(x, y) or (Link(x, y) and reaches(y, y))",
        ),
        (
            "a call that does not walk to the step's end",
            "Link(x, y) or (Link(x, next) and reaches(x, y))",
        ),
    ] {
        assert_eq!(link_read(body), ReadFilter::Whole, "{shape}: {body}");
    }
}
