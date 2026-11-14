//! A programme in parts reads as the concatenation of its parts, refuses a boundary a construct
//! could cross, and places every span back in the file a reader sees.

#![expect(
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

use morpholog_core::format::canonical_hash;
use morpholog_surface::{SourceText, parse_program};
use std::path::PathBuf;

fn parts(texts: &[(&str, &str)]) -> SourceText {
    SourceText::parts(
        texts
            .iter()
            .map(|(n, t)| ((*n).to_string(), (*t).to_string())),
    )
    .expect("at least one part")
}

fn gallery() -> Vec<(PathBuf, String)> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let mut out = Vec::new();
    for dir in std::fs::read_dir(&root).expect("the gallery") {
        let dir = dir.expect("an entry").path();
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for file in files {
            let path = file.expect("an entry").path();
            if path.extension().is_some_and(|e| e == "morph") {
                let text = std::fs::read_to_string(&path).expect("readable");
                out.push((path, text));
            }
        }
    }
    out.sort();
    assert!(!out.is_empty(), "the gallery is found");
    out
}

/// Byte offsets where a top-level declaration starts, after the header.
fn declaration_starts(text: &str) -> Vec<usize> {
    const KEYWORDS: [&str; 7] = [
        "predicate ",
        "define ",
        "invariant ",
        "transformation ",
        "intent ",
        "derived ",
        "const ",
    ];
    let mut offset = 0;
    let mut out = Vec::new();
    for line in text.split_inclusive('\n') {
        if KEYWORDS.iter().any(|k| line.starts_with(k)) {
            out.push(offset);
        }
        offset += line.len();
    }
    out
}

fn split_at(text: &str, cuts: &[usize]) -> SourceText {
    let mut bounds = vec![0];
    bounds.extend(cuts.iter().copied().filter(|&c| c > 0));
    bounds.push(text.len());
    SourceText::parts(
        bounds
            .windows(2)
            .enumerate()
            .map(|(i, w)| (format!("part{i}.morph"), text[w[0]..w[1]].to_string())),
    )
    .expect("at least one part")
}

/// Every gallery programme, split into three parts and into one part per declaration, hashes
/// as the file it was cut from: constants, named patterns and definitions reach across parts,
/// and programme order is the order of the concatenated text.
#[test]
fn a_gallery_programme_in_parts_hashes_as_its_one_file() {
    for (path, text) in gallery() {
        let whole = canonical_hash(&parse_program(&text).expect("the gallery parses"));
        let starts = declaration_starts(&text);
        let thirds = [starts[starts.len() / 3], starts[2 * starts.len() / 3]];
        for (how, cuts) in [
            ("in thirds", &thirds[..]),
            ("one per declaration", &starts[..]),
        ] {
            let source = split_at(&text, cuts);
            let (program, _) = source
                .parse()
                .unwrap_or_else(|e| panic!("{} {how}: {:?}", path.display(), e));
            assert_eq!(canonical_hash(&program), whole, "{} {how}", path.display());
        }
    }
}

const HEADER: &str = "program shop\n";
const STOCK: &str = "predicate Stock(item: Subject, quantity: Decimal)\n";
const SHIPPED: &str = "predicate Shipped(item: Subject)\n";
const SHIP: &str = "transformation ship(item):\n    admit Shipped(item)\n";

fn hash(texts: &[(&str, &str)]) -> String {
    canonical_hash(&parts(texts).parse().expect("parses").0)
}

/// Reordering parts changes the hash exactly when it changes the order within a kind.
#[test]
fn a_reordered_manifest_changes_the_hash_exactly_when_a_kind_reorders() {
    let header = ("h.morph", HEADER);
    let (stock, shipped, ship) = (("a.morph", STOCK), ("b.morph", SHIPPED), ("c.morph", SHIP));
    assert_eq!(
        hash(&[header, stock, ship]),
        hash(&[header, ship, stock]),
        "a predicate and a transformation keep their own orders"
    );
    assert_ne!(
        hash(&[header, stock, shipped]),
        hash(&[header, shipped, stock]),
        "two predicates swap places"
    );
}

