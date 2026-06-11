//! `morpholog generate python-client` end to end: the emitted package,
//! its determinism (the embedder's whole drift discipline is
//! regenerate-and-diff), the hash stamp, the verbatim-template
//! guarantee, and the whole-run refusal contract.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_morpholog")
}

fn repo_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p
}

fn trade_lifecycle() -> PathBuf {
    repo_root().join("examples/10_trade_lifecycle/trade_lifecycle.morph")
}

fn generate(file: &Path, out: &Path) -> std::process::Output {
    Command::new(bin())
        .args([
            "generate",
            "python-client",
            file.to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ])
        .output()
        .expect("run morpholog generate")
}

const PACKAGE_FILES: [&str; 5] = [
    "__init__.py",
    "models.py",
    "values.py",
    "envelopes.py",
    "adapter.py",
];

#[test]
fn generates_the_five_file_package() {
    let out = tempfile::tempdir().unwrap();
    let result = generate(&trade_lifecycle(), out.path());
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );
    let package = out.path().join("morpholog_client");
    for file in PACKAGE_FILES {
        assert!(package.join(file).is_file(), "{file} should exist");
    }
    // The package directory holds exactly the five files - nothing
    // extra travels (in particular, not the template test suite).
    let count = std::fs::read_dir(&package).unwrap().count();
    assert_eq!(count, PACKAGE_FILES.len(), "exactly the five package files");
}

// Determinism IS the drift contract: the same binary and programme
// produce byte-identical trees, so an embedder's check is
// regenerate-and-diff.
#[test]
fn generation_is_byte_deterministic() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    assert!(generate(&trade_lifecycle(), first.path()).status.success());
    assert!(generate(&trade_lifecycle(), second.path()).status.success());
    for file in PACKAGE_FILES {
        let a = std::fs::read(first.path().join("morpholog_client").join(file)).unwrap();
        let b = std::fs::read(second.path().join("morpholog_client").join(file)).unwrap();
        assert_eq!(a, b, "{file} should be byte-identical across runs");
    }
}

// The three static modules are emitted VERBATIM: the file the template
// test suite runs is the file the embedder receives.
#[test]
fn static_modules_are_byte_equal_to_their_templates() {
    let out = tempfile::tempdir().unwrap();
    assert!(generate(&trade_lifecycle(), out.path()).status.success());
    for file in ["values.py", "envelopes.py", "adapter.py"] {
        let template = std::fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("templates/python_client")
                .join(file),
        )
        .unwrap();
        let emitted = std::fs::read(out.path().join("morpholog_client").join(file)).unwrap();
        assert_eq!(template, emitted, "{file} should be emitted verbatim");
    }
}

// The stamp lets an embedder's CI assert generated code, manifest, and
// live binary all name the same rules.
#[test]
fn the_model_hash_stamp_matches_morpholog_hash() {
    let out = tempfile::tempdir().unwrap();
    assert!(generate(&trade_lifecycle(), out.path()).status.success());
    let hash_out = Command::new(bin())
        .args(["hash", trade_lifecycle().to_str().unwrap()])
        .output()
        .unwrap();
    let report: serde_json::Value = serde_json::from_slice(&hash_out.stdout).unwrap();
    let hash = report["hash"].as_str().unwrap();
    let init = std::fs::read_to_string(out.path().join("morpholog_client/__init__.py")).unwrap();
    assert!(
        init.contains(&format!("MODEL_HASH = \"{hash}\"")),
        "__init__.py should stamp the canonical hash {hash}"
    );
    assert!(
        init.contains(&format!(
            "MORPHOLOG_VERSION = \"{}\"",
            env!("CARGO_PKG_VERSION")
        )),
        "__init__.py should stamp the binary version"
    );
}

fn refusal_fixture(source: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("model.morph");
    std::fs::write(&path, source).unwrap();
    (dir, path)
}

