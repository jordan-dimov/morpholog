//! `examples/README.md` is the capability index, and it has to stay true.
//!
//! Why it exists: an embedder spent a week designing around a limitation that
//! did not exist, because the gallery is indexed by domain and the example
//! demonstrating what they wanted was named after a different business. Three
//! of their first four capability requests were features they could not find.
//!
//! Why it is tested: an index is a promise that a reader can get from a
//! question to an example, and that promise decays silently. Nothing about
//! writing a `.morph` reminds anyone to touch a table in a different file.
//!
//! **What the gate checks, and why the first version was not enough.** It
//! began by checking only that every example is linked and every link
//! resolves. That leaves the thing the index is actually for unverified:
//! repointing the `abs(...)` row at `01_settlement_netting` kept every test
//! green, because the directory exists and the other rows still linked
//! `10_trade_lifecycle`. So each row's construct now has to appear in the
//! example it points at - the row's own evidence, encoded instead of
//! discarded.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn examples_dir() -> PathBuf {
    repo_root().join("examples")
}

fn index_text() -> String {
    std::fs::read_to_string(examples_dir().join("README.md")).expect("examples/README.md")
}

/// One row of the capability table.
struct Row {
    capability: String,
    directory: String,
    /// The backticked spellings in the third column.
    constructs: Vec<String>,
}

/// Rows whose third column names something no token search can find in the
/// example, with the reason. Listed rather than skipped by a rule, so an
/// exemption is a decision someone made and can be argued with.
const CONCEPTUAL_ROWS: &[(&str, &str)] = &[
    (
        "record evidence *about* a claim",
        "the capability is a modelling shape - claims whose subjects are other \
         claims - not a keyword that appears anywhere",
    ),
    (
        "expire a check",
        "currentness is a pattern built from ordinary dated claims, with no \
         construct of its own",
    ),
    (
        "show that a rule comes from a named statute",
        "the artefact is the article-to-rule table in that example's README, \
         which the row names directly",
    ),
    (
        "call Morpholog from an application",
        "the subject is the generated client, not a surface construct",
    ),
    (
        "hold a ratio, a rate or an advance limit",
        "the row names arithmetic in prose rather than a spelling to search for",
    ),
    (
        "admit a whole set of records in one act",
        "a collection parameter has no distinguishing token - it is an \
         ordinary parameter whose argument is a list; the `for` half is \
         checked",
    ),
    (
        "find out what evidence a refusal is missing",
        "`explain` is a command, so it appears in that example's README rather \
         than in its `.morph`; checked there instead",
    ),
];

fn parse_rows(index: &str) -> Vec<Row> {
    let mut rows = Vec::new();
    for line in index.lines() {
        let line = line.trim();
        if !line.starts_with('|') || line.starts_with("|---") || line.contains("...do this") {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        if cells.len() != 3 {
            continue;
        }
        let Some(at) = cells[1].find("](") else {
            continue;
        };
        let directory = cells[1][at + 2..]
            .split(')')
            .next()
            .unwrap_or_default()
            .trim_end_matches('/')
            .to_string();
        // Backticked spellings, which is how the third column names a construct.
        let constructs: Vec<String> = cells[2]
            .split('`')
            .skip(1)
            .step_by(2)
            .map(|c| c.trim().to_string())
            .filter(|c| !c.is_empty())
            .collect();
        rows.push(Row {
            capability: cells[0].to_string(),
            directory,
            constructs,
        });
    }
    rows
}

/// Everything readable under an example directory, so a construct can be
/// found in the programme or in the prose that teaches it.
fn example_text(dir: &str) -> String {
    let mut text = String::new();
    let path = examples_dir().join(dir);
    let mut stack = vec![path];
    while let Some(p) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&p) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path
                .extension()
                .is_some_and(|e| e == "morph" || e == "md" || e == "py")
            {
                text.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
            }
        }
    }
    text
}

/// The searchable head of a construct spelling.
///
/// The leading identifier, plus `(` when the spelling is function-shaped:
/// `min(a, b)` searches as `min(`, `require some_name: ...` as `require`,
/// `Decimal[t]` as `Decimal`. Splitting on whitespace instead produced
/// `min(a,` and `sum(...`, which are in no example - the gate reported the
/// index as wrong when the tokenizer was.
fn searchable_head(construct: &str) -> String {
    let ident: String = construct
        .chars()
        .take_while(|c| c.is_ascii_alphabetic() || *c == '_')
        .collect();
    let rest = &construct[ident.len()..];
    if rest.starts_with('(') {
        format!("{ident}(")
    } else {
        ident
    }
}

fn is_conceptual(capability: &str) -> Option<&'static str> {
    CONCEPTUAL_ROWS
        .iter()
        .find(|(prefix, _)| capability.contains(prefix))
        .map(|(_, reason)| *reason)
}

