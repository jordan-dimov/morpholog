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
# platform, changelog, announcement. Then the register's own record is
# sealed, signed and published beside the downloads. Safe to run again
# after a failure: a step already in the register is checked and
# skipped, and an existing tag is accepted only if it points at the
# commit the gate recorded.
#
# Needs: git, gh (authenticated), jq, cargo, the register database
# (RELEASE_DATABASE_URL, default postgres:///morpholog_release - not a
# test database, which the suites reset), and the register's signing
# key (RELEASE_SIGNING_KEY, default
# ~/.config/morpholog/release-register.pem).
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
KEY_ID=release-register-1
PUBLIC_KEY_FILE=examples/16_release_governance/release-register.pub
SIGNING_KEY="${RELEASE_SIGNING_KEY:-$HOME/.config/morpholog/release-register.pem}"
RECORD="release-record-$TAG"
# The first release that published its record; each later one checks
# the record of the release before it.
FIRST_RECORD=v0.0.20

cd "$(git rev-parse --show-toplevel)"

die() { echo "error: $*" >&2; exit 1; }
say() { echo "==> $*"; }

[ -r "$SIGNING_KEY" ] || die "no signing key at $SIGNING_KEY"

cargo build --quiet --locked -p morpholog-cli
MORPHOLOG=target/debug/morpholog

# ---------------------------------------------------------------- the commit
# Version tags only: main-latest is recreated on every push to main, so
# a local copy of it is routinely stale.
git fetch --quiet origin main 'refs/tags/v*:refs/tags/v*'
if git rev-parse -q --verify "refs/tags/$TAG" > /dev/null; then
    # Resuming: the tag already names the commit. A release tag is an
    # annotated tag, a deliberate act with its own object.
    [ "$(git cat-file -t "refs/tags/$TAG")" = tag ] \
        || die "$TAG is a lightweight tag; release tags are annotated"
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

# The register's whole history must decide as it was decided, under this
# programme and this binary, before anything new is recorded.
rehearsal="$("$MORPHOLOG" rehearse "$MORPH" --database-url "$DB")"
jq -e '.totals | .different_delta + .would_reject + .would_error + .unreplayable == 0' \
    <<< "$rehearsal" > /dev/null \
    || die "the register's history does not rehearse the same under $MORPH: $(jq -c .totals <<< "$rehearsal")"
say "register: its history rehearses the same under this programme and binary"

# verify_record TAG - the record published with TAG, both files, verified
# under this binary with a signature by the key in this repository.
verify_record() {
    local tag="$1" dir
    dir="$(mktemp -d)"
    for name in "release-record-$tag.pack" "release-record-$tag.anchor.json"; do
        gh release download "$tag" -p "$name" -O "$dir/$name" \
            || die "$tag has no published $name"
    done
    "$MORPHOLOG" audit verify-pack "$dir/release-record-$tag.pack" \
        --anchor-file "$dir/release-record-$tag.anchor.json" \
        --require-signatures --require-signing-key "$PUBLIC_KEY_FILE" > "$dir/verdict.json" \
        || die "the record published with $tag does not verify under this binary: $(jq -c .verdict "$dir/verdict.json")"
    rm -rf "$dir"
}

# Every release from the first to publish one carries its record. The
# register's claim that it did is held to the release, never asked
# whether to check.
if [ -n "$previous" ] \
    && [ "$(printf '%s\n%s\n' "$previous" "$FIRST_RECORD" | sort -V | head -1)" = "$FIRST_RECORD" ]; then
    verify_record "$previous"
    has RecordPublished "${previous//./_}" \
        || die "$previous published its record, but the register does not say so"
    say "the record published with $previous verifies under this binary"
fi

# A gate or tag already recorded for another commit means this release
# would name a commit the register never saw pass.
for predicate in GateGreen Tagged; do
    recorded="$(jq -r --arg v "$SUBJECT" --arg p "$predicate" \
        '.[] | select(.predicate == $p and .args[0].value == $v) | .args[1].value' <<< "$claims")"
    [ -z "$recorded" ] || [ "$recorded" = "$SHA" ] \
        || die "the register's $predicate for $TAG is $recorded, not $SHA"
done

# ---------------------------------------------------------------- the gate
ci="$(gh run list --commit "$SHA" --workflow CI --json status,conclusion --jq '.[0] // empty')"
[ -n "$ci" ] || die "no CI run for $SHA yet"
[ "$(jq -r .status <<< "$ci")" = completed ] || die "CI on $SHA is still running"
[ "$(jq -r .conclusion <<< "$ci")" = success ] \
    || die "CI on $SHA concluded $(jq -r .conclusion <<< "$ci")"
step record_gate GateGreen \
    "{\"version\": \"$SUBJECT\", \"commit\": \"$SHA\"}" "$SUBJECT" "$SHA"

