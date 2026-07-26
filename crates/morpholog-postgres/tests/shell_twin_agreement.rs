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

#[test]
fn the_shell_twin_agrees_with_the_rust_rule() {
    let mut filled = 0;
    for script in ["scripts/precommit.sh", "scripts/sqlx-prepare.sh"] {
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