// Refusal is whole-run: every finding is listed, nothing is written.
#[test]
fn refusal_names_every_finding_and_writes_nothing() {
    let (_dir, path) = refusal_fixture(
        "program refusals\n\
         predicate Timed(id: Subject, took: Duration)\n\
         predicate Keyword(id: Subject, class: Decimal)\n\
         transformation t(id, unused):\n    admit Timed(id, duration(PT1H))\n",
    );
    let out = tempfile::tempdir().unwrap();
    let result = generate(&path, out.path());
    assert!(!result.status.success(), "unsupported kinds must refuse");
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("`took` has kind Duration"),
        "the Duration field is named; got:\n{stderr}"
    );
    assert!(
        stderr.contains("`class` is a Python keyword"),
        "the keyword field is named; got:\n{stderr}"
    );
    assert!(
        stderr.contains("`unused` has no single concrete kind"),
        "the unconstrained parameter is named; got:\n{stderr}"
    );
    assert!(
        stderr.contains("nothing was written"),
        "the whole-run contract is stated; got:\n{stderr}"
    );
    assert!(
        !out.path().join("morpholog_client").exists(),
        "refusal must leave the out directory untouched"
    );
}

// A field named after a generated metadata slot would corrupt the
// very ClassVar `submit()` dispatches on; refused like a keyword.
#[test]
fn fields_colliding_with_generated_members_are_refused() {
    let (_dir, path) = refusal_fixture(
        "program members\n\
         predicate Meta(id: Subject, PREDICATE: Subject)\n\
         transformation t(id, PREDICATE):\n    admit Meta(id, PREDICATE)\n",
    );
    let out = tempfile::tempdir().unwrap();
    let result = generate(&path, out.path());
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("`PREDICATE` collides with a generated member name"),
        "got:\n{stderr}"
    );
}

// camel() is many-to-one: `capture_trade` and `CaptureTrade` are
// distinct lawful Morpholog names that render the same Python class.
#[test]
fn class_name_collisions_are_refused_naming_both_sources() {
    let (_dir, path) = refusal_fixture(
        "program collisions\n\
         predicate Logged(id: Subject)\n\
         transformation capture_trade(id):\n    admit Logged(id)\n\
         transformation CaptureTrade(id):\n    admit Logged(id)\n",
    );
    let out = tempfile::tempdir().unwrap();
    let result = generate(&path, out.path());
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains(
            "`CaptureTrade` and `capture_trade` all generate class `CaptureTradeRequest`"
        ) || stderr.contains(
            "`capture_trade` and `CaptureTrade` all generate class `CaptureTradeRequest`"
        ),
        "both sources are named; got:\n{stderr}"
    );
    assert!(
        !out.path().join("morpholog_client").exists(),
        "refusal must leave the out directory untouched"
    );
}

// The committed package under the worked embedder is exactly what
// this binary generates - the drift gate, caught locally under
// precommit before CI's regenerate-and-diff sees it.
#[test]
fn the_committed_example_package_is_current() {
    let out = tempfile::tempdir().unwrap();
    assert!(generate(&trade_lifecycle(), out.path()).status.success());
    let committed = repo_root().join("examples/etrm_embedder/morpholog_client");
    for file in PACKAGE_FILES {
        let fresh = std::fs::read(out.path().join("morpholog_client").join(file)).unwrap();
        let on_disk = std::fs::read(committed.join(file)).unwrap_or_else(|e| {
            panic!("missing committed {file} ({e}); regenerate the example package")
        });
        assert_eq!(
            fresh, on_disk,
            "{file} drifted; regenerate with `morpholog generate python-client \
             examples/10_trade_lifecycle/trade_lifecycle.morph --out examples/etrm_embedder`"
        );
    }
}

// The worked examples that fit the supported kind set all generate;
// the laytime example (Duration-shaped by design) is refused by name -
// the documented consequence of the kind floor.
#[test]
fn laytime_is_refused_for_its_durations() {
    let out = tempfile::tempdir().unwrap();
    let result = generate(
        &repo_root().join("examples/12_laytime_demurrage/laytime.morph"),
        out.path(),
    );
    assert!(!result.status.success());
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("Duration"), "got:\n{stderr}");
}
