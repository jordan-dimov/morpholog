//! The shell twin of `with_default_user` must not drift from it.
//!
//! `precommit.sh` and `sqlx-prepare.sh` each carry a `sqlx_url` shell
//! function, because the external `sqlx-cli` cannot call our Rust one.
//! Two implementations of one rule drift silently - and did: an earlier
//! cut treated an `@` anywhere in the URL as credentials, so a database
//! named `weird@name` skipped the fill-in and connected as `anonymous`,
//! while the Rust side handled it correctly. This pins the agreement
//! rather than asserting it a third time.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

/// Every shape the Rust unit tests pin, plus the two a review caught.
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
    // The child INHERITS the environment: overriding it here compared two
    // environments rather than one rule, and the two disagreed on the
    // username while agreeing perfectly on the substitution.
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
            let rust = morpholog_postgres::with_user("postgres:///db", user);
            assert_eq!(shell, rust, "{script} disagrees on PGUSER={user}");
        }
    }
}

#[test]
fn the_encoded_url_parses_back_to_the_intended_username() {
    // The property that actually matters: not that the string LOOKS
    // right, but that the driver reads the username we meant. A test
    // comparing text alone would pass on a URL sqlx misinterprets.
    for user in HOSTILE_USERS {
        let url = morpholog_postgres::with_user("postgres:///db", user);
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
    // Demonstrating the bug the encoding closes, rather than asserting
    // only that the fix works: the naive form appends a real parameter.
    let naive = format!("postgres:///db?user={}", "ops&sslmode=disable");
    let encoded = morpholog_postgres::with_user("postgres:///db", "ops&sslmode=disable");
    assert!(naive.contains("&sslmode=disable"), "the naive form injects");
    assert!(
        !encoded.contains("&sslmode="),
        "the encoded form must not: {encoded}"
    );
}

#[test]
fn both_scripts_source_the_shared_twin() {
    let root = format!("{}/../..", env!("CARGO_MANIFEST_DIR"));

    // Present on this machine is not the same as present for everyone: a
    // stock Python .gitignore excludes any directory named `lib`, so the
    // first home of this file was silently skipped by `git add -A` and
    // every test here passed locally against a file CI never received.
    let tracked = std::process::Command::new("git")
        .args(["ls-files", "--error-unmatch", "scripts/shared/sqlx_url.sh"])
        .current_dir(&root)
        .output()
        .expect("git runs");
    assert!(
        tracked.status.success(),
        "the shared twin is not tracked by git, so a clean checkout has no copy of it"
    );

    // The agreement tests check one shared file; this is what keeps that
    // from being a dodge - a script that stopped sourcing it, or grew its
    // own copy, would pass those tests while diverging in use.
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
            let rust = morpholog_postgres::with_default_user(url);
            assert_eq!(
                shell, rust,
                "{script} disagrees with with_default_user on {url}"
            );
            if rust != *url {
                filled += 1;
            }
        }
    }
    // With no username in the environment at all, every case returns
    // unchanged and the comparison holds vacuously - so require that the
    // fill-in branch was actually exercised.
    assert!(
        filled > 0,
        "no case filled in a username, so this test proved nothing - set \
         USER or PGUSER"
    );
}
