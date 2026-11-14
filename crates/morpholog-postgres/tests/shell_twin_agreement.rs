//! The shell twin of `with_connection_defaults` must not drift from it.
//!
//! `precommit.sh` and `sqlx-prepare.sh` both source `sqlx_url` from
//! `scripts/shared/sqlx_url.sh`, because the external `sqlx-cli` cannot
//! call the Rust one. Two copies of one rule drift silently, so these
//! tests run both over the same cases.

#![expect(
    clippy::expect_used,
    reason = "helper functions here fail fast on a broken fixture, and clippy exempts only `#[test]` functions"
)]

use std::process::Command;

/// Every shape the Rust unit tests cover, plus two that once diverged.
const CASES: &[&str] = &[
    "postgres:///morpholog_dev",
    "postgres://localhost:5432/db",
    "postgres://carol:pw@host:5432/db",
    "postgres:///db?user=carol",
    "postgres:///db?user",
    "postgres:///weird@name",
    "postgres:///db?clusteruser=x",
    "postgres:///db?superuser=1",
    "postgres:///db?sslmode=disable",
    "postgres:///db?port=55432",
    "postgres://:55432/db",
    "postgres://:5432/db?port=55432",
    "postgres:///db?port=55432&host=/tmp",
    "postgres:///db?port=55432&hostaddr=127.0.0.1",
    "postgres:///db?hostx=1&port=55432",
    "postgres://localhost:55432/db",
    "not-a-url",
];

fn shell_sqlx_url_as(script: &str, url: &str, user: &str) -> String {
    let program = format!(
        "set -euo pipefail\n\
         source <(sed -n '/^sqlx_url()/,/^}}/p' {script})\n\
         sqlx_url \"$1\"",
    );
    let out = Command::new("bash")
        .args(["-c", &program, "--", url])
        .env("PGUSER", user)
        .output()
        .expect("bash runs");
    assert!(out.status.success(), "{script} failed for PGUSER={user}");
    String::from_utf8(out.stdout).expect("utf8")
}

fn shell_sqlx_url(script: &str, url: &str) -> String {
    // Source just the function out of the script, then call it. `set -u`
    // is on so an unset-variable regression fails here too.
    let program = format!(
        "set -euo pipefail\n         source <(sed -n '/^sqlx_url()/,/^}}/p' {script})\n         sqlx_url \"$1\"",
    );
    // The child inherits the environment, so both sides see the same
    // username.
    let out = Command::new("bash")
        .args(["-c", &program, "--", url])
        .output()
        .expect("bash runs");
    assert!(
        out.status.success(),
        "{script} sqlx_url failed for {url}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8")
}

/// Hostile usernames, compared against the explicit-user form so the
/// environment is not in the way. An unencoded `&` here would append a
/// real connection option.
const HOSTILE_USERS: &[&str] = &[
    "alice",
    "ops&sslmode=disable",
    "ops#blue",
    "ops user",
    "ops%20already",
    "user@server",
    "acc:ount",
];

#[test]
fn the_twin_encodes_hostile_usernames_identically() {
    for script in ["scripts/shared/sqlx_url.sh"] {
        let path = format!("{}/../../{script}", env!("CARGO_MANIFEST_DIR"));
        for user in HOSTILE_USERS {
            let shell = shell_sqlx_url_as(&path, "postgres:///db", user);
            let rust = morpholog_postgres::with_connection_defaults_as("postgres:///db", user);
            assert_eq!(shell, rust, "{script} disagrees on PGUSER={user}");
        }
    }
}

#[test]
fn the_encoded_url_parses_back_to_the_intended_username() {
    // What matters is that the driver reads the username we meant, not
    // that the text looks right.
    for user in HOSTILE_USERS {
        let url = morpholog_postgres::with_connection_defaults_as("postgres:///db", user);
        let opts: sqlx::postgres::PgConnectOptions = url
            .parse()
            .unwrap_or_else(|e| panic!("{url} must parse: {e}"));
        assert_eq!(
            opts.get_username(),
            *user,
            "{url} parsed to a different username than intended"
        );
    }
}

#[test]
fn an_unencoded_username_would_have_smuggled_in_an_option() {
    // Without encoding, the `&` appends a real parameter.
    let naive = format!("postgres:///db?user={}", "ops&sslmode=disable");
    let encoded =
        morpholog_postgres::with_connection_defaults_as("postgres:///db", "ops&sslmode=disable");
    assert!(naive.contains("&sslmode=disable"), "the naive form injects");
    assert!(
        !encoded.contains("&sslmode="),
        "the encoded form must not: {encoded}"
    );
}

