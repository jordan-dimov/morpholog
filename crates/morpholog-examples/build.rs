//! Generates the per-example accessor modules from the `.morph` source.
//!
//! Each `examples/<NN_dir>/` holds exactly one `.morph`, the single source of
//! truth. This script scans it for top-level `transformation`, `invariant` and
//! `derived` names (a textual scan of line-leading keywords) and writes an
//! accessor module into `OUT_DIR`. `lib.rs` includes each with one
//! `example_module!(<name>)` line; forgetting it is a compile error, because the
//! generated `all_programs()` registry names the module.
//!
//! The scan sees authored declarations only; generated discipline invariants
//! appear later, at parse time. Transformations and invariants keep their names;
//! derived claims are snake-cased from their PascalCase predicate. A name that
//! is not a plain Rust identifier fails the build with a clear message.

// A build script panics on error by design - a failure here is a build
// failure, surfaced with the panic message.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let examples_dir = Path::new(&manifest).join("../../examples");

    println!("cargo:rerun-if-changed={}", examples_dir.display());

    let mut modules: Vec<(String, String)> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
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
        assert!(
            seen.insert(module.clone()),
            "two example directories yield the same module name `{module}` \
             (numeric prefixes are stripped); rename one"
        );

        let file_name = morph.file_name().unwrap().to_str().unwrap();
        let rel = format!("{dir_name}/{file_name}");
        let source = fs::read_to_string(&morph).unwrap();

        let rendered = render_module(&module, &rel, &source);
        fs::write(Path::new(&out_dir).join(format!("{module}.rs")), rendered).unwrap();
        modules.push((module, rel));
    }

    // One generated registry, so a new `.morph` is covered the moment it is
    // added, with no manual list to forget.
    modules.sort();
    let mut registry = String::from(
        "/// Every worked example, as build discovery found it: the single\n\
         /// authority on what \"all worked examples\" means.\n\
         pub fn all_examples() -> Vec<crate::ExampleDescriptor> {\n    vec![\n",
    );
    for (m, rel) in &modules {
        registry.push_str(&format!(
            "        crate::ExampleDescriptor {{\n            \
             name: {m:?},\n            \
             rel_path: {rel:?},\n            \
             source: crate::{m}::SOURCE,\n            \
             program: crate::{m}::program,\n        \
             }},\n"
        ));
    }
    registry.push_str("    ]\n}\n\n");
    registry.push_str(
        "/// Every worked example's program, derived from [`all_examples`].\n\
         pub fn all_programs() -> Vec<morpholog_core::Program> {\n    \
         all_examples().into_iter().map(|e| (e.program)()).collect()\n}\n",
    );
    fs::write(Path::new(&out_dir).join("_registry.rs"), registry).unwrap();
}

/// The single `.morph` in an example directory, or `None` for a directory
/// (like the worked-embedder) that has none. More than one is an error: a
/// worked example is exactly one source file.
fn find_morph(dir: &Path) -> Option<PathBuf> {
    let mut morphs: Vec<PathBuf> = fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "morph"))
        .collect();
    morphs.sort();
    assert!(
        morphs.len() <= 1,
        "example directory {} has more than one .morph file ({morphs:?}); \
         a worked example is exactly one source",
        dir.display(),
    );
    morphs.into_iter().next()
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

/// Names of each `keyword` declaration, in source order: the first token after
/// `keyword ` at the start of a line, before the terminator. Textual, not lexical.
fn declarations<'a>(source: &'a str, keyword: &str, terminator: char) -> Vec<&'a str> {
    let prefix = format!("{keyword} ");
    source
        .lines()
        .filter_map(|line| line.trim().strip_prefix(&prefix))
        .filter_map(|rest| rest.split(terminator).next())
        .map(str::trim)
        // A declaration may carry clauses before its terminator
        // (`invariant N total over P:`); the name is the first token.
        .filter_map(|head| head.split_whitespace().next())
        .filter(|name| !name.is_empty())
        .collect()
}

