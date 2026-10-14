#!/usr/bin/env bash
#
# Time the read, write, index-build and query paths of this crate and of GDAL
# over three published GeoPackages.
#
#   buildings  Microsoft Building Footprints, California. 11.5M polygons of
#              four or five vertices.
#   rivers     HydroRIVERS v1.0, global. 8.5M linestrings with 16 attributes.
#   admin      GADM 4.1, global administrative areas. 356k multipolygons with
#              54 attributes and about 7 kB of geometry each.
#
# The fetch step converts each source to an unindexed GeoPackage with ogr2ogr.
#
# The two sides are geopackage/examples/dataset_bench.rs and
# scripts/gdal_dataset_bench.c, which have the same subcommands. The script
# compiles the C program against the GDAL that `$GDAL_CONFIG` (default
# `gdal-config`) names. Each tool times the operation inside its process.
#
# Method, per dataset and operation:
#
#   1. `reps` runs of each side, alternating which side runs first, and the
#      median of each side.
#   2. The index build gets a fresh copy of the dataset for each run. The
#      indexed query then runs against the index that the same side built.
#   3. The write reads the source into memory as Arrow batches (not timed),
#      then times the write of those batches into a new file.
#   4. Writes on both sides use synchronous = OFF.
#   5. Peak resident memory for the columnar read, the index build and the
#      write, as the median of `reps` extra runs. Needs /usr/bin/time.
#   6. Each side's row and hit counts, so that a side that read less is
#      visible.
#
# The page cache is warm for every run.
#
# Usage:
#   scripts/bench_datasets.sh fetch [dir]         download and convert (~5 GB)
#   scripts/bench_datasets.sh run   [dir] [reps]  measure

set -euo pipefail

MODE="${1:-run}"
DIR="${2:-${GEOPACKAGE_BENCH_DIR:-$PWD/benchdata}}"
REPS="${3:-3}"
GDAL_CONFIG="${GDAL_CONFIG:-gdal-config}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OURS="$ROOT/target/release/examples/dataset_bench"

# name | layer | gpkg | query box (minx miny maxx maxy)
# The boxes are San Francisco, part of the Amazon basin, and western Europe.
DATASETS=(
    "buildings|buildings|ca_buildings.gpkg|-122.52 37.70 -122.35 37.83"
    "rivers|rivers|hydrorivers.gpkg|-70 -10 -60 0"
    "admin|gadm|gadm_noidx.gpkg|0 45 10 55"
)

