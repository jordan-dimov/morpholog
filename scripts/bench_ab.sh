#!/usr/bin/env bash
# Interleaved baseline-against-candidate runs of the bench suite, with
# the provenance recorded beside the evidence.
#
# docs/benchmarking.md asks a performance claim for at least four suite
# runs a side, interleaved in balanced order, the order and the
# machine's state recorded, judged by `morpholog-bench compare`. This
# script produces exactly that:
#
#   - the baseline is <baseline-rev>, built from a temporary git worktree
#     into a private target directory; the candidate is the current
#     working tree, uncommitted changes included, built into another;
#   - both binaries are copied into the session directory and run from
#     there, so no rebuild can swap one mid-session;
#   - the runs go in pairs, A B | B A | A B | B A, each after the machine
#     has been idle and cool for a sustained interval;
#   - every run's order, timing, machine state, throttled time and report
#     hash go into manifest.tsv, the session's identity into session.txt,
#     and the candidate's changes into candidate.patch.
#
# A laptop that turbos to its temperature limit throttles during every
# run. That is not contamination while both sides throttle alike, and
# interleaving is what makes them alike, so the session is flagged only
# when the two sides' throttled shares differ markedly. `--max-mhz` caps
# the clock for the session (it needs sudo, asked for up front, and the
# original limits are restored on exit) so the chip never reaches its
# limit: lower absolute numbers, much less noise between runs.
#
# Both binaries run against the one database named by DATABASE_URL, which
# the suite truncates. The runner does not migrate it: a baseline and
# candidate that need different schemas cannot be compared this way, and
# the run that fails aborts the session.
#
# Exit status: 0 when the session completed clean; 1 when the runner, a
# build or a run failed; 2 when the session completed but its evidence is
# suspect - the machine's configuration changed mid-session, the sides
# throttled unevenly, or throttling could not be observed. The compare
# table is printed in every completed case.
#
# Manual and exploratory, like the scale bench: never a CI step.
#
# Usage:
#   DATABASE_URL=postgres:///morpholog_bench \
#     ./scripts/bench_ab.sh <baseline-rev> [--runs N] [--max-mhz MHZ] \
#       [--idle-above PCT] [--cool-below C] [--quiet-for S] \
#       [--quiet-timeout S] [--uneven-throttle PP] [-- <suite args>]

set -euo pipefail

usage() {
    sed -n '/^# Usage:/,/^$/p' "$0" | sed 's/^# \{0,1\}//' >&2
}

fail() {
    echo "error: $*" >&2
    exit 1
}

say() { echo "bench_ab: $*" >&2; }

cd "$(dirname "${BASH_SOURCE[0]}")/.."

BASELINE_REV=""
RUNS=4
MAX_MHZ=""
IDLE_ABOVE=95
COOL_BELOW=60
QUIET_FOR=5
QUIET_TIMEOUT=1800
UNEVEN_THROTTLE=5
SUITE_ARGS=(--ladder quick --repeat 5)

while [[ $# -gt 0 ]]; do
    case "$1" in
        -h | --help) usage; exit 0 ;;
        --runs) RUNS="${2:?--runs takes a number}"; shift 2 ;;
        --max-mhz) MAX_MHZ="${2:?--max-mhz takes a clock in MHz}"; shift 2 ;;
        --idle-above) IDLE_ABOVE="${2:?--idle-above takes a percentage}"; shift 2 ;;
        --cool-below) COOL_BELOW="${2:?--cool-below takes degrees C}"; shift 2 ;;
        --quiet-for) QUIET_FOR="${2:?--quiet-for takes seconds}"; shift 2 ;;
        --quiet-timeout) QUIET_TIMEOUT="${2:?--quiet-timeout takes seconds}"; shift 2 ;;
        --uneven-throttle) UNEVEN_THROTTLE="${2:?--uneven-throttle takes percentage points}"; shift 2 ;;
        --)
            shift
            SUITE_ARGS=("$@")
            break
            ;;
        -*) fail "unknown option '$1'" ;;
        *)
            [[ -z "$BASELINE_REV" ]] || fail "one baseline rev, got '$BASELINE_REV' and '$1'"
            BASELINE_REV="$1"
            shift
            ;;
    esac
