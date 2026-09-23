#!/usr/bin/env bash
# Publish a release that the version-bump PR prepared, walking the
# release register (examples/16_release_governance) as it goes:
#
#   scripts/release.sh v0.0.12
#
# The bump PR carries the version, the regenerated client stamp and
# release-notes/<tag>.md, reviewed like code. This script invents no
# prose: the release workflow publishes that file, and this script only
# checks that it did.
#
# Each register step is recorded only after what it claims is verified,
# and in the register's own order: gate, tag, one asset per declared
# platform, changelog, announcement. Safe to run again after a failure:
# a step already in the register is checked and skipped, and an existing
# tag is accepted only if it points at the commit the gate recorded.
#
# Needs: git, gh (authenticated), jq, cargo, and the register database
# (RELEASE_DATABASE_URL, default postgres:///morpholog_release - not a
# test database, which the suites reset).
set -euo pipefail

TAG="${1:-}"
case "$TAG" in
    v[0-9]*.[0-9]*.[0-9]*) ;;
    *) echo "usage: $0 vX.Y.Z" >&2; exit 2 ;;
esac
VERSION="${TAG#v}"
SUBJECT="${TAG//./_}"
DB="${RELEASE_DATABASE_URL:-postgres:///morpholog_release}"
MORPH=examples/16_release_governance/release_governance.morph
NOTES="release-notes/$TAG.md"

cd "$(git rev-parse --show-toplevel)"

die() { echo "error: $*" >&2; exit 1; }
say() { echo "==> $*"; }

cargo build --quiet --locked -p morpholog-cli
MORPHOLOG=target/debug/morpholog

# ---------------------------------------------------------------- the commit
# Version tags only: main-latest is recreated on every push to main, so
# a local copy of it is routinely stale.
git fetch --quiet origin main 'refs/tags/v*:refs/tags/v*'
if git rev-parse -q --verify "refs/tags/$TAG" > /dev/null; then
    # Resuming: the tag already names the commit.
    SHA="$(git rev-parse "$TAG^{commit}")"
    say "$TAG exists at $SHA; resuming"
else
    [ "$(git rev-parse --abbrev-ref HEAD)" = main ] || die "not on main"
    [ -z "$(git status --porcelain)" ] || die "the working tree is not clean"
    [ "$(git rev-parse HEAD)" = "$(git rev-parse origin/main)" ] \
        || die "main is not origin/main; pull first"
    SHA="$(git rev-parse HEAD)"
fi
git merge-base --is-ancestor "$SHA" origin/main || die "$SHA is not on main"

at_sha_version="$(git show "$SHA:Cargo.toml" | sed -n 's/^version = "\(.*\)"$/\1/p' | head -1)"
[ "$at_sha_version" = "$VERSION" ] \
    || die "Cargo.toml at $SHA says $at_sha_version, not $VERSION"
if git cat-file -e "$SHA:$NOTES" 2> /dev/null; then
    notes="$(git show "$SHA:$NOTES")"
elif git rev-parse -q --verify "refs/tags/$TAG" > /dev/null && [ -f "$NOTES" ]; then
    # A release tagged before its notes were committed: check what was
    # published against the file on this branch.
    notes="$(cat "$NOTES")"
else
    die "$NOTES is not in $SHA; it belongs in the bump PR"
fi

# A count of merges stated in the notes must still be true now that the
# bump PR has landed. Fail rather than rewrite reviewed prose.
previous="$(git describe --tags --abbrev=0 --match 'v*' "$SHA^" 2> /dev/null || true)"
stated="$(grep -oE '^[0-9]+ merges since v[0-9]+[.][0-9]+[.][0-9]+' <<< "$notes" | head -1 || true)"
if [ -n "$stated" ] && [ -n "$previous" ]; then
    actual="$(git rev-list --first-parent --count "$previous..$SHA")"
    [ "$stated" = "$actual merges since $previous" ] \
        || die "the notes say '$stated'; it is $actual merges since $previous"
fi

# ------------------------------------------------------------- the register
say "register: $DB"
"$MORPHOLOG" migrate --database-url "$DB" > /dev/null

claims="$("$MORPHOLOG" inspect claims --database-url "$DB")"
refresh_claims() { claims="$("$MORPHOLOG" inspect claims --database-url "$DB")"; }
# has PREDICATE ARG... - is exactly this claim admitted?
has() {
    local predicate="$1" want; shift
    want="$(jq -cn '$ARGS.positional' --args "$@")"
    jq -e --arg p "$predicate" --argjson want "$want" \
        'any(.[]; .predicate == $p and ([.args[].value] == $want))' <<< "$claims" > /dev/null
}
# step TRANSFORMATION PREDICATE JSON-ARGS CLAIM-ARG... - propose once.
step() {
    local transformation="$1" predicate="$2" args="$3"; shift 3
    if has "$predicate" "$@"; then
        say "register: $predicate $* already recorded"
        return
    fi
    local out
    out="$("$MORPHOLOG" propose "$MORPH" "$transformation" --actor releaser \
        --args-named "$args" --database-url "$DB")" \
        || die "register refused $transformation: $(jq -r .reason <<< "$out")"
    say "register: $transformation committed"
    refresh_claims
}

