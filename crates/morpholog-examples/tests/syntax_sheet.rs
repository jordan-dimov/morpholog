//! `docs/surface-syntax.md` shows every surface form once, and each block
//! on it must be a programme the parser accepts, the validator passes and
//! the lints leave alone: a sheet that drifts from the parser teaches the
//! wrong spelling.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use morpholog_core::lints;
use morpholog_surface::parse_program;
use morpholog_test_support::prepare;

fn sheet_blocks() -> Vec<(usize, String)> {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/surface-syntax.md");
    let text = std::fs::read_to_string(&path).expect("docs/surface-syntax.md");
    let mut blocks = Vec::new();
    let mut current: Option<(usize, String)> = None;
    for (number, line) in text.lines().enumerate() {
        match (&mut current, line.trim_end()) {
            (None, "```morph") => current = Some((number + 1, String::new())),
            (Some(_), "```") => blocks.push(current.take().unwrap()),
            (Some((_, body)), _) => {
                body.push_str(line);
                body.push('\n');
            }
            (None, _) => {}
        }
    }
    assert!(current.is_none(), "an unclosed block");
    blocks
}

#[test]
fn every_block_on_the_sheet_parses_validates_and_is_lint_clean() {
    let blocks = sheet_blocks();
    assert!(blocks.len() >= 8, "anti-vacuity: {} blocks", blocks.len());
    for (line, source) in blocks {
        let program = parse_program(&source)
            .unwrap_or_else(|e| panic!("the block at line {line} does not parse: {e:?}"));
        program
            .validate()
            .unwrap_or_else(|e| panic!("the block at line {line} does not validate: {e:?}"));
        let found = lints(&prepare(&program));
        assert!(
            found.is_empty(),
            "the block at line {line} ({}) is not lint-clean: {found:?}",
            program.name
        );
    }
}