done

[[ -n "$BASELINE_REV" ]] || { usage; fail "a baseline rev is required"; }
: "${DATABASE_URL:?set DATABASE_URL to a disposable database; the suite truncates it}"

whole() { [[ "$1" =~ ^[0-9]+$ ]]; }
# Balanced order needs the same number of first and second positions on
# each side, so an odd count cannot be balanced.
whole "$RUNS" && ((RUNS >= 4 && RUNS % 2 == 0)) ||
    fail "--runs must be an even number of at least 4 (got '$RUNS')"
for pair in "idle-above:$IDLE_ABOVE" "cool-below:$COOL_BELOW" "quiet-for:$QUIET_FOR" \
    "quiet-timeout:$QUIET_TIMEOUT" "uneven-throttle:$UNEVEN_THROTTLE"; do
    whole "${pair#*:}" || fail "--${pair%%:*} takes a whole number (got '${pair#*:}')"
done
((IDLE_ABOVE <= 100)) || fail "--idle-above is a percentage (got '$IDLE_ABOVE')"
[[ -z "$MAX_MHZ" ]] || whole "$MAX_MHZ" || fail "--max-mhz takes a clock in MHz (got '$MAX_MHZ')"

# The runner owns how the evidence is produced: the output format, the
# reset acknowledgement, and the one database.
for arg in "${SUITE_ARGS[@]}"; do
    case "$arg" in
        --format | --format=* | --reset | --database-url | --database-url=*)
            fail "'$arg' is set by the runner, not passed through" ;;
    esac
done

BASELINE_SHA="$(git rev-parse --verify --quiet "${BASELINE_REV}^{commit}")" ||
    fail "'$BASELINE_REV' does not name a commit"

# The candidate is HEAD plus its tracked changes. A file git does not
# track would be built but appear in no patch, so the source state
# behind the binary could not be recovered.
UNTRACKED="$(git ls-files --others --exclude-standard)"
[[ -z "$UNTRACKED" ]] || fail "untracked files would be built but recorded nowhere; \
add them, or 'git add -N' them, before benchmarking:
$UNTRACKED"

FREQ_FILES=(/sys/devices/system/cpu/cpu*/cpufreq/scaling_max_freq)
if [[ -n "$MAX_MHZ" ]]; then
    [[ -e "${FREQ_FILES[0]}" ]] || fail "--max-mhz needs cpufreq, which this machine does not expose"
    say "--max-mhz needs sudo to cap the clock; authenticating now, before the builds"
    sudo -v || fail "sudo is required for --max-mhz"
fi

ROOT="target/bench-ab"
SESSION="$ROOT/$(date -u +%Y%m%dT%H%M%SZ)"
WORKTREE="$ROOT/baseline-src"
mkdir -p "$SESSION"

