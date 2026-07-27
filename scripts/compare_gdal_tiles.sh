#!/usr/bin/env bash
#
# Time reading every tile of the same pyramid, this crate against GDAL.
#
# The two arms do different work, and the difference is a capability one rather
# than an optimisation. GDAL's raster driver returns pixels: it decodes each
# PNG. This crate returns the stored tile bytes and has no decoder at all,
# which is the M4 scope decision recorded in roadmap/06-m4-tiles.md. So this is
# not a like-for-like comparison in the sense scripts/compare_gdal_index.sh is,
# and no ratio derived from it says one implementation is faster at the same
# job. What it does say is what each costs to get tiles out of a GeoPackage,
# which is the number a caller serving or copying tiles needs, and it is
# reported as two operations rather than one.
#
# Method:
#
#   1. Build one pyramid fixture with this crate (a full web mercator ladder of
#      4 KiB PNG tiles), so both arms read identical bytes.
#   2. Time each arm as externally observed wall time, over a fresh copy of the
#      file per repetition so neither reads a warmed page cache, alternating
#      which arm runs first.
#   3. Subtract each arm's own startup floor: `gdalinfo` on the file with no
#      band read for GDAL, an open-and-close for ours.
#   4. Report the median over repetitions.
#
# The GDAL arm is `gdalinfo -checksum`, which reads every pixel of every band,
# and is the closest thing its CLI offers to "read the whole raster once".
#
# Usage: scripts/compare_gdal_tiles.sh [max_zoom] [reps]

set -euo pipefail

MAX_ZOOM="${1:-6}"
REPS="${2:-5}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

for tool in gdalinfo; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: $tool not found on PATH (GDAL is required for the comparison arm)" >&2
    exit 1
  fi
done

echo "building the tile_bench example (release)"
cargo build --release -p geopackage --example tile_bench >/dev/null 2>&1
BENCH="$REPO_ROOT/target/release/examples/tile_bench"

FIXTURE="$WORK/fixture.gpkg"
echo "writing fixture: zoom 0..$MAX_ZOOM"
"$BENCH" fixture "$FIXTURE" "$MAX_ZOOM"
TILES="$("$BENCH" scan "$FIXTURE" | sed -n 's/^tiles=//p')"
echo "  tiles=$TILES  size=$(wc -c <"$FIXTURE") bytes"

# Median of a list of numbers on stdin.
median() {
  sort -n | awk '{ v[NR] = $1 } END {
    if (NR == 0) { print "0"; exit }
    if (NR % 2) { print v[(NR + 1) / 2] } else { print (v[NR / 2] + v[NR / 2 + 1]) / 2 }
  }'
}

# Wall milliseconds for a command, measured outside the process.
time_ms() {
  local start end
  start=$(python3 -c 'import time; print(time.perf_counter_ns())')
  "$@" >/dev/null 2>&1
  end=$(python3 -c 'import time; print(time.perf_counter_ns())')
  python3 -c "print(($end - $start) / 1e6)"
}

ours_scan=()
ours_random=()
gdal_read=()
ours_floor=()
gdal_floor=()

for rep in $(seq 1 "$REPS"); do
  copy="$WORK/rep-$rep.gpkg"
  cp "$FIXTURE" "$copy"

  if (( rep % 2 == 0 )); then
    gdal_read+=("$(time_ms gdalinfo -checksum "$copy")")
    ours_scan+=("$(time_ms "$BENCH" scan "$copy")")
  else
    ours_scan+=("$(time_ms "$BENCH" scan "$copy")")
    gdal_read+=("$(time_ms gdalinfo -checksum "$copy")")
  fi
  ours_random+=("$(time_ms "$BENCH" random "$copy")")
  ours_floor+=("$(time_ms "$BENCH" noop "$copy")")
  gdal_floor+=("$(time_ms gdalinfo "$copy")")
  rm -f "$copy"
done

report() {
  local label="$1"
  shift
  local values=("$@")
  printf '%s\n' "${values[@]}" | median
}

ours_scan_ms="$(report scan "${ours_scan[@]}")"
ours_random_ms="$(report random "${ours_random[@]}")"
gdal_read_ms="$(report gdal "${gdal_read[@]}")"
ours_floor_ms="$(report ours_floor "${ours_floor[@]}")"
gdal_floor_ms="$(report gdal_floor "${gdal_floor[@]}")"

python3 - "$TILES" "$ours_scan_ms" "$ours_random_ms" "$gdal_read_ms" "$ours_floor_ms" "$gdal_floor_ms" <<'PY'
import sys

tiles = int(sys.argv[1])
scan, random_read, gdal, ours_floor, gdal_floor = (float(v) for v in sys.argv[2:7])


def rate(ms, floor):
    net = max(ms - floor, 1e-6)
    return tiles / (net / 1000.0), net


scan_rate, scan_net = rate(scan, ours_floor)
random_rate, random_net = rate(random_read, ours_floor)
gdal_rate, gdal_net = rate(gdal, gdal_floor)

print()
print(f"tiles                    {tiles}")
print(f"startup floor (ours)     {ours_floor:8.1f} ms")
print(f"startup floor (gdal)     {gdal_floor:8.1f} ms")
print()
print("retrieving stored tile bytes (this crate)")
print(f"  streaming scan         {scan_net:8.1f} ms   {scan_rate:10.0f} tiles/sec")
print(f"  random by address      {random_net:8.1f} ms   {random_rate:10.0f} tiles/sec")
print()
print("decoding tiles to pixels (GDAL, gdalinfo -checksum)")
print(f"  full read              {gdal_net:8.1f} ms   {gdal_rate:10.0f} tiles/sec")
print()
print("These are different operations: this crate has no decoder, so it cannot")
print("produce what GDAL's figure produces. Quote them as two numbers.")
PY