#[test]
fn an_error_in_a_part_is_placed_in_that_part() {
    let source = parts(&[
        ("h.morph", HEADER),
        ("stock.morph", STOCK),
        (
            "ship.morph",
            "-- shipping\ntransformation ship(item)\n    admit Shipped(item)\n",
        ),
    ]);
    let errors = source.parse().expect_err("the colon is missing");
    let at = source.locate(errors[0].primary.start);
    assert_eq!((at.file, at.line), ("ship.morph", 3), "{errors:?}");
    let rendered = source.render(&errors[0], false);
    assert!(rendered.contains("ship.morph:3:"), "{rendered}");
}

#[test]
fn a_duplicate_across_parts_points_at_both_files() {
    let source = parts(&[
        ("h.morph", HEADER),
        ("a.morph", STOCK),
        ("b.morph", "predicate Stock(item: Subject)\n"),
    ]);
    let errors = source.parse().expect_err("Stock twice");
    let dup = errors
        .iter()
        .find(|d| d.message.contains("duplicate"))
        .expect("a duplicate");
    assert_eq!(source.locate(dup.primary.start).file, "b.morph");
    assert_eq!(source.locate(dup.secondary[0].0.start).file, "a.morph");
    let rendered = source.render(dup, false);
    assert!(
        rendered.contains("a.morph:1:") && rendered.contains("b.morph:1:"),
        "{rendered}"
    );
}

/// The first error at a boundary, and the file it is placed in.
fn refused(texts: &[(&str, &str)]) -> (String, String) {
    let source = parts(texts);
    let errors = source.parse().expect_err("refused at a boundary");
    let at = source.locate(errors[0].primary.start);
    (at.file.to_string(), errors[0].message.clone())
}

/// Without its newline, a part's last line would run into the next part's first: here the
/// comment would swallow `Shipped`, and the programme would lose a predicate without a word.
#[test]
fn a_part_without_a_final_newline_is_refused() {
    let (file, message) = refused(&[
        ("h.morph", HEADER),
        ("a.morph", "predicate Stock(item: Subject) -- the stock"),
        ("b.morph", SHIPPED),
    ]);
    assert_eq!(file, "a.morph");
    assert!(message.contains("does not end with a newline"), "{message}");
}

#[test]
fn a_part_that_leaves_a_parenthesis_open_is_refused() {
    let (file, message) = refused(&[
        ("h.morph", HEADER),
        ("a.morph", "predicate Stock(item: Subject,\n"),
        ("b.morph", "quantity: Decimal)\n"),
    ]);
    assert_eq!(file, "a.morph");
    assert!(
        message.contains("still open at the end of `a.morph`"),
        "{message}"
    );
}

#[test]
fn a_part_that_starts_indented_is_refused() {
    let (file, message) = refused(&[
        ("h.morph", HEADER),
        ("a.morph", &format!("{SHIPPED}transformation ship(item):\n")),
        ("b.morph", "-- the body\n    admit Shipped(item)\n"),
    ]);
    assert_eq!(file, "b.morph");
    assert!(message.contains("starts indented"), "{message}");
}

#[test]
fn only_the_first_part_holds_the_header() {
    let (file, message) = refused(&[("a.morph", STOCK), ("h.morph", HEADER)]);
    assert_eq!(file, "a.morph");
    assert!(message.contains("must begin with `program"), "{message}");
    let (file, message) = refused(&[("h.morph", HEADER), ("b.morph", "program other\n")]);
    assert_eq!(file, "b.morph");
    assert!(message.contains("only the first part listed"), "{message}");
}

/// A part given alone is a file with no header, and says where its header lives.
#[test]
fn a_part_alone_points_at_its_manifest() {
    let errors = parse_program(STOCK).expect_err("no header");
    assert!(
        errors[0]
            .message
            .contains("expected `program` header first")
            && errors[0].message.contains("`.parts` manifest"),
        "{errors:?}"
    );
}

/// One file is judged by the parser alone, with offsets unchanged.
#[test]
fn one_file_reads_as_itself() {
    let text = format!("{HEADER}{STOCK}");
    let source = SourceText::file("shop.morph", text.as_str());
    assert_eq!(
        canonical_hash(&source.parse().expect("parses").0),
        canonical_hash(&parse_program(&text).expect("parses"))
    );
    let at = source.locate(HEADER.len());
    assert_eq!(
        (at.file, at.offset, at.line, at.column),
        ("shop.morph", HEADER.len(), 2, 1)
    );
}