# A gate already recorded for another commit means the tag would name a
# commit the register never saw pass.
recorded="$(jq -r --arg v "$SUBJECT" \
    '.[] | select(.predicate == "GateGreen" and .args[0].value == $v) | .args[1].value' <<< "$claims")"
[ -z "$recorded" ] || [ "$recorded" = "$SHA" ] \
    || die "the register's gate for $TAG is $recorded, not $SHA"

# ---------------------------------------------------------------- the gate
ci="$(gh run list --commit "$SHA" --workflow CI --json status,conclusion --jq '.[0] // empty')"
[ -n "$ci" ] || die "no CI run for $SHA yet"
[ "$(jq -r .status <<< "$ci")" = completed ] || die "CI on $SHA is still running"
[ "$(jq -r .conclusion <<< "$ci")" = success ] \
    || die "CI on $SHA concluded $(jq -r .conclusion <<< "$ci")"
step record_gate GateGreen \
    "{\"version\": \"$SUBJECT\", \"commit\": \"$SHA\"}" "$SUBJECT" "$SHA"

# ----------------------------------------------------------------- the tag
step tag_release Tagged \
    "{\"version\": \"$SUBJECT\", \"commit\": \"$SHA\"}" "$SUBJECT" "$SHA"
if ! git rev-parse -q --verify "refs/tags/$TAG" > /dev/null; then
    git tag -a "$TAG" -m "$TAG" "$SHA"
fi
# The peeled line names the commit an annotated tag points at; a plain
# tag has only the unpeeled one.
remote="$(git ls-remote --tags origin "refs/tags/$TAG" "refs/tags/$TAG^{}" \
    | sort -k2 | tail -1 | cut -f1)"
if [ -z "$remote" ]; then
    git push --quiet origin "refs/tags/$TAG"
    say "pushed $TAG"
elif [ "$remote" != "$SHA" ]; then
    die "origin's $TAG points at $remote, not $SHA"
fi

# -------------------------------------------------------------- the assets
if ! gh release view "$TAG" > /dev/null 2>&1; then
    say "waiting for the release workflow"
    run=""
    for _ in $(seq 30); do
        run="$(gh run list --workflow release --branch "$TAG" --json databaseId --jq '.[0].databaseId // empty')"
        [ -n "$run" ] && break
        sleep 10
    done
    [ -n "$run" ] || die "no release workflow run started for $TAG"
    gh run watch "$run" --exit-status --interval 30 > /dev/null \
        || die "the release workflow failed: gh run view $run --log-failed"
fi
assets="$(gh release view "$TAG" --json assets --jq '[.assets[].name]')"

target_of() {
    case "$1" in
        linux_x86_64) echo x86_64-unknown-linux-musl ;;
        linux_arm64)  echo aarch64-unknown-linux-musl ;;
        macos_arm64)  echo aarch64-apple-darwin ;;
        *) die "no build target known for declared platform $1" ;;
    esac
}
host_target=""
case "$(uname -s)-$(uname -m)" in
    Linux-x86_64)  host_target=x86_64-unknown-linux-musl ;;
    Linux-aarch64) host_target=aarch64-unknown-linux-musl ;;
    Darwin-arm64)  host_target=aarch64-apple-darwin ;;
esac

for platform in $(jq -r '.[] | select(.predicate == "PlatformDeclared") | .args[0].value' <<< "$claims"); do
    target="$(target_of "$platform")"
    archive="morpholog-$TAG-$target.tar.gz"
    for name in "$archive" "$archive.sha256"; do
        jq -e --arg n "$name" 'index($n)' <<< "$assets" > /dev/null \
            || die "$TAG has no asset $name"
    done
    if [ "$target" = "$host_target" ]; then
        tmp="$(mktemp -d)"
        gh release download "$TAG" -p "$archive" -O - > "$tmp/$archive"
        gh release download "$TAG" -p "$archive.sha256" -O - > "$tmp/$archive.sha256"
        (cd "$tmp" && sha256sum -c --quiet "$archive.sha256" && tar xzf "$archive")
        reported="$(find "$tmp" -name morpholog -type f -exec {} --version \;)"
        rm -rf "$tmp"
        [ "$reported" = "morpholog-cli $VERSION" ] \
            || die "$archive reports '$reported', not morpholog-cli $VERSION"
        say "$archive: checksum ok, runs, reports $VERSION"
    fi
    step publish_asset AssetPublished \
        "{\"version\": \"$SUBJECT\", \"platform\": \"$platform\"}" "$SUBJECT" "$platform"
done

# ------------------------------------------------- the notes, and the end
body="$(gh release view "$TAG" --json body --jq .body)"
[ "${body:0:${#notes}}" = "$notes" ] \
    || die "the published notes do not begin with $NOTES; fix the release, not the file"
step record_changelog ChangelogEntry "{\"version\": \"$SUBJECT\"}" "$SUBJECT"
step announce Announced "{\"version\": \"$SUBJECT\"}" "$SUBJECT"

say "$TAG released and recorded"