ORIGINAL_MAX=()
remove_worktree() {
    if git worktree list --porcelain | grep -qx "worktree $(pwd)/$WORKTREE"; then
        git worktree remove --force "$WORKTREE" >/dev/null 2>&1 || true
    fi
}
restore_clock() {
    local i
    for i in "${!ORIGINAL_MAX[@]}"; do
        echo "${ORIGINAL_MAX[$i]}" | sudo tee "${FREQ_FILES[$i]}" >/dev/null || true
    done
    if [[ ${#ORIGINAL_MAX[@]} -gt 0 ]]; then
        say "clock limits restored"
    fi
    ORIGINAL_MAX=()
}
cleanup() {
    remove_worktree
    restore_clock
}
trap cleanup EXIT
remove_worktree
git worktree prune
# Left behind by a session that was killed before its trap ran.
rm -rf "$WORKTREE"

sha() { sha256sum "$1" | cut -d' ' -f1; }

say "session $SESSION"
CANDIDATE_HEAD="$(git rev-parse HEAD)"
git diff --binary HEAD > "$SESSION/candidate.patch"
PATCH_SHA="$(sha "$SESSION/candidate.patch")"
DIRTY=no
if [[ -s "$SESSION/candidate.patch" ]]; then DIRTY=yes; fi

say "building the baseline $BASELINE_SHA"
git worktree add --detach --quiet "$WORKTREE" "$BASELINE_SHA"
(cd "$WORKTREE" && cargo build --release --locked -q -p morpholog-bench \
    --target-dir "$(pwd)/../build-baseline") ||
    fail "the baseline did not build"
cp "$ROOT/build-baseline/release/morpholog-bench" "$SESSION/baseline-morpholog-bench"
remove_worktree

say "building the candidate (HEAD $CANDIDATE_HEAD, dirty: $DIRTY)"
cargo build --release --locked -q -p morpholog-bench --target-dir "$ROOT/build-candidate" ||
    fail "the candidate did not build"
cp "$ROOT/build-candidate/release/morpholog-bench" "$SESSION/candidate-morpholog-bench"
# An edit landing between the patch and the build would put source into
# the binary that the patch does not record.
[[ "$(git rev-parse HEAD)" == "$CANDIDATE_HEAD" &&
    "$(git diff --binary HEAD | sha256sum | cut -d' ' -f1)" == "$PATCH_SHA" ]] ||
    fail "the working tree changed while the candidate was being built"

if [[ -n "$MAX_MHZ" ]]; then
    sudo -v || fail "sudo is required for --max-mhz"
    for f in "${FREQ_FILES[@]}"; do ORIGINAL_MAX+=("$(cat "$f")"); done
    for f in "${FREQ_FILES[@]}"; do
        echo $((MAX_MHZ * 1000)) | sudo tee "$f" >/dev/null || fail "could not cap $f"
    done
    say "clock capped at ${MAX_MHZ} MHz for the session"
fi

# Machine observations. Anything the machine does not expose is NA,
# never zero.
distinct() {
    local v
    v="$(cat "$@" 2>/dev/null | sort -u | paste -sd, -)"
    echo "${v:-NA}"
}
governors() { distinct /sys/devices/system/cpu/cpu*/cpufreq/scaling_governor; }
clock_cap() {
    local v
    v="$(cat "${FREQ_FILES[@]}" 2>/dev/null | sort -u | awk '{ printf "%s%d", sep, $1 / 1000; sep = "," }')"
    echo "${v:-NA}"
}
platform_profile() { distinct /sys/firmware/acpi/platform_profile; }
power_source() {
    local supply seen=""
    for supply in /sys/class/power_supply/*; do
        [[ "$(cat "$supply/type" 2>/dev/null)" == Mains ]] || continue
        seen=yes
        if [[ "$(cat "$supply/online" 2>/dev/null)" == 1 ]]; then
            echo ac
            return
        fi
    done
    if [[ -n "$seen" ]]; then echo battery; else echo NA; fi
}
pkg_temp() {
    local zone
    for zone in /sys/class/thermal/thermal_zone*; do
        if [[ "$(cat "$zone/type" 2>/dev/null)" == x86_pkg_temp ]]; then
            echo $(($(cat "$zone/temp") / 1000))
            return
        fi
    done
    echo NA
}
# Time spent throttled, per CPU, in a fixed order. The per-CPU copies of
# the package counter disagree with each other, so a run's throttled time
# is the largest per-CPU increase, never a sum.
throttle_ms() {
    local files
    files=(/sys/devices/system/cpu/cpu*/thermal_throttle/package_throttle_total_time_ms)
    if [[ -e "${files[0]}" ]]; then
        cat "${files[@]}" | paste -sd' ' -
    else
        echo NA
    fi
}
throttled_between() {
    if [[ "$1" == NA || "$2" == NA ]]; then
        echo NA
        return
    fi
    awk -v a="$1" -v b="$2" 'BEGIN {
        n = split(a, x, " "); split(b, y, " "); m = 0
        for (i = 1; i <= n; i++) if (y[i] - x[i] > m) m = y[i] - x[i]
        print m
    }'
}
# Percentage of CPU time idle over one second, from /proc/stat.
idle_pct() {
    local a b
    a="$(head -1 /proc/stat)"
    sleep 1
    b="$(head -1 /proc/stat)"
    awk -v a="$a" -v b="$b" 'BEGIN {
        n = split(a, x, " "); split(b, y, " "); total = 0
        for (i = 2; i <= n; i++) total += y[i] - x[i]
        idle = (y[5] - x[5]) + (y[6] - x[6])
        printf "%d", (total > 0 ? 100 * idle / total : 0)
    }'
}

