//! Generates the per-example accessor modules from the `.morph` source.
//!
//! Each worked example's `.morph` is the single source of truth. This
//! script reads every `examples/<NN_dir>/<file>.morph`, extracts its
//! top-level `transformation` / `invariant` / `derived` declaration
//! names by a line-leading scan (those forms are stable v0 syntax), and
//! emits a module of by-name accessors into `OUT_DIR`. `lib.rs` includes
//! the generated modules, so adding an example is just dropping a
//! `.morph` - no hand-written accessor boilerplate, no registry edit.
//!
//! The scan reads only the SOURCE, so it sees authored declarations only
//! (generated discipline invariants do not exist until parse-time
//! lowering). Accessor names: transformations and invariants verbatim
//! (snake-case in the surface), derived claims snake-cased from their
//! PascalCase output predicate.

// A build script panics on error by design - a failure here is a build
// failure, surfaced with the panic message.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let examples_dir = Path::new(&manifest).join("../../examples");

    println!("cargo:rerun-if-changed={}", examples_dir.display());

    let mut modules: Vec<String> = Vec::new();
    for entry in fs::read_dir(&examples_dir).expect("examples/ dir") {
        let dir = entry.unwrap().path();
        if !dir.is_dir() {
            continue;
        }
        let Some(morph) = find_morph(&dir) else {
            continue;
        };
        println!("cargo:rerun-if-changed={}", morph.display());

        let dir_name = dir.file_name().unwrap().to_str().unwrap();
        let module = strip_numeric_prefix(dir_name).to_string();
        let source = fs::read_to_string(&morph).unwrap();
        let abs = fs::canonicalize(&morph).unwrap();

        let rendered = render_module(&module, &abs, &source);
        fs::write(Path::new(&out_dir).join(format!("{module}.rs")), rendered).unwrap();
        modules.push(module);
    }

    // The auto-discovered registry: every example, enumerated for the
    // cross-example property tests. Generated, so a new `.morph` is covered
    // the moment it is added - no manual list to forget.
    modules.sort();
    let mut registry = String::from(
        "/// Every worked example's program, auto-discovered from `examples/`.\n\
         pub fn all_programs() -> Vec<morpholog_core::Program> {\n    vec![\n",
    );
    for m in &modules {
        registry.push_str(&format!("        crate::{m}::program(),\n"));
    }
    registry.push_str("    ]\n}\n");
    fs::write(Path::new(&out_dir).join("_registry.rs"), registry).unwrap();
}

/// The single `.morph` file in an example directory, if any.
fn find_morph(dir: &Path) -> Option<PathBuf> {
    fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "morph"))
}

/// `03_double_entry_ledger` -> `double_entry_ledger`. A directory with no
/// numeric prefix keeps its name.
fn strip_numeric_prefix(dir: &str) -> &str {
    match dir.split_once('_') {
        Some((prefix, rest))
            if !prefix.is_empty() && prefix.bytes().all(|b| b.is_ascii_digit()) =>
        {
            rest
        }
        _ => dir,
    }
}

/// Line-leading names of each declaration kind, in source order.
fn declarations<'a>(source: &'a str, keyword: &str, terminator: char) -> Vec<&'a str> {
    let prefix = format!("{keyword} ");
    source
        .lines()
        .filter_map(|line| line.trim().strip_prefix(&prefix))
        .filter_map(|rest| rest.split(terminator).next())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect()
}

fn render_module(name: &str, morph_abs: &Path, source: &str) -> String {
    let transformations = declarations(source, "transformation", '(');
    let invariants = declarations(source, "invariant", ':');
    let deriveds = declarations(source, "derived", '(');

    let mut out = String::new();
    out.push_str(&format!("// @generated from {}\n", morph_abs.display()));
    out.push_str("use std::sync::LazyLock;\n");
    out.push_str("use morpholog_core::{Definition, Invariant, PredicateDecl, Program");
    if !transformations.is_empty() {
        out.push_str(", Transformation");
    }
    if !deriveds.is_empty() {
        out.push_str(", DerivedClaim");
    }
    out.push_str("};\n\n");

    out.push_str(&format!(
        "static PROGRAM: LazyLock<Program> = LazyLock::new(|| {{\n    \
         crate::parse_example({name:?}, include_str!({:?}))\n}});\n\n",
        morph_abs.display()
    ));

    out.push_str("pub fn program() -> Program { PROGRAM.clone() }\n");
    out.push_str("pub fn all_predicates() -> Vec<PredicateDecl> { PROGRAM.predicates.clone() }\n");
    out.push_str("pub fn all_invariants() -> Vec<Invariant> { PROGRAM.invariants.clone() }\n");
    out.push_str("pub fn definitions() -> Vec<Definition> { PROGRAM.definitions.clone() }\n\n");

    for t in &transformations {
        out.push_str(&format!(
            "pub fn {t}() -> Transformation {{ crate::transformation(&PROGRAM, {t:?}) }}\n"
        ));
    }
    for i in &invariants {
        out.push_str(&format!(
            "pub fn {i}() -> Invariant {{ crate::invariant(&PROGRAM, {i:?}) }}\n"
        ));
    }
    for d in &deriveds {
        out.push_str(&format!(
            "pub fn {}() -> DerivedClaim {{ crate::derived(&PROGRAM, {d:?}) }}\n",
            snake_case(d)
        ));
    }
    out
}

/// `TrialBalanceRow` -> `trial_balance_row`. Derived output predicates are
/// PascalCase; their accessors are snake-case, matching test call sites.
fn snake_case(pascal: &str) -> String {
    let mut out = String::new();
    for (i, c) in pascal.chars().enumerate() {
        if c.is_uppercase() {
            if i != 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}
