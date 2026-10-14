#!/usr/bin/env bash
#
# Compare two builds of dataset_bench on the subsets of the benchmark datasets
# that `bench_datasets.sh subset` makes, by the instructions each run
# executes.
#
# A run's user-space instruction count changes by less than 0.1 % from one run
# to the next, so a few runs resolve a difference of 1 %. Cycles and wall time
# are reported as well, but vary by 3 to 10 %. Instructions do not show I/O
# or waiting, so confirm a change to threading or to file access with
# scripts/bench_datasets.sh.
#
# Each operation runs `reps` times on each build, alternating which build runs
# first. The script reports the median of each and the ratio of B to A.
#
# Needs `perf` with access to hardware counters. bench_fly.sh runs this on the
# Fly machine through its `quick` subcommand.
#
# Usage: scripts/quick_ab.sh <bench-a> <bench-b> <datadir> [reps] [filter]
#
# `filter` is an extended regular expression that selects operations by name,
# for example `arrow` or `rivers`.

set -euo pipefail

A="${1:?usage: quick_ab.sh <bench-a> <bench-b> <datadir> [reps] [filter]}"
B="${2:?usage: quick_ab.sh <bench-a> <bench-b> <datadir> [reps] [filter]}"
DATA="${3:?usage: quick_ab.sh <bench-a> <bench-b> <datadir> [reps] [filter]}"
REPS="${4:-3}"
FILTER="${5:-.}"

# Ubuntu's /usr/bin/perf is a wrapper that refuses to run under a kernel it
# has no tools package for, such as Fly's; the versioned binary runs.
PERF=""
for candidate in /usr/lib/linux-tools/*/perf /usr/lib/linux-tools-*/perf; do
    [[ -x "$candidate" ]] && { PERF="$candidate"; break; }
done
PERF="${PERF:-$(command -v perf || true)}"
[[ -x "$PERF" ]] || { echo "perf not found" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# name | layer | file | query box
DATASETS=(
    "buildings|buildings|ca_buildings.gpkg|-122.52 37.70 -122.35 37.83"
    "rivers|rivers|hydrorivers.gpkg|-70 -10 -60 0"
    "admin|gadm|gadm_noidx.gpkg|0 45 10 55"
)
OPS=(scan arrow1 arrow bbox index write)

# Prints the arguments of one operation; `index` and `write` also prepare
# their files, which is not measured.
op_args() {
    local op="$1" src="$2" layer="$3" box="$4"
    case "$op" in
        scan) echo "scan $src $layer geo 1" ;;
        arrow1) echo "arrow $src $layer 1 1" ;;
        arrow) echo "arrow $src $layer 1 0" ;;
        bbox) echo "bbox $src $layer $box 1" ;;
        index)
            cp "$src" "$WORK/index.gpkg"
            echo "index $WORK/index.gpkg $layer"
            ;;
        write)
            rm -f "$WORK/write.gpkg"
            echo "write $src $layer $WORK/write.gpkg no"
            ;;
    esac
}

# Runs one build once and prints `instructions cycles elapsed_ms`.
measure() {
    local bin="$1" op="$2" src="$3" layer="$4" box="$5" args
    args="$(op_args "$op" "$src" "$layer" "$box")"
    # Word splitting of `args` is intended: it is an argument list.
    # shellcheck disable=SC2086
    "$PERF" stat -x, -e instructions:u,cycles:u -o "$WORK/perf.txt" \
        "$bin" $args >"$WORK/out.txt" || { echo "failed: $bin $args" >&2; exit 1; }
    local instructions cycles elapsed
    instructions="$(grep ',instructions:u,' "$WORK/perf.txt" | cut -d, -f1)"
    cycles="$(grep ',cycles:u,' "$WORK/perf.txt" | cut -d, -f1)"
    elapsed="$(sed -n 's/^elapsed_ms=//p' "$WORK/out.txt")"
    [[ -n $instructions && -n $cycles && -n $elapsed ]] \
        || { echo "no counters for: $bin $args" >&2; exit 1; }
    echo "$instructions $cycles $elapsed"
}

report() {
    python3 - "$@" <<'PY'
import statistics
import sys

label, *rows = sys.argv[1:]
split = rows.index("--")
a = [tuple(map(float, r.split())) for r in rows[:split]]
b = [tuple(map(float, r.split())) for r in rows[split + 1:]]
med = lambda xs, i: statistics.median(x[i] for x in xs)
cells = []
for i, unit in [(0, 1e9), (1, 1e9), (2, 1.0)]:
    ma, mb = med(a, i), med(b, i)
    cells.append(f"{ma / unit:9.3f} {mb / unit:9.3f} {mb / ma:6.3f}")
print(f"{label:18s} {'   '.join(cells)}")
PY
}

echo "A: $A"
echo "B: $B"
echo "reps=$REPS; instructions and cycles in billions, wall time in ms"
printf '%-18s %26s   %26s   %26s\n' "" "instructions A, B, B/A" "cycles A, B, B/A" "wall A, B, B/A"
for ds in "${DATASETS[@]}"; do
    IFS='|' read -r name layer file box <<<"$ds"
    for op in "${OPS[@]}"; do
        label="$op $name"
        [[ $label =~ $FILTER ]] || continue
        ra=() rb=()
        for rep in $(seq 1 "$REPS"); do
            if (( rep % 2 )); then
                ra+=("$(measure "$A" "$op" "$DATA/$file" "$layer" "$box")")
                rb+=("$(measure "$B" "$op" "$DATA/$file" "$layer" "$box")")
            else
                rb+=("$(measure "$B" "$op" "$DATA/$file" "$layer" "$box")")
                ra+=("$(measure "$A" "$op" "$DATA/$file" "$layer" "$box")")
            fi
        done
        report "$label" "${ra[@]}" -- "${rb[@]}"
    done
done
