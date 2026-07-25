#!/usr/bin/env bash
#
# Compare this crate's Arrow write path against GDAL's, like for like
# (M3 acceptance criterion 3, write side).
#
# Both arms do the same thing: read one source GeoPackage's Arrow stream into
# memory, untimed, then time only the writing of those batches into a fresh
# file. The read sits outside the measurement deliberately. Timing a read plus a
# write produces a figure that says nothing about either, which is exactly the
# mistake the M2 GDAL comparison had to withdraw.
#
# Worth knowing when reading the result: GDAL's GeoPackage driver has no
# specialised WriteArrowBatch. Slide 11 of the reference talk lists only
# GeoParquet and GeoArrow, so this exercises its generic implementation over
# CreateFeature. That is still the honest comparison, because it is what a GDAL
# user writing a GeoPackage actually gets, but it means being ahead here is the
# expectation rather than an achievement, and being behind would be a problem.
#
# Both configurations are measured, because the spatial index is not a detail.
# GDAL's driver creates one by default and this crate's create_layer does not, so
# an unqualified comparison would put a write that builds an index against one
# that does not. The first run of this script did exactly that, and flattered us.
#
# With the index on, this crate creates it empty before the write so the bulk
# path builds it in one pass, which is the arrangement the roadmap calls the
# pyogrio-shaped fast path.
#
# Method otherwise follows scripts/compare_gdal_arrow.sh: alternate which arm
# runs first, take medians, and cross-check that both wrote the same row count.
#
# Usage: scripts/compare_gdal_arrow_write.sh [rows] [reps]

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

GDAL_BIN="$WORK/gdal_arrow_write"
# shellcheck disable=SC2046
cc -O2 -o "$GDAL_BIN" "$ROOT/scripts/gdal_arrow_write.c" \
  $(gdal-config --cflags) $(gdal-config --libs)

SOURCE="$WORK/source.gpkg"
echo "writing a source of $ROWS polygon rows with 13 attributes"
"$OURS" fixture "$SOURCE" "$ROWS" >/dev/null

echo "gdal $(gdal-config --version)"
echo

value() { grep "^$1=" | head -1 | cut -d= -f2; }

median() {
  sort -g | awk '{v[NR]=$1} END {if (NR % 2) print v[(NR+1)/2]; else print (v[NR/2] + v[NR/2+1]) / 2}'
}

: >"$WORK/ours.txt"
: >"$WORK/gdal.txt"

run_ours() {
  rm -f "$WORK/ours_out.gpkg"
  "$OURS" write "$SOURCE" "$WORK/ours_out.gpkg" "$1" >"$WORK/out.txt"
  value elapsed_ms <"$WORK/out.txt" >>"$WORK/ours.txt"
  OURS_ROWS=$(value rows <"$WORK/out.txt")
}

run_gdal() {
  rm -f "$WORK/gdal_out.gpkg"
  "$GDAL_BIN" "$SOURCE" "$WORK/gdal_out.gpkg" "$1" >"$WORK/out.txt"
  value elapsed_ms <"$WORK/out.txt" >>"$WORK/gdal.txt"
  GDAL_ROWS=$(value rows <"$WORK/out.txt")
}

# Cross-check that the flag reached both writers, rather than trusting it.
check_index() {
  local want=$1 file=$2 label=$3 found
  found=$(sqlite3 "$file" \
    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='rtree_features_geom';")
  if { [ "$want" = "yes" ] && [ "$found" = "0" ]; } ||
     { [ "$want" = "no" ] && [ "$found" != "0" ]; }; then
    echo "WARNING: $label index=$want but rtree tables found=$found" >&2
  fi
}

for index in no yes; do
  : >"$WORK/ours.txt"
  : >"$WORK/gdal.txt"
  echo "running $REPS repetitions with index=$index, alternating arm order"
  for rep in $(seq "$REPS"); do
    if [ $((rep % 2)) -eq 1 ]; then
      run_ours "$index"
      run_gdal "$index"
    else
      run_gdal "$index"
      run_ours "$index"
    fi
  done
  check_index "$index" "$WORK/ours_out.gpkg" ours
  check_index "$index" "$WORK/gdal_out.gpkg" gdal

  OURS_MS=$(median <"$WORK/ours.txt")
  GDAL_MS=$(median <"$WORK/gdal.txt")
  if [ "$OURS_ROWS" != "$GDAL_ROWS" ]; then
    echo "WARNING: arms wrote different row counts; the comparison is void" >&2
  fi
  printf 'index=%-3s rows=%s   ours %9s ms   gdal %9s ms   ratio %s\n' \
    "$index" "$OURS_ROWS" "$OURS_MS" "$GDAL_MS" \
    "$(echo "scale=2; $OURS_MS / $GDAL_MS" | bc)"
  echo
done

echo "criterion 3 asks for the write to be at or ahead of GDAL (ratio <= 1.00)"
