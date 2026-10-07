//! `docs/README.md` maps the documentation, and it has to stay complete.
//!
//! A new document in `docs/` that the guide never links is invisible to a
//! reader who starts there, and a renamed one leaves a dead link behind.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn docs_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs")
}

fn guide_text() -> String {
    std::fs::read_to_string(docs_dir().join("README.md")).expect("docs/README.md")
}

/// Every `](target)` in the text that is not a URL or an in-page anchor,
/// with any anchor removed.
fn relative_links(text: &str) -> Vec<String> {
    text.split("](")
        .skip(1)
        .filter_map(|rest| rest.split_once(')'))
        .map(|(target, _)| target.split('#').next().unwrap().to_string())
        .filter(|target| !target.is_empty() && !target.contains("://"))
        .collect()
}

#[test]
fn the_documentation_guide_links_every_document_in_docs() {
    let linked: BTreeSet<String> = relative_links(&guide_text()).into_iter().collect();
    let unlinked: Vec<String> = std::fs::read_dir(docs_dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name.ends_with(".md") && name != "README.md")
        .filter(|name| !linked.contains(name))
        .collect();
    assert!(
        unlinked.is_empty(),
        "docs/README.md does not link {unlinked:?}: add each where a reader would look for it"
    );
}

#[test]
fn every_link_in_the_documentation_guide_resolves() {
    let dead: Vec<String> = relative_links(&guide_text())
        .into_iter()
        .filter(|target| !docs_dir().join(target).exists())
        .collect();
    assert!(
        dead.is_empty(),
        "docs/README.md links to missing files: {dead:?}"
    );
}
