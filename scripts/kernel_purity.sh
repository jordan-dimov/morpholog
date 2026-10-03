#!/usr/bin/env bash
# The kernel reads no clock and no randomness: `new Subject()` takes its
# subjects from a source the caller supplies, and nothing else in
# `morpholog-core` may reach for the outside world. A mechanical guard,
# not a proof - Rust can always reach `std::time` - in two parts:
#
#   1. no randomness crate among the kernel's normal dependencies
#      (dev-dependencies such as proptest are free to bring their own);
#   2. no known ambient read in the kernel's source.
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
    echo 'morpholog-core reads the clock or randomness above; take it as an input instead' >&2
    status=1
fi
exit "$status"
