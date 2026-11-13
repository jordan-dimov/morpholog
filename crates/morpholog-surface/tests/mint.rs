//! An inline `new Subject()` in an `admit` or `emit` argument: the short form and its two-line
//! twin give the same `Program` and the same hash; a mint anywhere else is refused by name.

#![expect(
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

use morpholog_core::format::{canonical_hash, format_program};
use morpholog_core::{Stmt, Term};
use morpholog_surface::parse_program;

fn header(rest: &str) -> String {
    format!(
        "program mint_test\n\n\
         predicate Item(item: Subject, part: Subject)\n\
         predicate Child(child: Subject, parent: Subject)\n\
         intent Made(item: Subject)\n\n{rest}"
    )
}

fn assert_equivalent(short: &str, long: &str) {
    let p1 = parse_program(&header(short)).expect("the short form parses");
    let p2 = parse_program(&header(long)).expect("the long form parses");
    assert_eq!(p1, p2, "the short form and its two-line twin are one IR");
    assert_eq!(canonical_hash(&p1), canonical_hash(&p2));
}

fn refusal_containing(source: &str, needle: &str) {
    let errs = parse_program(&header(source)).expect_err("the source is refused");
    assert_eq!(errs.len(), 1, "one diagnostic: {errs:?}");
    assert!(
        errs[0].message.contains(needle),
        "expected a diagnostic containing {needle:?}, got: {:?}",
        errs[0].message
    );
}

#[test]
fn a_mint_in_an_admit_is_a_let_of_a_generated_name_before_it() {
    assert_equivalent(
        "transformation make(part):\n    admit Item(new Subject(), part)\n",
        "transformation make(part):\n    let _subject_1 = new Subject()\n    admit Item(_subject_1, part)\n",
    );
}

#[test]
fn two_mints_in_one_statement_are_two_lets_in_slot_order() {
    assert_equivalent(
        "transformation split(part):\n    admit Child(new Subject(), new Subject())\n",
        "transformation split(part):\n    let _subject_1 = new Subject()\n    let _subject_2 = new Subject()\n    admit Child(_subject_1, _subject_2)\n",
    );
}

#[test]
fn a_mint_in_an_emit_and_in_a_named_pattern_works_the_same() {
    assert_equivalent(
        "transformation make(part):\n    emit Made(new Subject())\n",
        "transformation make(part):\n    let _subject_1 = new Subject()\n    emit Made(_subject_1)\n",
    );
    assert_equivalent(
        "transformation make(part):\n    admit Item(part: part, item: new Subject())\n",
        "transformation make(part):\n    let _subject_1 = new Subject()\n    admit Item(_subject_1, part)\n",
    );
}

#[test]
fn the_names_count_on_across_the_body_and_skip_a_taken_one() {
    assert_equivalent(
        "transformation make(part, _subject_1):\n    admit Item(new Subject(), part)\n    admit Item(new Subject(), _subject_1)\n",
        "transformation make(part, _subject_1):\n    let _subject_2 = new Subject()\n    admit Item(_subject_2, part)\n    let _subject_3 = new Subject()\n    admit Item(_subject_3, _subject_1)\n",
    );
}

