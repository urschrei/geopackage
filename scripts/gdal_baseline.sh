#!/usr/bin/env bash
#
# GDAL write-throughput baseline for the M2 benchmark write-up.
#
# There is no clean way to isolate GDAL's GPKG *write* cost from the CLI, so this
# measures the honest, reproducible thing: the wall time of `ogr2ogr` loading N
# features into a fresh GeoPackage, with and without a spatial index. The source
# is a GeoPackage built once (untimed) from a generated CSV, so the timed step is
# GPKG-read + GPKG-write; that read overhead is why the figure is a *conservative*
# (slower) baseline for a pure write. GDAL's own SPATIAL_INDEX=YES path uses the
# same scratch-shadow-table trick this crate reimplements (D8), so the indexed
# numbers are the fair "parity" comparison.
#
# Requires: ogr2ogr, python3 (hi-res timing; macOS `date` lacks %N).
#
# Usage: scripts/gdal_baseline.sh [N]   (default N = 1000000)

set -euo pipefail

N="${1:-1000000}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

now() { python3 -c 'import time; print(time.perf_counter())'; }
elapsed() { python3 -c "print(f'{($2)-($1):.3f}')" "$1" "$2"; }

gen_csv() {
  # $1 = geom kind, $2 = output csv
  local kind="$1" out="$2"
  awk -v n="$N" -v kind="$kind" 'BEGIN {
    print "id,WKT"
    for (i = 0; i < n; i++) {
      x = (i * 0.61803398875) % 360 - 180
      y = (i * 0.31415926536) % 180 - 90
      if (kind == "point") {
        wkt = sprintf("POINT (%.6f %.6f)", x, y)
      } else if (kind == "linestring") {
        wkt = sprintf("LINESTRING (%.6f %.6f, %.6f %.6f, %.6f %.6f)", \
                      x, y, x+0.001, y+0.002, x+0.003, y-0.001)
      } else {
        wkt = sprintf("POLYGON ((%.6f %.6f, %.6f %.6f, %.6f %.6f, %.6f %.6f, %.6f %.6f))", \
                      x, y, x+0.01, y, x+0.01, y+0.01, x, y+0.01, x, y)
      }
      printf "%d,\"%s\"\n", i, wkt
    }
  }' >"$out"
}

echo "GDAL baseline: $(ogr2ogr --version)"
echo "rows=$N"
printf '%-12s %-10s %-10s\n' geom index seconds

for kind in point linestring polygon; do
  csv="$WORK/${kind}.csv"
  src="$WORK/${kind}_src.gpkg"
  gen_csv "$kind" "$csv"
  # Build the canonical source GPKG once (untimed).
  ogr2ogr -f GPKG "$src" "$csv" \
    -oo GEOM_POSSIBLE_NAMES=WKT -oo KEEP_GEOM_COLUMNS=NO \
    -a_srs EPSG:4326 -nln feats >/dev/null 2>&1

  for idx in NO YES; do
    dest="$WORK/${kind}_${idx}.gpkg"
    rm -f "$dest"
    t0="$(now)"
    ogr2ogr -f GPKG "$dest" "$src" -lco "SPATIAL_INDEX=${idx}" >/dev/null 2>&1
    t1="$(now)"
    printf '%-12s %-10s %-10s\n' "$kind" "$idx" "$(elapsed "$t0" "$t1")"
  done
done