fn render_module(name: &str, rel: &str, source: &str) -> String {
    let transformations = declarations(source, "transformation", '(');
    let invariants = declarations(source, "invariant", ':');
    let deriveds = declarations(source, "derived", '(');

    let mut out = String::new();
    out.push_str(&format!("// @generated from examples/{rel}\n"));
    out.push_str("use std::sync::LazyLock;\n");
    out.push_str("use morpholog_core::{Definition, Invariant, PredicateDecl, Program");
    if !transformations.is_empty() {
        out.push_str(", Transformation");
    }
    if !deriveds.is_empty() {
        out.push_str(", DerivedClaim");
    }
    out.push_str("};\n\n");

    // Resolved from CARGO_MANIFEST_DIR at compile time, not baked in as an
    // absolute path. The registry reuses this const, so the path lives in one place.
    out.push_str(&format!(
        "/// The example's `.morph` source, embedded at compile time.\n\
         pub const SOURCE: &str =\n    \
         include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../../examples/{rel}\"));\n\n"
    ));
    out.push_str(&format!(
        "static PROGRAM: LazyLock<Program> = LazyLock::new(|| crate::parse_example({name:?}, SOURCE));\n\n"
    ));

    out.push_str("pub fn program() -> Program { PROGRAM.clone() }\n");
    out.push_str("pub fn all_predicates() -> Vec<PredicateDecl> { PROGRAM.predicates.clone() }\n");
    out.push_str("pub fn all_invariants() -> Vec<Invariant> { PROGRAM.invariants.clone() }\n");
    out.push_str("pub fn definitions() -> Vec<Definition> { PROGRAM.definitions.clone() }\n\n");

    for t in &transformations {
        let ident = validated_ident(t);
        out.push_str(&format!(
            "pub fn {ident}() -> Transformation {{ crate::transformation(&PROGRAM, {t:?}) }}\n"
        ));
    }
    for i in &invariants {
        let ident = validated_ident(i);
        out.push_str(&format!(
            "pub fn {ident}() -> Invariant {{ crate::invariant(&PROGRAM, {i:?}) }}\n"
        ));
    }
    for d in &deriveds {
        let snake = snake_case(d);
        let ident = validated_ident(&snake);
        out.push_str(&format!(
            "pub fn {ident}() -> DerivedClaim {{ crate::derived(&PROGRAM, {d:?}) }}\n"
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

/// Fails the build with a clear message unless `name` is a plain Rust
/// identifier and not a keyword, rather than a cryptic error in generated code.
fn validated_ident(name: &str) -> &str {
    let mut chars = name.chars();
    let well_formed = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    assert!(
        well_formed,
        "declaration name `{name}` is not a plain identifier; the accessor generator cannot name a getter for it"
    );
    assert!(
        !is_rust_keyword(name),
        "declaration name `{name}` is a Rust keyword; rename the declaration or extend the generator with raw-identifier handling"
    );
    name
}

fn is_rust_keyword(name: &str) -> bool {
    matches!(
        name,
        "as" | "break"
            | "const"
            | "continue"
            | "crate"
            | "dyn"
            | "else"
            | "enum"
            | "extern"
            | "false"
            | "fn"
            | "for"
            | "if"
            | "impl"
            | "in"
            | "let"
            | "loop"
            | "match"
            | "mod"
            | "move"
            | "mut"
            | "pub"
            | "ref"
            | "return"
            | "self"
            | "Self"
            | "static"
            | "struct"
            | "super"
            | "trait"
            | "true"
            | "type"
            | "unsafe"
            | "use"
            | "where"
            | "while"
            | "async"
            | "await"
            | "box"
            | "do"
            | "final"
            | "macro"
            | "override"
            | "priv"
            | "typeof"
            | "unsized"
            | "virtual"
            | "yield"
            | "gen"
            | "try"
            | "union"
    )
}