# ----------------------------------------------------------------- the tag
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
remote="$(git ls-remote --tags origin "refs/tags/$TAG^{}" | cut -f1)"
[ "$remote" = "$SHA" ] || die "origin's $TAG does not resolve to $SHA after the push"
step tag_release Tagged \
    "{\"version\": \"$SUBJECT\", \"commit\": \"$SHA\"}" "$SUBJECT" "$SHA"

# -------------------------------------------------------------- the assets
# Assets count only if the tag's own workflow run built, smoke-tested
# and published them - whether a release already exists does not matter.
run=""
for _ in $(seq 30); do
    # The push to main runs the same workflow on the same commit, for the
    # rolling build; the tag's run is the one named after the tag.
    run="$(gh run list --workflow release --commit "$SHA" --json databaseId,headBranch \
        | jq -r --arg tag "$TAG" '[.[] | select(.headBranch == $tag)][0].databaseId // empty')"
    [ -n "$run" ] && break
    sleep 10
done
[ -n "$run" ] || die "no release workflow run for $TAG at $SHA"
say "release workflow: run $run"
gh run watch "$run" --exit-status --interval 30 > /dev/null \
    || die "the release workflow did not succeed: gh run view $run --log-failed"
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
        # macOS has shasum, not sha256sum; both read the same checksum file.
        if command -v sha256sum > /dev/null; then sum=(sha256sum); else sum=(shasum -a 256); fi
        (cd "$tmp" && "${sum[@]}" -c --quiet "$archive.sha256" && tar xzf "$archive")
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
# The workflow publishes the notes, a blank line, then its install
# paragraph; anything else in the body was not reviewed.
install="$(git show "$SHA:.github/workflows/release.yml" \
    | grep -o 'Prebuilt binaries for linux[^"]*' | head -1)"
[ -n "$install" ] || die "no install paragraph in the release workflow at $SHA"
body="$(gh release view "$TAG" --json body --jq .body)"
[ "$body" = "$(printf '%s\n\n%s' "$notes" "$install")" ] \
    || die "the published notes are not exactly $NOTES plus the install paragraph; fix the release, not the file"
step record_changelog ChangelogEntry "{\"version\": \"$SUBJECT\"}" "$SUBJECT"
step announce Announced "{\"version\": \"$SUBJECT\"}" "$SUBJECT"

# ---------------------------------------------------------------- the record
# The register seals what it holds, signs the seal, and publishes the
# record beside the downloads. A rerun builds the same files, since the
# seal is the one already made and the export is deterministic, and it
# never replaces a published file that differs. Once the register says
# the record is published, every rerun checks it still is.
public_key="$(cat "$PUBLIC_KEY_FILE")"
step authorise_signing_key AuditSigningKey \
    "{\"key_id\": \"$KEY_ID\", \"public_key\": \"$public_key\"}" \
    "$KEY_ID" audit_checkpoint_v1 "$public_key"
if has RecordPublished "$SUBJECT"; then
    verify_record "$TAG"
    say "$RECORD: still published, signed, verifies"
else
    tmp="$(mktemp -d)"
    newest="$("$MORPHOLOG" inspect audit --database-url "$DB" | tail -1 | jq -r .transition_id)"
    "$MORPHOLOG" audit checkpoint --database-url "$DB" --signing-key "$SIGNING_KEY" \
        --key-id "$KEY_ID" --covering "$newest" > "$tmp/checkpoint.json" \
        || die "the register did not seal its record: $(cat "$tmp/checkpoint.json")"
    jq 'del(.status)' "$tmp/checkpoint.json" > "$tmp/$RECORD.anchor.json"
    "$MORPHOLOG" audit export --database-url "$DB" \
        --tree-size "$(jq -r .tree_size "$tmp/checkpoint.json")" > "$tmp/$RECORD.pack"
    assets="$(gh release view "$TAG" --json assets --jq '[.assets[].name]')"
    mkdir "$tmp/published"
    for name in "$RECORD.pack" "$RECORD.anchor.json"; do
        if ! jq -e --arg n "$name" 'index($n)' <<< "$assets" > /dev/null; then
            gh release upload "$TAG" "$tmp/$name"
            say "uploaded $name"
        fi
        gh release download "$TAG" -p "$name" -O "$tmp/published/$name"
        cmp -s "$tmp/$name" "$tmp/published/$name" \
            || die "$TAG already has a different $name; it is never replaced"
    done
    rm -rf "$tmp"
    verify_record "$TAG"
    say "$RECORD: published, signed, verifies"
    step publish_record RecordPublished "{\"version\": \"$SUBJECT\"}" "$SUBJECT"
fi

say "$TAG released and recorded"
