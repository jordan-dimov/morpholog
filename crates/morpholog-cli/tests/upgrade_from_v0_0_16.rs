//! A database the v0.0.16 release wrote, upgraded in place, in the order
//! a real upgrade takes. The release's own binary, downloaded and held to
//! its published digest, writes the history, a checkpoint and a pack.
//! This binary then migrates the database, verifies what the old one
//! wrote, writes on and exports the whole history again. The old binary,
//! connecting afresh, is refused.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;

const RELEASE: &str = "v0.0.16";

/// The release asset for this platform and its sha256, as published.
fn asset() -> (&'static str, &'static str) {
    match (std::env::consts::ARCH, std::env::consts::OS) {
        ("x86_64", "linux") => (
            "x86_64-unknown-linux-musl",
            "3de51232a96398b9bdf006cc1a8abbe4d95737c49dfee752945f84290a9d9e71",
        ),
        ("aarch64", "linux") => (
            "aarch64-unknown-linux-musl",
            "8b061e999a0a32792ffcfd7ef9e81edb1ebe2ce993bc3e1b52344b2b571d56d7",
        ),
        ("aarch64", "macos") => (
            "aarch64-apple-darwin",
            "96ed2f2dcd0c8f40ee7709039d70c264bde96cfb078b35d76e6395d8f9737767",
        ),
        other => panic!("no {RELEASE} release asset for {other:?}"),
    }
}

/// The released binary, downloaded once into the target directory and
/// checked against its digest on every run. With no network and no copy
/// yet, the test is skipped, except in CI, where it must run.
fn old_binary() -> Option<PathBuf> {
    let (target, digest) = asset();
    let name = format!("morpholog-{RELEASE}-{target}");
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(&name);
    std::fs::create_dir_all(&dir).unwrap();
    let archive = dir.join(format!("{name}.tar.gz"));
    if !archive.exists() {
        let url = format!(
            "https://github.com/jordan-dimov/morpholog/releases/download/{RELEASE}/{name}.tar.gz"
        );
        let partial = dir.join("download.partial");
        let status = Command::new("curl")
            .args(["-fsSL", "--retry", "3", "-o"])
            .arg(&partial)
            .arg(&url)
            .status()
            .expect("spawn curl");
        if !status.success() {
            assert!(std::env::var_os("CI").is_none(), "download {url}");
            eprintln!("skipped: could not download {url}");
            return None;
        }
        std::fs::rename(&partial, &archive).unwrap();
    }
    let actual: String = Sha256::digest(std::fs::read(&archive).unwrap())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        actual,
        digest,
        "{} is not the published asset",
        archive.display()
    );
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&dir)
        .status()
        .expect("spawn tar");
    assert!(status.success(), "unpack {}", archive.display());
    Some(dir.join(&name).join("morpholog"))
}

const PROGRAMME: &str = "program upgrade
predicate X(k: Subject)
transformation admit_x(k):
    admit X(k)
transformation retract_x(k):
    require X(k)
    retract X(k)
";

fn run(binary: &Path, url: &str, args: &[&str]) -> Output {
    Command::new(binary)
        .args(args)
        .env("DATABASE_URL", url)
        .output()
        .expect("spawn morpholog")
}

