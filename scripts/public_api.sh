#!/usr/bin/env bash
# The public Rust API of every library crate, as committed text under
# api/. Check mode (the default) regenerates each snapshot and fails on
# any difference; `--update` writes them. A change to the public
# surface is then an ordinary diff of api/ in the PR that makes it.
#
# Hidden items (`#[doc(hidden)]`) are rendered too: another crate can
# still call them, so a change to one is a change to what compiles
# against us. That includes the standard library's hidden derive
# internals; they move only when the pinned nightly does.
#
# The crate list is explicit: a crate is API because it is named here,
# never because it has a lib target. The toolchain that renders
# rustdoc JSON is pinned here and nowhere else; stable and the declared
# floor remain the compiler contract. Bumping either pin is its own
# commit, and the snapshots move with it.
set -euo pipefail

NIGHTLY=nightly-2026-09-17
CARGO_PUBLIC_API_VERSION=0.52.0
CRATES=(
    morpholog-core
    morpholog-postgres
    morpholog-cli
    morpholog-witness
    morpholog-surface
    morpholog-outbox
)

cd "$(dirname "$0")/.."

mode=check
case "${1:-}" in
    "") ;;
    --update) mode=update ;;
    *) echo "usage: $0 [--update]" >&2; exit 2 ;;
esac

if ! rustup toolchain list | awk '{print $1}' | grep -q "^${NIGHTLY}-"; then
    echo "error: the pinned toolchain $NIGHTLY is not installed. Install it with:" >&2
    echo "  rustup toolchain install $NIGHTLY --profile minimal" >&2
    exit 1
fi
installed=$(cargo public-api --version 2>/dev/null | awk '{print $2}' || true)
if [ "$installed" != "$CARGO_PUBLIC_API_VERSION" ]; then
    echo "error: cargo-public-api $CARGO_PUBLIC_API_VERSION is required (found: ${installed:-none}). Install it with:" >&2
    echo "  cargo install cargo-public-api --locked --version $CARGO_PUBLIC_API_VERSION" >&2
    exit 1
fi

# Every snapshot belongs to a listed crate: a stale file for a crate
# taken off the list is an error, not a silent leftover.
for f in api/*.txt; do
    [ -e "$f" ] || continue
    name=$(basename "$f" .txt)
    if ! printf '%s\n' "${CRATES[@]}" | grep -qx "$name"; then
        echo "error: $f has no crate on the list in $0" >&2
        exit 1
    fi
done

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p api
failed=()
for crate in "${CRATES[@]}"; do
    RUSTDOCFLAGS="-Zunstable-options --document-hidden-items" \
        cargo "+$NIGHTLY" public-api \
        --manifest-path "crates/$crate/Cargo.toml" \
        --all-features --simplified --color never \
        > "$tmp/$crate.txt" 2> "$tmp/$crate.err" || {
        cat "$tmp/$crate.err" >&2
        echo "error: rendering the public API of $crate failed" >&2
        exit 1
    }
    if [ "$mode" = update ]; then
        cp "$tmp/$crate.txt" "api/$crate.txt"
    elif [ ! -e "api/$crate.txt" ]; then
        echo "error: $crate is on the list but api/$crate.txt does not exist" >&2
        failed+=("$crate")
    elif ! diff -u "api/$crate.txt" "$tmp/$crate.txt" > "$tmp/$crate.diff"; then
        failed+=("$crate")
        cat "$tmp/$crate.diff" >&2
    fi
done

if [ "${#failed[@]}" -gt 0 ]; then
    echo >&2
    echo "error: the public Rust API changed in: ${failed[*]}" >&2
    echo "If the change is intended, regenerate and commit the snapshots with it:" >&2
    echo "  ./scripts/public_api.sh --update" >&2
    exit 1
fi
[ "$mode" = update ] && echo "wrote api/ for ${#CRATES[@]} crates" || echo "public Rust API unchanged for ${#CRATES[@]} crates"
