//! Generates the per-example accessor modules from the `.morph` source.
//!
//! Each worked example's `.morph` is the single source of truth. This
//! script reads every `examples/<NN_dir>/<file>.morph` (exactly one `.morph`
//! per directory - more is an error), extracts its top-level `transformation`
//! / `invariant` / `derived` declaration names by a leading-token scan (the
//! name follows the keyword at the start of a line, after any indentation;
//! these forms are stable v0 syntax, and the scan is textual, not lexical),
//! and emits an accessor module into `OUT_DIR`. `lib.rs` brings each in with
//! one `example_module!(<name>)` line - that one line is the only manual step,
//! and it cannot be silently forgotten: the generated `all_programs()`
//! registry references the module, so a missing line is a compile error.
//!
//! The scan reads only the SOURCE, so it sees authored declarations only
//! (generated discipline invariants do not exist until parse-time lowering).
//! Accessor names: transformations and invariants verbatim (snake-case in the
//! surface), derived claims snake-cased from their PascalCase output
//! predicate. Every emitted name is validated as a plain Rust identifier (and
//! not a keyword), so a malformed declaration fails the build with a clear
//! message rather than a cryptic error in generated code.

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

    let mut modules: Vec<String> = Vec::new();
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

/// Leading-token names of each declaration kind, in source order: the name
/// following `keyword ` at the start of a line (after any indentation), up to
/// the terminator. Textual, not lexical - fine for the stable top-level
/// declaration forms.
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

    // The path is resolved at the lib's compile time from CARGO_MANIFEST_DIR,
    // not baked in as a build-machine absolute path.
    out.push_str(&format!(
        "static PROGRAM: LazyLock<Program> = LazyLock::new(|| {{\n    \
         crate::parse_example({name:?}, include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/../../examples/{rel}\")))\n}});\n\n"
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

/// A declaration name must become a plain Rust identifier (`a-z`, `0-9`, `_`,
/// not starting with a digit) and not a keyword. A name that cannot fails the
/// build here with a clear message, instead of a cryptic error in generated
/// code. Raw-identifier handling for a keyword-named declaration is left until
/// an example forces it.
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