/// Each row's construct appears in the example the row points at.
///
/// This is the check the index exists for: not that the link resolves, but
/// that a reader following it finds the thing they came for.
#[test]
fn every_row_demonstrates_its_construct_in_the_example_it_names() {
    let rows = parse_rows(&index_text());
    assert!(
        rows.len() > 15,
        "anti-vacuity: parsed only {} rows out of the table",
        rows.len()
    );

    let mut wrong = Vec::new();
    let mut checked = 0;
    for row in &rows {
        if is_conceptual(&row.capability).is_some() {
            continue;
        }
        let text = example_text(&row.directory);
        assert!(
            !text.is_empty(),
            "read nothing under examples/{}, so this row is unverified",
            row.directory
        );
        for construct in &row.constructs {
            let token = searchable_head(construct);
            if token.len() < 2 {
                continue;
            }
            checked += 1;
            if !text.contains(&token) {
                wrong.push(format!(
                    "`{token}` is not in examples/{} (row: {})",
                    row.directory, row.capability
                ));
            }
        }
    }
    assert!(
        checked > 15,
        "anti-vacuity: only {checked} construct-in-example checks ran"
    );
    assert!(
        wrong.is_empty(),
        "the index points readers at examples that do not show the construct: {wrong:#?}. \
         Either the row names the wrong example or the construct is spelled wrong - both \
         send a reader to the wrong place, which is what this index exists to stop."
    );
}

/// Every example is reachable from the index.
#[test]
fn every_example_appears_in_the_capability_index() {
    let index = index_text();
    let linked: BTreeSet<String> = parse_rows(&index)
        .into_iter()
        .map(|r| r.directory)
        .collect();
    let dirs: BTreeSet<String> = std::fs::read_dir(examples_dir())
        .expect("examples directory")
        .filter_map(|e| {
            let path = e.expect("dir entry").path();
            if !path.is_dir() {
                return None;
            }
            Some(path.file_name()?.to_string_lossy().to_string())
        })
        .collect();

    assert!(
        dirs.len() > 10,
        "anti-vacuity: found {} examples",
        dirs.len()
    );
    let missing: Vec<&String> = dirs.difference(&linked).collect();
    assert!(
        missing.is_empty(),
        "these examples are in the gallery and not in examples/README.md, so a reader \
         with the question they answer cannot find them: {missing:?}"
    );
    let dangling: Vec<&String> = linked.difference(&dirs).collect();
    assert!(
        dangling.is_empty(),
        "examples/README.md links directories that are not there: {dangling:?}"
    );
}

/// Every construct the index names exists in the canonical surface-to-IR
/// table - derived from the index's own third column, not a shadow list.
///
/// The first version kept a hand-maintained array of spellings to check, and
/// silently ignored anything absent from it: a misspelt construct could be
/// added to the index and the test stayed green. It also searched the whole
/// semantics document, so unrelated prose could satisfy it.
#[test]
fn every_construct_the_index_names_is_in_the_canonical_table() {
    let semantics = std::fs::read_to_string(repo_root().join("docs/runtime-semantics.md"))
        .expect("docs/runtime-semantics.md");
    let table = semantics
        .split_once("## Surface-to-IR mapping")
        .expect("the canonical table has its own section")
        .1;
    assert!(
        table.len() > 2000,
        "anti-vacuity: the canonical table section reads as {} bytes",
        table.len()
    );

    // Types, command names, and predicate names the RUNTIME reserves
    // are not surface constructs and have no row. The reserved names
    // are ordinary predicates an operator declares and governs; only
    // their recognition is built in, so the surface grammar has
    // nothing to say about them.
    const NOT_IN_THE_TABLE: &[&str] = &[
        "Timestamp",
        "Duration",
        "Decimal[t]",
        "Decimal[USD]",
        "explain",
        "intent",
        "ActorAssertionRestricted",
        "ActorAssertionAuthority",
    ];

    let mut absent = Vec::new();
    let mut checked = 0;
    for row in parse_rows(&index_text()) {
        for construct in row.constructs {
            let token = searchable_head(&construct);
            if token.len() < 2 || NOT_IN_THE_TABLE.contains(&token.as_str()) {
                continue;
            }
            checked += 1;
            if !table.contains(&token) {
                absent.push(format!("`{token}` (row: {})", row.capability));
            }
        }
    }
    assert!(
        checked > 10,
        "anti-vacuity: checked only {checked} constructs"
    );
    assert!(
        absent.is_empty(),
        "the index names constructs the canonical surface-to-IR table does not: {absent:#?}. \
         Either the spelling is wrong or the table is out of date - one sends a reader to \
         write something that will not parse."
    );
}

/// The two indexes agree on which examples exist.
///
/// The index page says the main README lists the same examples; it did not -
/// the domain list stopped short of two of them, so the sentence was false.
#[test]
fn the_domain_index_and_the_capability_index_cover_the_same_examples() {
    let readme = std::fs::read_to_string(repo_root().join("README.md")).expect("README.md");
    let indexed: BTreeSet<String> = parse_rows(&index_text())
        .into_iter()
        .map(|r| r.directory)
        .collect();
    let missing: Vec<&String> = indexed
        .iter()
        .filter(|d| !readme.contains(&format!("examples/{d}/")))
        .collect();
    assert!(
        missing.is_empty(),
        "the capability index covers examples the main README's domain list does not: \
         {missing:?}. The index page tells the reader both list the same examples."
    );
}
