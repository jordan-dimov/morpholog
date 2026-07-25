//! Shared scaffolding for the CLI integration suites: the binary
//! path, the repo root, temp-`.morph` writers, and the connecting
//! role - each previously copied per test file.

#![allow(dead_code)]

use std::path::PathBuf;

pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_morpholog")
}

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("repo root resolves")
}

/// Write `content` to a fresh temp `.morph` and return its path (kept
/// alive by leaking the TempDir - test-lifetime is process-lifetime).
pub fn write_fixture(name: &str, content: &str) -> PathBuf {
    let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
    let path = dir.path().join(format!("{name}.morph"));
    std::fs::write(&path, content).expect("write fixture");
    path
}

/// The connecting role's name, via a throwaway pool.
pub async fn session_user(database_url: &str) -> String {
    let pool = morpholog_postgres::PgPool::connect(database_url)
        .await
        .expect("connect");
    let (name,): (String,) = sqlx::query_as("SELECT session_user::text")
        .fetch_one(&pool)
        .await
        .expect("session_user");
    name
}
