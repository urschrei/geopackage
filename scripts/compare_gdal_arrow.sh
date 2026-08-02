#!/usr/bin/env bash
#
# Compare this crate's Arrow read path against GDAL's, like for like.
#
# Both arms are handed the same file and asked for the same thing: consume every
# Arrow batch of the whole layer, and nothing else. The GDAL arm is a small C
# program against the OGR C API (scripts/gdal_arrow_read.c) rather than anything
# driven from Python, so nothing but GDAL is inside the measured loop.
#
# Method:
#
#   1. Write one fixture: polygon features with thirteen attributes, the shape
#      GDAL's published benchmark uses.
#   2. For each repetition, alternate which arm runs first, so any drift in
#      machine state is shared rather than landing on one arm.
#   3. Time the read as the process's own internal measurement, and subtract
#      each arm's measured startup floor (an open and close) from the externally
#      observed wall time. Both figures are reported.
#   4. Report the median over repetitions, not the mean.
#   5. Cross-check that both arms read the same number of rows and a comparable
#      number of columns. A faster arm that read less is not faster.
#
# Thread count is set explicitly on the GDAL side. Its GeoPackage driver
# defaults to min(4, CPUs) for this path, so leaving it alone would compare four
# of its cores against one of ours. The single-threaded figure is the one
# criterion 3 is about; a four-thread run is also reported, for context and to
# size the prize for our own parallel work.
#
# Usage: scripts/compare_gdal_arrow.sh [rows] [reps]

set -euo pipefail

ROWS="${1:-200000}"
REPS="${2:-5}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

if ! command -v gdal-config >/dev/null 2>&1; then
  echo "gdal-config not found; install GDAL to run this comparison" >&2
  exit 1
fi

echo "building both arms"
cargo build --release --quiet --manifest-path "$ROOT/Cargo.toml" \
  --features arrow --example arrow_bench
OURS="$ROOT/target/release/examples/arrow_bench"

GDAL_BIN="$WORK/gdal_arrow_read"
# shellcheck disable=SC2046
cc -O2 -o "$GDAL_BIN" "$ROOT/scripts/gdal_arrow_read.c" \
  $(gdal-config --cflags) $(gdal-config --libs)

FIXTURE="$WORK/features.gpkg"
echo "writing a fixture of $ROWS polygon rows with 13 attributes"
"$OURS" fixture "$FIXTURE" "$ROWS" >/dev/null

echo "gdal $(gdal-config --version), $(sw_vers -productName 2>/dev/null || uname -s)"
echo

# `value key output` extracts a key=value line printed by either arm.
value() {
  grep "^$1=" | head -1 | cut -d= -f2
}

# Externally observed wall time in milliseconds for a command.
wall_ms() {
  local start end
  start=$(python3 -c 'import time;print(time.monotonic_ns())')
  "$@" >"$WORK/out.txt" 2>"$WORK/err.txt" || {
    echo "arm failed: $*" >&2
    cat "$WORK/err.txt" >&2
    exit 1
  }
  end=$(python3 -c 'import time;print(time.monotonic_ns())')
  echo "scale=3; ($end - $start) / 1000000" | bc
}

median() {
  sort -g | awk '{v[NR]=$1} END {if (NR % 2) print v[(NR+1)/2]; else print (v[NR/2] + v[NR/2+1]) / 2}'
}

# Startup floors, five samples each, median.
for _ in $(seq 5); do wall_ms "$OURS" noop "$FIXTURE"; done >"$WORK/floor_ours.txt"
for _ in $(seq 5); do OGR_GPKG_NUM_THREADS=1 wall_ms "$GDAL_BIN" noop "$FIXTURE"; done \
  >"$WORK/floor_gdal.txt"
FLOOR_OURS=$(median <"$WORK/floor_ours.txt")
FLOOR_GDAL=$(median <"$WORK/floor_gdal.txt")

: >"$WORK/ours_internal.txt"; : >"$WORK/ours_wall.txt"
: >"$WORK/gdal1_internal.txt"; : >"$WORK/gdal1_wall.txt"
: >"$WORK/gdal4_internal.txt"; : >"$WORK/gdal4_wall.txt"
: >"$WORK/ours4_internal.txt"

