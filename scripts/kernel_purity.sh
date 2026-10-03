#!/usr/bin/env bash
# Ambient clock and randomness never supply a governed value or affect a
# decision in the kernel: `new Subject()` takes its subjects from a source
# the caller supplies, and nothing else in `morpholog-core` may source a
# value from the outside world. (Its hash maps still seed from host
# randomness, which changes no result.) Nor does the kernel touch files,
# standard input or output, the network, other processes, the environment
# or threads. A mechanical tripwire, not a proof - Rust can always reach
# `std::time` - in three parts:
#
#   1. no randomness crate among the kernel's normal dependencies
#      (dev-dependencies such as proptest are free to bring their own);
#   2. no known ambient clock or randomness read in the kernel's source;
#   3. no file, standard I/O, network, process, environment or thread
#      access in it, and no printing.
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
if git grep -nE 'SystemTime::now|Instant::now|Timestamp::now|Zoned::now|Uuid::now|thread_rng|getrandom|rand::' \
        -- crates/morpholog-core/src; then
    echo 'morpholog-core reads the clock or randomness above; take the value as an input instead' >&2
    status=1
fi
# A path (`std::fs`), a grouped import (`use std::{fs, io}`, possibly
# across lines, so `fs::read` after it is caught too), or `std` itself
# under another name; and the printing macros.
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
            while ($src =~ /std\s*::\s*(?:$mods|\{((?:[^{}]|\{[^{}]*\})*)\})|\bstd\s+as\b|\b(?:e?print(?:ln)?|dbg)!/g) {
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
