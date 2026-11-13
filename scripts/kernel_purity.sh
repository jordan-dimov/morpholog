#!/usr/bin/env bash
# Ambient clock and randomness never supply a governed value or affect a
# decision in the kernel: `new Subject()` takes its subjects from a source
# the caller supplies, and nothing else in `morpholog-core` may source a
# value from the outside world. (Its hash maps still seed from host
# randomness, which changes no result.) Nor does the kernel touch files,
# standard input or output, the network, other processes, the environment
# or threads. This script holds the two checks that need no build:
#
#   1. no randomness crate among the kernel's normal dependencies
#      (dev-dependencies such as proptest are free to bring their own);
#   2. no file, standard I/O, network, process, environment or thread
#      module named in its source at all.
#
# Clippy holds the rest, with every call resolved to its definition, so a
# renamed import or a re-export cannot slip past it: the clock, environment,
# file, stream, thread and process functions and types listed in
# `crates/morpholog-core/clippy.toml`, and printing (lints at the crate root).
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."

status=0
deps=$(cargo tree -p morpholog-core -e normal --prefix none --locked | awk '{print $1}' | sort -u)
for crate in uuid rand rand_core getrandom fastrand; do
    if grep -qx "$crate" <<< "$deps"; then
        echo "morpholog-core depends on $crate; the kernel may not source randomness" >&2
        status=1
    fi
done
# A path (`std::fs`), a grouped import (`use std::{fs, io}`, possibly
# across lines, so `fs::read` after it is caught too), or `std` itself
# under another name.
# Exit 0 = found, 1 = none; anything else means the check itself broke.
outside_world() {
    git ls-files -z -- 'crates/morpholog-core/src/*.rs' | perl -e '
        my $mods = qr/\b(?:fs|io|net|process|env|thread)\b/;
        my $found = 0;
        local $/ = "\0";
        for my $file (<STDIN>) {
            chomp $file;
            open(my $fh, "<", $file) or die "cannot read $file: $!";
            my $src = do { local $/; <$fh> };
            while ($src =~ /std\s*::\s*(?:$mods|\{((?:[^{}]|\{[^{}]*\})*)\})|\bstd\s+as\b/g) {
                my ($at, $text, $group) = ($-[0], $&, $1);
                next if defined $group && $group !~ $mods;
                my $line = 1 + (substr($src, 0, $at) =~ tr/\n//);
                $text =~ s/\s+/ /g;
                print "$file:$line: $text\n";
                $found = 1;
            }
        }
        exit($found ? 0 : 1);'
}
set +e
outside_world
found=$?
set -e
case "$found" in
    0)
        echo 'morpholog-core reaches the outside world above; a decision may read only what it is given' >&2
        status=1
        ;;
    1) ;;
    *)
        echo "the outside-world check failed to run (exit $found)" >&2
        status=1
        ;;
esac
exit "$status"