wait_for_quiet() {
    local quiet=0 waited=0 idle temp
    while ((waited < QUIET_TIMEOUT)); do
        idle="$(idle_pct)"
        whole "$idle" || fail "could not measure CPU idle time (got '$idle')"
        temp="$(pkg_temp)"
        waited=$((waited + 1))
        if ((idle >= IDLE_ABOVE)) && { [[ "$temp" == NA ]] || ((temp < COOL_BELOW)); }; then
            quiet=$((quiet + 1))
            if ((quiet >= QUIET_FOR)); then
                return 0
            fi
        else
            quiet=0
        fi
    done
    fail "the machine was not idle (>= ${IDLE_ABOVE}%) and cool (< ${COOL_BELOW} C) for \
${QUIET_FOR}s within ${QUIET_TIMEOUT}s; idle ${idle}%, package ${temp} C"
}

PG_VERSION="$(psql "$DATABASE_URL" -Atc 'SHOW server_version' 2>/dev/null || echo NA)"
OBSERVABLE=yes
if [[ "$(throttle_ms)" == NA ]]; then OBSERVABLE=no; fi
{
    echo "baseline_rev=$BASELINE_REV"
    echo "baseline_sha=$BASELINE_SHA"
    echo "candidate_head=$CANDIDATE_HEAD"
    echo "candidate_dirty=$DIRTY"
    echo "candidate_patch_sha256=$PATCH_SHA"
    echo "baseline_binary_sha256=$(sha "$SESSION/baseline-morpholog-bench")"
    echo "candidate_binary_sha256=$(sha "$SESSION/candidate-morpholog-bench")"
    echo "suite_args=${SUITE_ARGS[*]} --reset --format json"
    echo "runs_per_side=$RUNS"
    echo "max_mhz=${MAX_MHZ:-uncapped}"
    echo "quiet_gate=idle>=${IDLE_ABOVE}% package<${COOL_BELOW}C for ${QUIET_FOR}s, timeout ${QUIET_TIMEOUT}s"
    echo "uneven_throttle_points=$UNEVEN_THROTTLE"
    echo "throttle_counters_observable=$OBSERVABLE"
    echo "cpu=$(awk -F': ' '/^model name/ { print $2; exit }' /proc/cpuinfo)"
    echo "kernel=$(uname -r)"
    echo "postgresql=$PG_VERSION"
    rustc -Vv | sed 's/^/rustc: /'
    echo "started=$(date -Is)"
} > "$SESSION/session.txt"

MANIFEST="$SESSION/manifest.tsv"
printf 'seq\tpair\tposition\tlabel\tstarted\tduration_ms\tpower\tprofile\tgovernors\tclock_cap_mhz\tpkg_temp_start\tpkg_temp_end\tthrottled_ms\treport_sha256\n' > "$MANIFEST"