run_ours() {
  wall_ms "$OURS" read "$FIXTURE" 1 1 >>"$WORK/ours_wall.txt"
  value elapsed_ms <"$WORK/out.txt" >>"$WORK/ours_internal.txt"
  OURS_ROWS=$(value rows <"$WORK/out.txt")
  OURS_COLS=$(value columns <"$WORK/out.txt")
  OURS_BATCHES=$(value batches <"$WORK/out.txt")
}

run_ours_parallel() {
  wall_ms "$OURS" read "$FIXTURE" 1 4 >/dev/null
  value elapsed_ms <"$WORK/out.txt" >>"$WORK/ours4_internal.txt"
  OURS4_ROWS=$(value rows <"$WORK/out.txt")
}

run_gdal() {
  local threads=$1 tag=$2
  OGR_GPKG_NUM_THREADS="$threads" wall_ms "$GDAL_BIN" read "$FIXTURE" \
    >>"$WORK/${tag}_wall.txt"
  value elapsed_ms <"$WORK/out.txt" >>"$WORK/${tag}_internal.txt"
  GDAL_ROWS=$(value rows <"$WORK/out.txt")
  GDAL_COLS=$(value columns <"$WORK/out.txt")
  GDAL_BATCHES=$(value batches <"$WORK/out.txt")
}

echo "running $REPS repetitions, alternating arm order"
for rep in $(seq "$REPS"); do
  if [ $((rep % 2)) -eq 1 ]; then
    run_ours
    run_gdal 1 gdal1
  else
    run_gdal 1 gdal1
    run_ours
  fi
  run_gdal 4 gdal4
  run_ours_parallel
done

OURS_MS=$(median <"$WORK/ours_internal.txt")
GDAL1_MS=$(median <"$WORK/gdal1_internal.txt")
GDAL4_MS=$(median <"$WORK/gdal4_internal.txt")
OURS4_MS=$(median <"$WORK/ours4_internal.txt")
OURS_WALL=$(median <"$WORK/ours_wall.txt")
GDAL1_WALL=$(median <"$WORK/gdal1_wall.txt")

OURS_ADJ=$(echo "scale=3; $OURS_WALL - $FLOOR_OURS" | bc)
GDAL1_ADJ=$(echo "scale=3; $GDAL1_WALL - $FLOOR_GDAL" | bc)

echo
echo "rows read:    ours=$OURS_ROWS gdal=$GDAL_ROWS"
echo "columns:      ours=$OURS_COLS gdal=$GDAL_COLS"
echo "batches:      ours=$OURS_BATCHES gdal=$GDAL_BATCHES"
if [ "$OURS_ROWS" != "$GDAL_ROWS" ]; then
  echo "WARNING: the two arms read different row counts; the comparison is void" >&2
fi
echo
printf 'internal, 1 thread:  ours %8s ms   gdal %8s ms   ratio %s\n' \
  "$OURS_MS" "$GDAL1_MS" "$(echo "scale=2; $OURS_MS / $GDAL1_MS" | bc)"
printf 'wall minus floor:    ours %8s ms   gdal %8s ms   ratio %s\n' \
  "$OURS_ADJ" "$GDAL1_ADJ" "$(echo "scale=2; $OURS_ADJ / $GDAL1_ADJ" | bc)"
printf 'gdal, 4 threads:     %s ms (%sx its own single-threaded figure)\n' \
  "$GDAL4_MS" "$(echo "scale=2; $GDAL1_MS / $GDAL4_MS" | bc)"
printf 'ours, 4 threads:     %s ms (%sx our own single-threaded figure, rows=%s)\n' \
  "$OURS4_MS" "$(echo "scale=2; $OURS_MS / $OURS4_MS" | bc)" "$OURS4_ROWS"
echo
echo "criterion 3 asks for a single-threaded ratio no worse than 1.25"
printf 'ours at 4 threads against gdal single-threaded: %s\n' \
  "$(echo "scale=2; $OURS4_MS / $GDAL1_MS" | bc)"
