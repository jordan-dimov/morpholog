//! Faithfulness: every hand-authored example `.morph` parses to exactly
//! the IR its program registers in `all_programs()`.
//!
//! Complements `round_trip` (which checks `format(IR) -> parse == IR` over
//! *generated* source). This checks the *hand-written* teaching source, so
//! a dropped invariant, a wrong argument order, or a typo in an example
//! cannot silently teach a different model than the code actually runs.
//! The teaching surface is a first-class asset; this holds it to the
//! registered IR.
//!
//! Discovery is by parsed `program.name`, not by directory name, so the
//! numbered example directories need not map to anything - and a `.morph`
//! that is not registered, or a registered program with no source, both
//! fail loudly.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

use morpholog_core::Program;
use morpholog_examples::all_programs;
use morpholog_surface::parse_program;

/// A readable, structural account of how a parsed `.morph` differs from
/// its registered IR: which names are present on one side only, and -
/// when the names all match - which bodies or argument lists differ.
/// Far more useful than two `{:#?}` dumps.
fn describe_drift(parsed: &Program, registry: &Program) -> String {
    fn diff(out: &mut Vec<String>, kind: &str, morph: BTreeSet<&str>, reg: BTreeSet<&str>) {
        let missing: Vec<&str> = reg.difference(&morph).copied().collect();
        let extra: Vec<&str> = morph.difference(&reg).copied().collect();
        if !missing.is_empty() {
            out.push(format!(
                "  {kind} in IR but missing from .morph: {}",
                missing.join(", ")
            ));
        }
        if !extra.is_empty() {
            out.push(format!(
                "  {kind} in .morph but not in IR: {}",
                extra.join(", ")
            ));
        }
    }

    let mut lines = Vec::new();
    diff(
        &mut lines,
        "predicates",
        parsed.predicates.iter().map(|p| p.name.as_str()).collect(),
        registry
            .predicates
            .iter()
            .map(|p| p.name.as_str())
            .collect(),
    );
    diff(
        &mut lines,
        "intents",
        parsed.intents.iter().map(|i| i.name.as_str()).collect(),
        registry.intents.iter().map(|i| i.name.as_str()).collect(),
    );
    diff(
        &mut lines,
        "invariants",
        parsed.invariants.iter().map(|i| i.name.as_str()).collect(),
        registry
            .invariants
            .iter()
            .map(|i| i.name.as_str())
            .collect(),
    );
    diff(
        &mut lines,
        "transformations",
        parsed
            .transformations
            .iter()
            .map(|t| t.name.as_str())
            .collect(),
        registry
            .transformations
            .iter()
            .map(|t| t.name.as_str())
            .collect(),
    );
    diff(
        &mut lines,
        "derived_claims",
        parsed
            .derived_claims
            .iter()
            .map(|d| d.predicate.as_str())
            .collect(),
        registry
            .derived_claims
            .iter()
            .map(|d| d.predicate.as_str())
            .collect(),
    );

    // Names line up as sets: the difference is in declaration order, a
    // body, or an arg list. (`Program` equality is order-sensitive.)
    if lines.is_empty() {
        let order = |kind: &str, m: Vec<&str>, r: Vec<&str>, out: &mut Vec<String>| {
            if m != r {
                out.push(format!(
                    "  {kind} declared in a different order:\n    .morph: {}\n    IR:     {}",
                    m.join(", "),
                    r.join(", "),
                ));
            }
        };
        order(
            "predicates",
            parsed.predicates.iter().map(|p| p.name.as_str()).collect(),
            registry
                .predicates
                .iter()
                .map(|p| p.name.as_str())
                .collect(),
            &mut lines,
        );
        order(
            "invariants",
            parsed.invariants.iter().map(|i| i.name.as_str()).collect(),
            registry
                .invariants
                .iter()
                .map(|i| i.name.as_str())
                .collect(),
            &mut lines,
        );
        order(
            "transformations",
            parsed
                .transformations
                .iter()
                .map(|t| t.name.as_str())
                .collect(),
            registry
                .transformations
                .iter()
                .map(|t| t.name.as_str())
                .collect(),
            &mut lines,
        );
        for p in &registry.predicates {
            if parsed.predicates.iter().any(|x| x.name == p.name && x != p) {
                lines.push(format!("  predicate `{}` declaration differs", p.name));
            }
        }
        for inv in &registry.invariants {
            if parsed
                .invariants
                .iter()
                .any(|x| x.name == inv.name && x != inv)
            {
                lines.push(format!("  invariant `{}` body differs", inv.name));
            }
        }
        for t in &registry.transformations {
            if parsed
                .transformations
                .iter()
                .any(|x| x.name == t.name && x != t)
            {
                lines.push(format!("  transformation `{}` body differs", t.name));
            }
        }
    }
    lines.join("\n")
}

/// The workspace `examples/` directory, relative to this crate.
fn examples_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

/// Parse every `examples/*/*.morph`, keyed by parsed program name.
/// Panics on a parse failure or two sources sharing a program name.
fn parsed_example_sources() -> BTreeMap<String, Program> {
    let mut out: BTreeMap<String, Program> = BTreeMap::new();
    for dir_entry in fs::read_dir(examples_dir()).expect("read examples/") {
        let dir = dir_entry.expect("examples/ entry").path();
        if !dir.is_dir() {
            continue;
        }
        for file_entry in fs::read_dir(&dir).expect("read example dir") {
            let path = file_entry.expect("example dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("morph") {
                continue;
            }
            let source = fs::read_to_string(&path).expect("read .morph source");
            let program = parse_program(&source).unwrap_or_else(|diagnostics| {
                let msgs: Vec<String> = diagnostics.iter().map(|d| d.message.clone()).collect();
                panic!("{} failed to parse:\n{}", path.display(), msgs.join("\n"));
            });
            assert!(
                out.insert(program.name.clone(), program).is_none(),
                "two example .morph files parse to the same program name (at {})",
                path.display(),
            );
        }
    }
    out
}

#[test]
fn every_example_morph_matches_its_registered_ir() {
    let from_morph = parsed_example_sources();
    let registry: BTreeMap<String, Program> = all_programs()
        .into_iter()
        .map(|p| (p.name.clone(), p))
        .collect();

    let mut failures: Vec<String> = Vec::new();

    // Every registered program has a faithful teaching source.
    for (name, program) in &registry {
        match from_morph.get(name) {
            None => failures.push(format!(
                "`{name}`: registered, but no example .morph parses to it"
            )),
            Some(parsed) if parsed != program => failures.push(format!(
                "`{name}`: the example .morph drifted from its registered IR\n{}",
                describe_drift(parsed, program),
            )),
            Some(_) => {}
        }
    }

    // No orphan teaching source: every parsed .morph is registered.
    for name in from_morph.keys() {
        if !registry.contains_key(name) {
            failures.push(format!(
                "`{name}`: example .morph parses, but is not in all_programs()"
            ));
        }
    }

    assert!(
        failures.is_empty(),
        "surface-source faithfulness failed for {} example(s):\n\n{}",
        failures.len(),
        failures.join("\n\n"),
    );
}