seq=0
machine_states=()
declare -A run_ms=([a]=0 [b]=0) throttled_total=([a]=0 [b]=0)
unobservable=no
for ((pair = 1; pair <= RUNS; pair++)); do
    if ((pair % 2 == 1)); then order=(a b); else order=(b a); fi
    for position in first second; do
        side="${order[0]}"
        order=("${order[@]:1}")
        label="$side$pair"
        seq=$((seq + 1))
        binary="$SESSION/baseline-morpholog-bench"
        if [[ "$side" == b ]]; then binary="$SESSION/candidate-morpholog-bench"; fi

        wait_for_quiet
        power="$(power_source)" profile="$(platform_profile)" gov="$(governors)" cap="$(clock_cap)"
        machine_states+=("$power $profile $gov $cap")
        temp_start="$(pkg_temp)"
        counters_start="$(throttle_ms)"
        started="$(date -Is)"
        t0="$(date +%s%N)"
        say "run $seq/$((2 * RUNS)): $label"
        "$binary" suite "${SUITE_ARGS[@]}" --reset --format json \
            > "$SESSION/$label.json" 2> "$SESSION/$label.err" ||
            fail "run $label failed; see $SESSION/$label.err:
$(tail -5 "$SESSION/$label.err")"
        duration_ms=$((($(date +%s%N) - t0) / 1000000))
        throttled="$(throttled_between "$counters_start" "$(throttle_ms)")"
        run_ms[$side]=$((${run_ms[$side]} + duration_ms))
        if [[ "$throttled" == NA ]]; then
            unobservable=yes
        else
            throttled_total[$side]=$((${throttled_total[$side]} + throttled))
        fi
        printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
            "$seq" "$pair" "$position" "$label" "$started" "$duration_ms" "$power" \
            "$profile" "$gov" "$cap" "$temp_start" "$(pkg_temp)" "$throttled" \
            "$(sha "$SESSION/$label.json")" >> "$MANIFEST"
    done
done

share() { awk -v t="$1" -v r="$2" 'BEGIN { printf "%.1f", (r > 0 ? 100 * t / r : 0) }'; }
SHARE_A="$(share "${throttled_total[a]}" "${run_ms[a]}")"
SHARE_B="$(share "${throttled_total[b]}" "${run_ms[b]}")"

FLAGS=()
if [[ "$(printf '%s\n' "${machine_states[@]}" | sort -u | wc -l)" -gt 1 ]]; then
    FLAGS+=(machine_changed)
fi
if [[ "$unobservable" == yes ]]; then
    FLAGS+=(throttle_observability_unavailable)
elif awk -v a="$SHARE_A" -v b="$SHARE_B" -v t="$UNEVEN_THROTTLE" \
    'BEGIN { d = a - b; if (d < 0) d = -d; exit !(d > t) }'; then
    FLAGS+=(uneven_throttling)
fi
STATUS=clean
if [[ ${#FLAGS[@]} -gt 0 ]]; then STATUS="$(IFS=,; echo "${FLAGS[*]}")"; fi
{
    echo "throttled_share_baseline=${SHARE_A}%"
    echo "throttled_share_candidate=${SHARE_B}%"
    echo "finished=$(date -Is)"
    echo "session_status=$STATUS"
} >> "$SESSION/session.txt"
restore_clock

before=() after=()
for ((pair = 1; pair <= RUNS; pair++)); do
    before+=("$SESSION/a$pair.json")
    after+=("$SESSION/b$pair.json")
done
"$SESSION/candidate-morpholog-bench" compare --before "${before[@]}" --after "${after[@]}" \
    > "$SESSION/compare.md" || fail "compare refused the session's reports"

echo "session_status=$STATUS ($SESSION)"
echo "throttled: baseline ${SHARE_A}% of run time, candidate ${SHARE_B}%"
echo
cat "$SESSION/compare.md"

if [[ "$STATUS" != clean ]]; then
    say "the evidence is suspect ($STATUS); see $MANIFEST"
    exit 2
fi