#[test]
fn both_scripts_source_the_shared_twin() {
    let root = format!("{}/../..", env!("CARGO_MANIFEST_DIR"));

    // Present here is not present for everyone: a file that .gitignore
    // excludes passes locally and is missing in CI.
    assert!(
        std::path::Path::new(&format!("{root}/scripts/shared/sqlx_url.sh")).exists(),
        "the shared twin is missing from the working tree"
    );
    let tracked = std::process::Command::new("git")
        .args(["ls-files", "--error-unmatch", "scripts/shared/sqlx_url.sh"])
        .current_dir(&root)
        .output()
        .expect(
            "this check needs git and a checkout; every context that runs these tests has both",
        );
    assert!(
        tracked.status.success(),
        "the shared twin is not tracked by git, so a clean checkout has no copy of it"
    );

    // A script that stopped sourcing the shared file, or grew its own
    // copy, would pass the tests above while diverging in use.
    for script in ["scripts/precommit.sh", "scripts/sqlx-prepare.sh"] {
        let text = std::fs::read_to_string(format!("{root}/{script}")).expect("script readable");
        assert!(
            text.contains("shared/sqlx_url.sh"),
            "{script} must source the shared twin"
        );
        assert!(
            !text.contains("sqlx_url() {"),
            "{script} grew its own copy of the twin"
        );
    }
}

#[test]
fn the_shell_twin_agrees_with_the_rust_rule() {
    let mut filled = 0;
    for script in ["scripts/shared/sqlx_url.sh"] {
        let path = format!("{}/../../{script}", env!("CARGO_MANIFEST_DIR"));
        for url in CASES {
            let shell = shell_sqlx_url(&path, url);
            let rust = morpholog_postgres::with_connection_defaults(url);
            assert_eq!(
                shell, rust,
                "{script} disagrees with with_connection_defaults on {url}"
            );
            if rust != *url {
                filled += 1;
            }
        }
    }
    // With no username in the environment every case returns unchanged,
    // so require that the fill-in branch actually ran.
    assert!(
        filled > 0,
        "no case filled in a username, so this test proved nothing - set \
         USER or PGUSER"
    );
}

/// The socket branch, on any machine: a file named as a server socket for
/// a port nothing uses, in the last directory sqlx looks in, so both sides
/// must find it and name that directory as the host. With `PGHOST` set,
/// both must leave the host alone instead.
#[test]
fn the_twin_names_the_socket_directory_identically() {
    let port = 40_000 + std::process::id() % 20_000;
    let socket = std::path::PathBuf::from(format!("/tmp/.s.PGSQL.{port}"));
    std::fs::write(&socket, "").expect("a file in /tmp");
    let url = format!("postgres:///db?port={port}&user=u");
    let path = format!(
        "{}/../../scripts/shared/sqlx_url.sh",
        env!("CARGO_MANIFEST_DIR")
    );
    let shell = shell_sqlx_url(&path, &url);
    let rust = morpholog_postgres::with_connection_defaults(&url);
    std::fs::remove_file(&socket).expect("cleanup");
    assert_eq!(shell, rust, "the twin disagrees on {url}");
    let pghost = ["PGHOSTADDR", "PGHOST"]
        .iter()
        .any(|key| std::env::var(key).is_ok_and(|v| !v.is_empty()));
    let dir = ["/var/run/postgresql", "/private/tmp", "/tmp"]
        .into_iter()
        .find(|d| {
            *d == "/tmp"
                || std::path::Path::new(d)
                    .join(format!(".s.PGSQL.{port}"))
                    .exists()
        })
        .expect("/tmp is a candidate");
    let expected = if pghost {
        url.clone()
    } else {
        format!("{url}&host={dir}")
    };
    assert_eq!(rust, expected);
    if !pghost {
        let opts: sqlx::postgres::PgConnectOptions = rust.parse().expect("the filled URL parses");
        assert_eq!(
            opts.get_socket().map(|p| p.to_string_lossy().into_owned()),
            Some(dir.to_string()),
            "sqlx connects through the socket directory"
        );
        assert_eq!(opts.get_port(), u16::try_from(port).unwrap());
    }
}
