//! `classify_checked_query` may only appear where a checked macro query does.
//!
//! Checked macro queries are verified against `sql/schema.sql` at build
//! time, so a missing column at runtime means the database is behind, not
//! that the query is wrong. On anything else - dynamic SQL, a commit, a
//! rollback, acquiring a connection - it could report a real bug, or a
//! tampered view-defs table, as an upgrade problem.
//!
//! The compiler cannot express this rule, and it is easy to break by
//! hand, so a test enforces it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

/// The statement a byte offset sits in: back to the previous `;`, `{` or `}`.
fn enclosing_statement(source: &str, at: usize) -> &str {
    let start = [
        source[..at].rfind(';'),
        source[..at].rfind('{'),
        source[..at].rfind('}'),
    ]
    .into_iter()
    .flatten()
    .max()
    .map_or(0, |i| i + 1);
    &source[start..at]
}

#[test]
fn the_checked_classifier_only_guards_checked_queries() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut checked = 0;
    let mut offenders = Vec::new();

    for entry in std::fs::read_dir(&src).expect("src directory") {
        let path = entry.expect("dir entry").path();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        // error.rs defines the function and tests it directly.
        if name == "error.rs" {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read source");
        for (offset, _) in source.match_indices("classify_checked_query") {
            // The import names it without calling it. Checked by line: a
            // braced import's `{` is a statement boundary, so the statement
            // view would see only the names inside the braces.
            let line_start = source[..offset].rfind('\n').map_or(0, |i| i + 1);
            let line_end = source[offset..]
                .find('\n')
                .map_or(source.len(), |i| offset + i);
            if source[line_start..line_end]
                .trim_start()
                .starts_with("use ")
            {
                continue;
            }
            let stmt = enclosing_statement(&source, offset);
            checked += 1;
            let guards_a_macro = ["sqlx::query!", "sqlx::query_as!", "sqlx::query_scalar!"]
                .iter()
                .any(|m| stmt.contains(m));
            if !guards_a_macro {
                let line = source[..offset].lines().count();
                offenders.push(format!("{name}:{line}"));
            }
        }
    }

    assert!(
        checked > 20,
        "anti-vacuity: the scan found only {checked} call sites, so it is not reading what it thinks"
    );
    assert!(
        offenders.is_empty(),
        "classify_checked_query guards something that is not a checked macro query at: {}. \
         Use `classify` there - the stale-schema inference does not hold for dynamic SQL, \
         commits, rollbacks or connection acquisition.",
        offenders.join(", ")
    );
}
