//! A programme in parts through the binary: every command that reads a programme takes a
//! `.parts` manifest in place of a `.morph`, hashes it as the concatenated text, and places each
//! finding in the part it points into.

#![expect(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

mod common;
use common::{bin, database_url, reset_db};

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const HEADER: &str = "program shop\n";
const STOCK: &str = "predicate Stock(item: Subject, quantity: Decimal)\n";
const RULE: &str = "-- stock is never negative\n\
                    invariant stock_is_not_negative:\n    \
                    Stock(_, q) implies q >= 0\n";
const ACT: &str = "transformation receive(item, quantity):\n    admit Stock(item, quantity)\n";

/// A directory holding `parts` and a manifest `shop.parts` listing them in order.
fn shop(parts: &[(&str, &str)]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let mut listing = String::from("-- the shop, one programme\n");
    for (name, text) in parts {
        std::fs::write(dir.path().join(name), text).unwrap();
        listing.push_str(name);
        listing.push('\n');
    }
    let manifest = dir.path().join("shop.parts");
    std::fs::write(&manifest, listing).unwrap();
    (dir, manifest)
}

fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("the binary runs")
}

fn text(p: &Path) -> &str {
    p.to_str().unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn hash(path: &Path) -> String {
    let out = run(&["hash", text(path)]);
    assert!(out.status.success(), "{}", stderr(&out));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    report["hash"].as_str().unwrap().to_string()
}

/// The shared loader reads a manifest, and the programme hashes as one file holding the
/// concatenation.
#[test]
fn a_programme_in_parts_hashes_as_its_concatenation() {
    let (dir, manifest) = shop(&[
        ("00_shop.morph", HEADER),
        ("10_stock.morph", &format!("{STOCK}{RULE}")),
        ("20_receive.morph", ACT),
    ]);
    let whole = dir.path().join("whole.morph");
    std::fs::write(&whole, format!("{HEADER}{STOCK}{RULE}{ACT}")).unwrap();
    assert_eq!(hash(&manifest), hash(&whole));
}

#[test]
fn check_places_a_finding_in_its_part_and_lists_the_parts() {
    let (dir, manifest) = shop(&[
        ("00_shop.morph", HEADER),
        ("10_stock.morph", STOCK),
        (
            "20_rule.morph",
            "-- a typo\ninvariant cap:\n    Ghost(_, q) implies q >= 0\n",
        ),
    ]);
    let out = run(&["check", "--json", text(&manifest)]);
    assert!(!out.status.success());
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let part = |name: &str| dir.path().join(name).display().to_string();
    assert_eq!(report["file"], text(&manifest));
    assert_eq!(
        report["parts"],
        serde_json::json!([
            part("00_shop.morph"),
            part("10_stock.morph"),
            part("20_rule.morph")
        ])
    );
    let finding = &report["diagnostics"][0];
    assert_eq!(finding["file"], part("20_rule.morph"), "{report}");
    assert_eq!(finding["line"], 2, "{report}");
    let rule = std::fs::read_to_string(dir.path().join("20_rule.morph")).unwrap();
    let start = usize::try_from(finding["start"].as_u64().unwrap()).unwrap();
    assert!(rule[start..].starts_with("invariant cap"), "{report}");

    let out = run(&["check", text(&manifest)]);
    let rendered = stderr(&out);
    assert!(
        rendered.contains(&format!("{}:2:1", part("20_rule.morph"))),
        "{rendered}"
    );
}

/// Findings in an `--against` programme name its file: in parts, the part.
#[test]
fn check_against_a_programme_in_parts_names_its_part() {
    let (_mine, checked) = shop(&[("00_shop.morph", HEADER), ("10_stock.morph", STOCK)]);
    let (theirs, against) = shop(&[
        ("00_shop.morph", "program warehouse\n"),
        ("10_bin.morph", "predicate Bin(item Subject)\n"),
    ]);
    let out = run(&[
        "check",
        "--json",
        text(&checked),
        "--against",
        text(&against),
    ]);
    assert!(!out.status.success());
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let finding = &report["diagnostics"][0];
    assert_eq!(
        finding["file"],
        theirs.path().join("10_bin.morph").display().to_string(),
        "{report}"
    );
    assert_eq!(finding["line"], 1, "{report}");
}

#[test]
fn check_verbose_lists_the_parts_under_the_programme() {
    let (dir, manifest) = shop(&[("00_shop.morph", HEADER), ("10_stock.morph", STOCK)]);
    let out = run(&["check", "-v", text(&manifest)]);
    assert!(out.status.success(), "{}", stderr(&out));
    let summary = String::from_utf8(out.stdout).unwrap();
    let parts = format!(
        "program: shop\n  parts: {}, {}\n",
        dir.path().join("00_shop.morph").display(),
        dir.path().join("10_stock.morph").display()
    );
    assert!(summary.contains(&parts), "{summary}");
}

#[test]
fn a_refused_manifest_names_its_line() {
    let (_dir, manifest) = shop(&[("00_shop.morph", HEADER)]);
    std::fs::write(&manifest, "00_shop.morph\n../elsewhere.morph\n").unwrap();
    let out = run(&["hash", text(&manifest)]);
    assert!(!out.status.success());
    let message = stderr(&out);
    assert!(
        message.contains(&format!("{}:2:", manifest.display())) && message.contains("`..`"),
        "{message}"
    );
}

/// A refusal through `propose` points at the rule in the part that holds it.
#[tokio::test(flavor = "current_thread")]
async fn propose_locates_a_refusing_rule_in_its_part() {
    reset_db().await;
    let (dir, manifest) = shop(&[
        ("00_shop.morph", HEADER),
        ("10_stock.morph", &format!("{STOCK}\n{RULE}")),
        ("20_receive.morph", ACT),
    ]);
    let out = Command::new(bin())
        .args([
            "propose",
            text(&manifest),
            "receive",
            "--actor",
            "clerk",
            "--args-named",
            r#"{"item": "bolt", "quantity": "-1"}"#,
        ])
        .env("DATABASE_URL", database_url())
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let receipt: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(receipt["status"], "rejected", "{receipt}");
    let rule_at = format!(
        "rule at {}:4:1 (stock_is_not_negative)",
        dir.path().join("10_stock.morph").display()
    );
    assert!(stderr(&out).contains(&rule_at), "{}", stderr(&out));
}

/// Two programmes over one database may list one part of shared
/// declarations: each hashes as its own concatenation, the two declare the
/// shared predicate alike, and `check --against` still reports that both
/// write it.
#[test]
fn two_programmes_may_share_a_part() {
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name);
    std::fs::create_dir(path("shared")).unwrap();
    let files = [
        ("shared/stock.morph", STOCK),
        ("warehouse.morph", "program warehouse\n"),
        ("sales.morph", "program sales\n"),
        ("receive.morph", ACT),
        (
            "sell.morph",
            "transformation sell(item):\n    retract Stock(item, _)\n",
        ),
    ];
    for (name, text) in files {
        std::fs::write(path(name), text).unwrap();
    }
    std::fs::write(
        path("warehouse.parts"),
        "warehouse.morph\nshared/stock.morph\nreceive.morph\n",
    )
    .unwrap();
    std::fs::write(
        path("sales.parts"),
        "sales.morph\nshared/stock.morph\nsell.morph\n",
    )
    .unwrap();
    let concatenation = |name: &str, texts: &[&str]| {
        std::fs::write(path(name), texts.concat()).unwrap();
        hash(&path(name))
    };
    let warehouse = hash(&path("warehouse.parts"));
    assert_eq!(
        warehouse,
        concatenation("w.morph", &["program warehouse\n", STOCK, ACT])
    );
    assert_eq!(
        hash(&path("sales.parts")),
        concatenation(
            "s.morph",
            &[
                "program sales\n",
                STOCK,
                "transformation sell(item):\n    retract Stock(item, _)\n"
            ]
        )
    );

    let out = run(&[
        "check",
        "--json",
        text(&path("warehouse.parts")),
        "--against",
        text(&path("sales.parts")),
    ]);
    assert!(out.status.success(), "{}", stderr(&out));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let findings = report["diagnostics"].as_array().unwrap();
    assert!(
        findings.iter().all(|d| d["severity"] == "hint"),
        "the shared declarations agree: {report}"
    );
    assert!(
        findings.iter().any(|d| d["message"]
            .as_str()
            .unwrap()
            .contains("`Stock` is writable by another programme")),
        "both write Stock: {report}"
    );
}