fetch() {
    mkdir -p "$DIR"
    command -v ogr2ogr >/dev/null || { echo "ogr2ogr not found on PATH" >&2; exit 1; }

    # Microsoft Building Footprints, California. The legacy v2 snapshot has a
    # stable URL; the current release is split differently and is far larger.
    if [[ ! -f "$DIR/ca_buildings.gpkg" ]]; then
        echo "fetching Microsoft Building Footprints (California)"
        curl -fL --retry 3 -o "$DIR/California.geojson.zip" \
            "https://minedbuildings.z5.web.core.windows.net/legacy/usbuildings-v2/California.geojson.zip"
        unzip -o -q "$DIR/California.geojson.zip" -d "$DIR/ca"
        ogr2ogr -f GPKG -lco SPATIAL_INDEX=NO -nln buildings \
            "$DIR/ca_buildings.gpkg" "$DIR/ca/California.geojson"
    fi

    # HydroRIVERS v1.0, global, as a shapefile.
    if [[ ! -f "$DIR/hydrorivers.gpkg" ]]; then
        echo "fetching HydroRIVERS v1.0"
        curl -fL --retry 3 -o "$DIR/HydroRIVERS_v10_shp.zip" \
            "https://data.hydrosheds.org/file/HydroRIVERS/HydroRIVERS_v10_shp.zip"
        unzip -o -q "$DIR/HydroRIVERS_v10_shp.zip" -d "$DIR/hr"
        ogr2ogr -f GPKG -lco SPATIAL_INDEX=NO -nln rivers \
            "$DIR/hydrorivers.gpkg" "$DIR/hr/HydroRIVERS_v10_shp/HydroRIVERS_v10.shp"
    fi

    # GADM 4.1. Distributed as a GeoPackage already, but an indexed one written
    # by a 2022 GDAL; re-converted so all three start unindexed and were
    # written by the same version.
    if [[ ! -f "$DIR/gadm_noidx.gpkg" ]]; then
        echo "fetching GADM 4.1"
        curl -fL --retry 3 -o "$DIR/gadm_410-gpkg.zip" \
            "https://geodata.ucdavis.edu/gadm/gadm4.1/gadm_410-gpkg.zip"
        unzip -o -q "$DIR/gadm_410-gpkg.zip" -d "$DIR/gadm"
        ogr2ogr -f GPKG -lco SPATIAL_INDEX=NO -nln gadm \
            "$DIR/gadm_noidx.gpkg" "$DIR/gadm/gadm_410.gpkg"
    fi

    echo
    echo "prepared in $DIR:"
    ls -la "$DIR"/*.gpkg
}

median() {
    python3 -c "
import sys
xs = sorted(float(x) for x in sys.argv[1:])
print(f'{xs[len(xs)//2]:.1f}')
" "$@"
}

# Peak resident set size from a /usr/bin/time report, in MiB. macOS `time -l`
# reports bytes, GNU `time -v` KiB.
rss_mib() {
    awk '/maximum resident set size/ { printf "%.0f", $1 / 1048576; exit }
         /Maximum resident set size/ { printf "%.0f", $6 / 1024; exit }' "$1"
}

# Run one side's tool. Print its elapsed_ms, or its peak RSS in MiB when
# MEASURE is `rss`. Keep the full output in $WORK/last-<side>.txt.
run_tool() {
    local side="$1"; shift
    local bin="$OURS" out="$WORK/last-$side.txt"
    [[ $side == gdal ]] && bin="$GDAL"
    if [[ ${MEASURE:-time} == rss ]]; then
        local flag=-v
        [[ $(uname -s) == Darwin ]] && flag=-l
        /usr/bin/time "$flag" "$bin" "$@" >"$out" 2>"$WORK/rss.txt"
        rss_mib "$WORK/rss.txt"
    else
        "$bin" "$@" >"$out"
        sed -n 's/^elapsed_ms=//p' "$out"
    fi
}

# The operations. Each takes the side first.

op_scan() {
    if [[ $1 == ours ]]; then run_tool ours scan "$src" "$layer" geo 1
    else run_tool gdal scan "$src" "$layer"; fi
}

# The second argument is the thread count: 0 is each side's default.
op_arrow() {
    if [[ $1 == ours ]]; then run_tool ours arrow "$src" "$layer" 1 "$2"
    elif [[ $2 == 0 ]]; then run_tool gdal arrow "$src" "$layer"
    else OGR_GPKG_NUM_THREADS="$2" run_tool gdal arrow "$src" "$layer"; fi
}

op_index() {
    cp "$src" "$WORK/indexed-$1.gpkg"
    run_tool "$1" index "$WORK/indexed-$1.gpkg" "$layer"
}

# The second argument is the file to query.
op_bbox() {
    local file="$2"
    [[ $file == indexed ]] && file="$WORK/indexed-$1.gpkg"
    # shellcheck disable=SC2086
    if [[ $1 == ours ]]; then run_tool ours bbox "$file" "$layer" $box 1
    else run_tool gdal bbox "$file" "$layer" $box; fi
}

# The second argument is yes or no: build the index during the write.
op_write() {
    rm -f "$WORK/written-$1.gpkg"
    run_tool "$1" write "$src" "$layer" "$WORK/written-$1.gpkg" "$2"
    rm -f "$WORK/written-$1.gpkg"
}

# Run one operation `reps` times on each side, alternating which side runs
# first. Print the median of each side, then each side's counts from its last
# run.
measure() {
    local label="$1"; shift
    local ours=() gdal=() rep
    for rep in $(seq 1 "$REPS"); do
        if (( rep % 2 )); then
            ours+=("$("$@" ours)"); gdal+=("$("$@" gdal)")
        else
            gdal+=("$("$@" gdal)"); ours+=("$("$@" ours)")
        fi
    done
    printf '  %-26s %12s %12s\n' "$label" "$(median "${ours[@]}")" "$(median "${gdal[@]}")"
    local counts
    for side in ours gdal; do
        counts="$(grep -E '^(rows|hits|geometries)=' "$WORK/last-$side.txt" | tr '\n' ' ' || true)"
        [[ -n $counts ]] && printf '    %s: %s\n' "$side" "$counts"
    done
    return 0
}

# `measure` passes the side last, so each operation is wrapped to put the side
# first.
scan() { op_scan "$1"; }
arrow_default() { op_arrow "$1" 0; }
arrow_single() { op_arrow "$1" 1; }
index() { op_index "$1"; }
bbox_unindexed() { op_bbox "$1" "$src"; }
bbox_indexed() { op_bbox "$1" indexed; }
write_plain() { op_write "$1" no; }
write_indexed() { op_write "$1" yes; }

run() {
    command -v "$GDAL_CONFIG" >/dev/null || { echo "$GDAL_CONFIG not found" >&2; exit 1; }
    [[ -x /usr/bin/time ]] || { echo "/usr/bin/time not found" >&2; exit 1; }

    WORK="$(mktemp -d)"
    trap 'rm -rf "$WORK"' EXIT

    echo "building both tools"
    cargo build --release --quiet --manifest-path "$ROOT/Cargo.toml" \
        -p geopackage --features arrow,bundled --example dataset_bench
    local prefix
    prefix="$("$GDAL_CONFIG" --prefix)"
    GDAL="$WORK/gdal_dataset_bench"
    # shellcheck disable=SC2046
    cc -O2 -o "$GDAL" "$ROOT/scripts/gdal_dataset_bench.c" \
        $("$GDAL_CONFIG" --cflags) $("$GDAL_CONFIG" --libs) -Wl,-rpath,"$prefix/lib"
    # As activating a conda environment would set them.
    if [[ $prefix != /usr && -d $prefix/share/proj ]]; then
        export PROJ_DATA="$prefix/share/proj" GDAL_DATA="$prefix/share/gdal"
    fi

    echo
    echo "environment"
    echo "  $(uname -sr), $(nproc 2>/dev/null || sysctl -n hw.ncpu) CPUs"
    echo "  GDAL $("$GDAL_CONFIG" --version) at $prefix"
    echo "  reps=$REPS"
    echo "  host load at start:$(uptime | sed 's/.*load averages*://')"

    for entry in "${DATASETS[@]}"; do
        IFS='|' read -r name layer file box <<<"$entry"
        src="$DIR/$file"
        [[ -f "$src" ]] || { echo "missing $src; run the fetch step" >&2; exit 1; }

        echo
        echo "=== $name ($file, layer $layer)"
        "$OURS" info "$src" "$layer" | sed 's/^/  /'
        printf '  %-26s %12s %12s\n' "milliseconds" "ours" "gdal"

        measure scan scan
        measure arrow arrow_default
        measure arrow_1_thread arrow_single
        measure bbox_unindexed bbox_unindexed
        measure index_build index
        measure bbox_indexed bbox_indexed
        for side in ours gdal; do
            echo "    $side indexed bytes: $(stat -c%s "$WORK/indexed-$side.gpkg" 2>/dev/null || stat -f%z "$WORK/indexed-$side.gpkg")"
        done
        rm -f "$WORK"/indexed-*.gpkg
        measure write write_plain
        measure write_with_index write_indexed

        printf '  %-26s %12s %12s\n' "peak MiB" "ours" "gdal"
        MEASURE=rss measure arrow_peak arrow_default
        MEASURE=rss measure index_peak index
        rm -f "$WORK"/indexed-*.gpkg
        MEASURE=rss measure write_peak write_plain
    done

    echo
    echo "host load at end:$(uptime | sed 's/.*load averages*://')"
}

case "$MODE" in
    fetch) fetch ;;
    run) run ;;
    *) echo "usage: scripts/bench_datasets.sh <fetch|run> [dir] [reps]" >&2; exit 1 ;;
esac