fn json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn lines(ndjson: &[u8]) -> Vec<Value> {
    ndjson
        .split(|b| *b == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect()
}

fn propose(binary: &Path, url: &str, file: &Path, act: &str, k: &str) -> Output {
    let args = format!(r#"[{{"type":"subject","value":"{k}"}}]"#);
    run(
        binary,
        url,
        &[
            "propose",
            file.to_str().unwrap(),
            act,
            "--actor",
            "w",
            "--args",
            &args,
        ],
    )
}

#[tokio::test(flavor = "current_thread")]
async fn a_database_the_last_release_wrote_upgrades_and_verifies() {
    let Some(old) = old_binary() else { return };
    let new = Path::new(common::bin());
    let base = morpholog_postgres::with_default_user(&common::database_url());
    let name = format!("morpholog_cli_upgrade_{}", std::process::id());
    let admin = PgPool::connect(&common::with_database(&base, "postgres"))
        .await
        .unwrap();
    for sql in [
        format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"),
        format!("CREATE DATABASE {name}"),
    ] {
        sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .execute(&admin)
            .await
            .unwrap();
    }
    let url = common::with_database(&base, &name);
    let file = common::write_fixture("upgrade", PROGRAMME);
    let pack = common::write_fixture("pack.ndjson", "");

    json(&run(&old, &url, &["init", "--database-url", &url]));
    for (act, k) in [("admit_x", "a"), ("admit_x", "b"), ("retract_x", "a")] {
        assert_eq!(
            json(&propose(&old, &url, &file, act, k))["status"],
            "committed"
        );
    }
    json(&run(&old, &url, &["audit", "checkpoint"]));
    let exported = run(&old, &url, &["audit", "export"]);
    assert!(exported.status.success());
    std::fs::write(&*pack, &exported.stdout).unwrap();

    let migrated = json(&run(new, &url, &["migrate", "--database-url", &url]));
    let verified = json(&run(new, &url, &["audit", "verify"]));
    let offline = json(&run(
        new,
        &url,
        &["audit", "verify-pack", pack.to_str().unwrap()],
    ));
    let written = json(&propose(new, &url, &file, "admit_x", "c"));
    let verified_after = json(&run(new, &url, &["audit", "verify"]));
    json(&run(new, &url, &["audit", "checkpoint"]));
    let exported_after = run(new, &url, &["audit", "export"]);
    assert!(exported_after.status.success());
    let pack_after = common::write_fixture("pack_after.ndjson", "");
    std::fs::write(&*pack_after, &exported_after.stdout).unwrap();
    let offline_after = json(&run(
        new,
        &url,
        &["audit", "verify-pack", pack_after.to_str().unwrap()],
    ));
    let refused = propose(&old, &url, &file, "admit_x", "d");

    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "DROP DATABASE IF EXISTS {name} WITH (FORCE)"
    )))
    .execute(&admin)
    .await
    .unwrap();

    assert_eq!(migrated["recorded_version_before"], 25, "{migrated}");
    assert_eq!(
        migrated["recorded_version_after"],
        morpholog_postgres::head_version(),
        "{migrated}"
    );
    // The legacy rows, the activation record, then the new write.
    for (report, rows) in [(&verified, 4), (&verified_after, 5)] {
        assert_eq!(report["passes"], true, "{report}");
        assert_eq!(report["replay"]["status"], "consistent", "{report}");
        assert_eq!(report["tree"]["status"], "intact", "{report}");
        assert_eq!(report["replay"]["transitions"], rows, "{report}");
        assert_eq!(report["tree"]["tree_size"], 3, "the checkpoint's: {report}");
        assert_eq!(report["tree"]["checkpoints"], 1, "{report}");
    }
    let old_pack = lines(&exported.stdout);
    assert_eq!(
        old_pack[0]["pack_format_version"],
        morpholog_postgres::LEGACY_PACK_FORMAT
    );
    assert_eq!(offline["passes"], true, "the old pack verifies: {offline}");
    assert_eq!(offline["verdict"]["status"], "intact", "{offline}");
    assert_eq!(offline["verdict"]["tree_size"], 3, "{offline}");
    assert_eq!(written["status"], "committed");

    // A pack of the whole history now: the old checkpoint keeps its root,
    // the activation record is the first ordered row, and the new write
    // follows it under the next order.
    let new_pack = lines(&exported_after.stdout);
    let manifest = &new_pack[0];
    assert_eq!(
        manifest["pack_format_version"],
        morpholog_postgres::PACK_FORMAT
    );
    assert_eq!(manifest["checkpoint_count"], 2, "{manifest}");
    assert_eq!(new_pack[1], old_pack[1], "the old checkpoint is unchanged");
    let rows = &new_pack[3..];
    assert_eq!(rows.len(), 5);
    assert_eq!(&rows[..3], &old_pack[2..], "the legacy rows are unchanged");
    assert!(rows[..3].iter().all(|r| r.get("audit_order").is_none()));
    assert_eq!(rows[3]["transformation_name"], "morpholog.activate");
    assert_eq!(
        (&rows[3]["audit_order"], &rows[3]["act_ordinal"]),
        (&1.into(), &0.into())
    );
    assert_eq!(rows[4]["transformation_name"], "admit_x");
    assert_eq!(rows[4]["audit_order"], 2, "the new write is ordered");
    assert_eq!(offline_after["passes"], true, "{offline_after}");
    assert_eq!(
        offline_after["verdict"]["status"], "intact",
        "{offline_after}"
    );
    assert_eq!(offline_after["verdict"]["tree_size"], 5, "{offline_after}");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        !refused.status.success() && stderr.contains("older than the database"),
        "the old binary is refused: {stderr}"
    );
}
