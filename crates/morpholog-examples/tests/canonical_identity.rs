//! What every gallery programme's source hashes to, so an upgrade cannot
//! quietly give an unchanged rulebook a new identity.
//!
//! The golden holds, per `.morph` file, a digest of its source bytes and
//! the programme's canonical hash. An unchanged source whose hash moved
//! fails always, even under `UPDATE_GOLDENS=1`: the parser or the encoder
//! now reads the same rules as different ones, and every audit row stamped
//! with the old hash would stop naming its programme. An edited, added or
//! removed source fails until the golden is regenerated, so the new
//! identity shows in review.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use morpholog_core::format::canonical_hash;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// `(source digest, canonical hash)` per path relative to the repository.
type Identities = BTreeMap<String, (String, String)>;

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn golden_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/canonical_hashes.ndjson")
}

fn gallery() -> Identities {
    let mut out = BTreeMap::new();
    for dir in std::fs::read_dir(repo().join("examples")).unwrap() {
        let dir = dir.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        for file in std::fs::read_dir(&dir).unwrap() {
            let file = file.unwrap().path();
            if file.extension().is_none_or(|e| e != "morph") {
                continue;
            }
            let source = std::fs::read_to_string(&file).unwrap();
            let program = morpholog_surface::parse_program(&source)
                .unwrap_or_else(|_| panic!("{} does not parse", file.display()));
            let digest: String = Sha256::digest(source.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let path = file.strip_prefix(repo()).unwrap().display().to_string();
            out.insert(path, (format!("sha256:{digest}"), canonical_hash(&program)));
        }
    }
    assert!(!out.is_empty(), "no .morph files found under examples/");
    out
}

fn read_golden() -> Option<Identities> {
    let text = std::fs::read_to_string(golden_path()).ok()?;
    Some(
        text.lines()
            .map(|line| {
                let row: Value = serde_json::from_str(line).unwrap();
                (
                    row["path"].as_str().unwrap().to_string(),
                    (
                        row["source_sha256"].as_str().unwrap().to_string(),
                        row["canonical_hash"].as_str().unwrap().to_string(),
                    ),
                )
            })
            .collect(),
    )
}

fn write_golden(identities: &Identities) {
    let text: String = identities
        .iter()
        .map(|(path, (source, hash))| {
            json!({"path": path, "source_sha256": source, "canonical_hash": hash}).to_string()
                + "\n"
        })
        .collect();
    std::fs::write(golden_path(), text).unwrap();
}

#[test]
fn an_unchanged_source_keeps_its_canonical_hash() {
    let now = gallery();
    let golden = read_golden();
    if let Some(golden) = &golden {
        let moved: Vec<_> = now
            .iter()
            .filter(|(path, (source, hash))| {
                golden
                    .get(*path)
                    .is_some_and(|(was_source, was)| was_source == source && was != hash)
            })
            .map(|(path, _)| path)
            .collect();
        assert!(
            moved.is_empty(),
            "these sources are unchanged but their canonical hash moved, so committed audit \
             rows would no longer name their programme. `sha256:` names canonical encoding 1, \
             which is frozen; regenerating the golden cannot accept this. Moved: {moved:?}"
        );
    }
    if std::env::var_os("UPDATE_GOLDENS").is_some() {
        write_golden(&now);
        return;
    }
    let golden = golden.expect("no golden; generate it with UPDATE_GOLDENS=1");
    let changed: Vec<_> = now
        .keys()
        .chain(golden.keys())
        .filter(|path| now.get(*path) != golden.get(*path))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    assert!(
        changed.is_empty(),
        "the gallery's sources changed: regenerate with UPDATE_GOLDENS=1 and review the \
         new identities. Changed, added or removed: {changed:?}"
    );
}
