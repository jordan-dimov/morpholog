//! Shared helpers for the CLI integration suites: the binary path, the
//! repo root, temp `.morph` writers, the connecting role, and released
//! binaries for upgrade tests.

#![allow(
    dead_code,
    reason = "each test binary uses a different subset of these helpers"
)]

use std::path::PathBuf;

pub(crate) fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_morpholog")
}

pub(crate) fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves")
}

/// A temp `.morph` fixture that owns its directory, removed when dropped.
pub(crate) struct Fixture {
    pub path: PathBuf,
    _dir: tempfile::TempDir,
}

impl std::ops::Deref for Fixture {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.path
    }
}

/// Write `content` to a fresh temp `.morph`.
pub(crate) fn write_fixture(name: &str, content: &str) -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join(format!("{name}.morph"));
    std::fs::write(&path, content).expect("write fixture");
    Fixture { path, _dir: dir }
}

/// The suites' connection string, from `DATABASE_URL`, with the default
/// user applied as the binary applies it.
pub(crate) fn database_url() -> String {
    let url = std::env::var("DATABASE_URL").expect(
        "DATABASE_URL must be set for morpholog-cli integration tests \
         (e.g. postgres:///morpholog_dev)",
    );
    morpholog_postgres::with_connection_defaults(&url)
}

/// The same connection string, naming another database.
pub(crate) fn with_database(url: &str, name: &str) -> String {
    let (base, query) = url
        .split_once('?')
        .map_or((url, None), |(b, q)| (b, Some(q)));
    let swapped = match base.rsplit_once('/') {
        Some((prefix, _)) => format!("{prefix}/{name}"),
        None => base.to_string(),
    };
    match query {
        Some(q) => format!("{swapped}?{q}"),
        None => swapped,
    }
}

/// Truncate the schema before a test, so the suites run serially
/// without crosstalk.
pub(crate) async fn reset_db() {
    let pool = morpholog_postgres::PgPool::connect(&database_url())
        .await
        .expect("connect to test DB");
    morpholog_postgres::testing::reset_database(&pool)
        .await
        .expect("truncate");
}

/// The connecting role's name, via a throwaway pool.
pub(crate) async fn session_user(database_url: &str) -> String {
    let pool = morpholog_postgres::PgPool::connect(database_url)
        .await
        .expect("connect");
    let (name,): (String,) = sqlx::query_as("SELECT session_user::text")
        .fetch_one(&pool)
        .await
        .expect("session_user");
    name
}

/// A released binary for this platform, downloaded once into the target
/// directory and checked on every run against `assets`, its published
/// digests: architecture, OS, target triple and the archive's sha256.
/// With no network and no copy yet, the test is skipped, except in CI,
/// where it must run.
pub(crate) fn release_binary(
    release: &str,
    assets: &[(&str, &str, &str, &str)],
) -> Option<PathBuf> {
    let (arch, os) = (std::env::consts::ARCH, std::env::consts::OS);
    let Some(&(_, _, target, digest)) = assets.iter().find(|a| a.0 == arch && a.1 == os) else {
        panic!("no {release} release asset for {arch} {os}");
    };
    let name = format!("morpholog-{release}-{target}");
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(&name);
    std::fs::create_dir_all(&dir).expect("release directory");
    let archive = dir.join(format!("{name}.tar.gz"));
    if !archive.exists() {
        let url = format!(
            "https://github.com/jordan-dimov/morpholog/releases/download/{release}/{name}.tar.gz"
        );
        let partial = dir.join("download.partial");
        let status = std::process::Command::new("curl")
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
        std::fs::rename(&partial, &archive).expect("keep the download");
    }
    use sha2::{Digest as _, Sha256};
    let actual: String = Sha256::digest(std::fs::read(&archive).expect("read the archive"))
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(
        actual,
        digest,
        "{} is not the published asset",
        archive.display()
    );
    let status = std::process::Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&dir)
        .status()
        .expect("spawn tar");
    assert!(status.success(), "unpack {}", archive.display());
    Some(dir.join(&name).join("morpholog"))
}