#[test]
fn a_generated_name_skips_one_the_transformation_has_anywhere() {
    // A parameter nothing reads.
    assert_equivalent(
        "transformation make(part, _subject_1):\n    admit Item(new Subject(), part)\n",
        "transformation make(part, _subject_1):\n    let _subject_2 = new Subject()\n    admit Item(_subject_2, part)\n",
    );
    // A `let` destination nothing reads.
    assert_equivalent(
        "transformation make(part):\n    let _subject_1 = 42\n    admit Item(new Subject(), part)\n",
        "transformation make(part):\n    let _subject_1 = 42\n    let _subject_2 = new Subject()\n    admit Item(_subject_2, part)\n",
    );
    // A `for` binder nothing reads, taken outside the body as well as inside it.
    assert_equivalent(
        "transformation make(part, parts):\n    for _subject_1 in parts:\n        admit Item(new Subject(), part)\n    admit Item(new Subject(), part)\n",
        "transformation make(part, parts):\n    for _subject_1 in parts:\n        let _subject_2 = new Subject()\n        admit Item(_subject_2, part)\n    let _subject_3 = new Subject()\n    admit Item(_subject_3, part)\n",
    );
    // A quantifier binder nothing reads.
    assert_equivalent(
        "transformation make(part):\n    require exists _subject_1: Item(_, part)\n    admit Item(new Subject(), part)\n",
        "transformation make(part):\n    require exists _subject_1: Item(_, part)\n    let _subject_2 = new Subject()\n    admit Item(_subject_2, part)\n",
    );
}

#[test]
fn mints_in_named_arguments_draw_in_slot_order_not_written_order() {
    // `Child` declares `child` before `parent`, so the child slot's mint is the first draw.
    assert_equivalent(
        "transformation split(part):\n    admit Child(parent: new Subject(), child: new Subject())\n",
        "transformation split(part):\n    let _subject_1 = new Subject()\n    let _subject_2 = new Subject()\n    admit Child(_subject_1, _subject_2)\n",
    );
}

#[test]
fn a_mint_inside_a_for_body_is_a_let_inside_the_body() {
    assert_equivalent(
        "transformation make_all(parts):\n    for p in parts:\n        admit Item(new Subject(), p)\n",
        "transformation make_all(parts):\n    for p in parts:\n        let _subject_1 = new Subject()\n        admit Item(_subject_1, p)\n",
    );
    let p = parse_program(&header(
        "transformation make_all(parts):\n    for p in parts:\n        admit Item(new Subject(), p)\n",
    ))
    .unwrap();
    let Stmt::For { body, .. } = &p.transformations[0].body[0] else {
        panic!("a for");
    };
    assert!(matches!(&body[0], Stmt::LetNewSubject { name } if name.as_str() == "_subject_1"));
    assert!(matches!(&body[1], Stmt::Assert(c) if c.args[0] == Term::Var("_subject_1".into())));
}

#[test]
fn the_short_form_formats_as_its_long_form_and_reparses_to_itself() {
    let short = header("transformation make(part):\n    admit Item(new Subject(), part)\n");
    let p = parse_program(&short).unwrap();
    let formatted = format_program(&p);
    assert!(
        formatted.contains("  let _subject_1 = new Subject()\n  admit Item(_subject_1, part)\n"),
        "{formatted}"
    );
    assert_eq!(parse_program(&formatted).unwrap(), p);
}

#[test]
fn a_mint_where_a_subject_is_read_is_refused_by_name() {
    for source in [
        "transformation take(part):\n    bind Item(new Subject(), part)\n    admit Item(part, part)\n",
        "transformation take(part):\n    retract Item(new Subject(), part)\n",
        "transformation take(part):\n    require Item(new Subject(), part)\n    admit Item(part, part)\n",
        "transformation take(part):\n    require exists i: Item(i, new Subject())\n    admit Item(part, part)\n",
        "transformation take(part):\n    require part = new Subject()\n    admit Item(part, part)\n",
        "transformation take(part):\n    let same = (new Subject())\n    admit Item(part, part)\n",
        "invariant parented: forall c in Child(c, _): Item(new Subject(), c)\n",
    ] {
        refusal_containing(source, "mints a subject only where one is written down");
    }
}

#[test]
fn the_two_line_form_still_parses_as_it_did() {
    let p = parse_program(&header(
        "transformation make(part):\n    let item = new Subject()\n    admit Item(item, part)\n",
    ))
    .unwrap();
    assert!(
        matches!(&p.transformations[0].body[0], Stmt::LetNewSubject { name } if name.as_str() == "item")
    );
}
