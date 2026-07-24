#!/usr/bin/env bash
#
# Compare this crate's RTree spatial-index build against GDAL's, like for like.
#
# The earlier comparison in roadmap/benchmarks/ timed `ogr2ogr` copying a whole
# GeoPackage against our write-only path. That is not a fair test of the index
# builders: GDAL was also reading a source file, parsing its geometries and
# writing a new one, none of which we were doing. Every figure derived from it
# had to be quoted with a caveat, which is a sign the measurement was wrong
# rather than the conclusion.
#
# GDAL exposes CreateSpatialIndex and DisableSpatialIndex as SQL functions on an
# existing GeoPackage, so both implementations can be handed the same file with
# the same rows and asked for the same thing: build the index, nothing else.
#
# Method, per size and distribution:
#
#   1. Build one unindexed fixture.
#   2. For each repetition, copy it fresh for each arm so neither sees an index
#      or a warmed file, and alternate which arm runs first so any drift in
#      machine state is shared rather than landing on one arm.
#   3. Time the build as externally observed wall time. Subtract each arm's own
#      measured startup floor: `ogrinfo -sql "SELECT 1"` for GDAL, an open and
#      close for ours. Both raw and adjusted figures are reported.
#   4. Report the median over repetitions, not the mean, so one scheduling
#      hiccup does not move the answer.
#   5. Compare what was built, not just how fast: node count, tree depth, node
#      bytes, and query latency over identical boxes measured with the same
#      reader, so the read path is a constant. A faster build that yields a
#      worse tree is not a win.
#
# What this still does not control: GDAL links its own SQLite and we bundle
# ours, so the two are not executing identical B-tree code, and neither arm
# controls the OS page cache beyond using a fresh copy each time. Both are
# recorded in the output rather than hidden.
#
# Usage: scripts/compare_gdal_index.sh [rows] [reps] [uniform|clustered]

set -euo pipefail

ROWS="${1:-1000000}"
REPS="${2:-5}"
DIST="${3:-uniform}"
LAYER="pts"
GEOM="geom"

command -v ogrinfo >/dev/null || { echo "ogrinfo not found on PATH" >&2; exit 1; }

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BENCH="$ROOT/target/release/examples/index_bench"

echo "building the measurement tool"
cargo build --release --quiet --manifest-path "$ROOT/Cargo.toml" --example index_bench

echo
echo "environment"
echo "  ours: bundled SQLite $("$BENCH" version | cut -d= -f2)"
echo "  $(ogrinfo --version)"
echo "  rows=$ROWS reps=$REPS distribution=$DIST"
echo "  host load at start:$(uptime | sed 's/.*load averages*://')"

FIXTURE="$WORK/fixture.gpkg"
echo
echo "writing an unindexed fixture of $ROWS $DIST rows"
"$BENCH" fixture "$FIXTURE" "$ROWS" "$DIST" >/dev/null

# Milliseconds of wall time for a command, measured by the shell.
wall_ms() {
    local start end
    start=$(python3 -c 'import time; print(time.perf_counter_ns())')
    "$@" >/dev/null 2>&1
    end=$(python3 -c 'import time; print(time.perf_counter_ns())')
    python3 -c "print(f'{($end - $start) / 1e6:.3f}')"
}

median() {
    python3 -c "
import sys
xs = sorted(float(x) for x in sys.argv[1:])
print(f'{xs[len(xs)//2]:.3f}')
" "$@"
}

echo
echo "measuring startup floors"
OURS_FLOOR_SAMPLES=()
GDAL_FLOOR_SAMPLES=()
for _ in $(seq 1 "$REPS"); do
    cp "$FIXTURE" "$WORK/floor.gpkg"
    OURS_FLOOR_SAMPLES+=("$(wall_ms "$BENCH" noop "$WORK/floor.gpkg")")
    GDAL_FLOOR_SAMPLES+=("$(wall_ms ogrinfo "$WORK/floor.gpkg" -sql "SELECT 1")")
done
OURS_FLOOR=$(median "${OURS_FLOOR_SAMPLES[@]}")
GDAL_FLOOR=$(median "${GDAL_FLOOR_SAMPLES[@]}")
echo "  ours:  ${OURS_FLOOR} ms"
echo "  gdal:  ${GDAL_FLOOR} ms"

echo
echo "timing the index build ($REPS repetitions, alternating order)"
OURS_SAMPLES=()
GDAL_SAMPLES=()
for rep in $(seq 1 "$REPS"); do
    cp "$FIXTURE" "$WORK/ours.gpkg"
    cp "$FIXTURE" "$WORK/gdal.gpkg"
    if (( rep % 2 == 1 )); then
        OURS_SAMPLES+=("$(wall_ms "$BENCH" build "$WORK/ours.gpkg")")
        GDAL_SAMPLES+=("$(wall_ms ogrinfo "$WORK/gdal.gpkg" -sql "SELECT CreateSpatialIndex('$LAYER','$GEOM')")")
    else
        GDAL_SAMPLES+=("$(wall_ms ogrinfo "$WORK/gdal.gpkg" -sql "SELECT CreateSpatialIndex('$LAYER','$GEOM')")")
        OURS_SAMPLES+=("$(wall_ms "$BENCH" build "$WORK/ours.gpkg")")
    fi
    echo "  rep $rep: ours ${OURS_SAMPLES[$((${#OURS_SAMPLES[@]}-1))]} ms, gdal ${GDAL_SAMPLES[$((${#GDAL_SAMPLES[@]}-1))]} ms"
done

OURS_RAW=$(median "${OURS_SAMPLES[@]}")
GDAL_RAW=$(median "${GDAL_SAMPLES[@]}")
OURS_NET=$(python3 -c "print(f'{max(0.0, $OURS_RAW - $OURS_FLOOR):.3f}')")
GDAL_NET=$(python3 -c "print(f'{max(0.0, $GDAL_RAW - $GDAL_FLOOR):.3f}')")
RATIO=$(python3 -c "print(f'{($OURS_NET / $GDAL_NET):.2f}' if $GDAL_NET > 0 else 'n/a')")

echo
echo "build time (median of $REPS)"
printf '  %-6s %12s %12s\n' "arm" "raw ms" "less floor"
printf '  %-6s %12s %12s\n' "ours" "$OURS_RAW" "$OURS_NET"
printf '  %-6s %12s %12s\n' "gdal" "$GDAL_RAW" "$GDAL_NET"
echo "  ours / gdal = ${RATIO}x"

echo
echo "what each build produced"
for arm in ours gdal; do
    echo "  $arm:"
    "$BENCH" stats "$WORK/$arm.gpkg" | sed 's/^/    /'
done

echo
echo "query latency over each index, both read by this crate"
for arm in ours gdal; do
    echo "  $arm:"
    "$BENCH" query "$WORK/$arm.gpkg" 25 | sed 's/^/    /'
done

echo
echo "host load at end:$(uptime | sed 's/.*load averages*://')"
