#!/usr/bin/env bash
#
# Time the read, write, index-build and query paths over real third-party
# GeoPackages, for the figures published in the README.
#
# The criterion benches in geopackage/benches answer "did this change make the
# code faster", which wants a fixed, generated shape held constant across runs.
# This answers a different question: what a caller should expect from a file
# they actually have. Real data differs from the fixtures in the two ways these
# paths are most sensitive to, vertex count per geometry and how unevenly the
# features are spread, so it is measured rather than extrapolated from the
# point fixtures.
#
# Three datasets, chosen to sit in different places on both axes rather than to
# be large:
#
#   buildings  Microsoft Building Footprints, California. 11.5M four- or
#              five-vertex polygons: many rows, almost no geometry each.
#   rivers     HydroRIVERS v1.0, global. 8.5M linestrings with sixteen
#              attributes: the mixed case, and the one closest to ordinary
#              vector data.
#   admin      GADM 4.1, global administrative areas. 356k multipolygons with
#              54 attributes, averaging ~7 kB of geometry each: few rows, and
#              nearly all of the bytes are coordinates.
#
# Method:
#
#   1. Each dataset is converted once to an unindexed GeoPackage, so the
#      index-build arm starts from the same state for every dataset and the
#      read arms are not reading index pages.
#   2. Reads are repeated and the median reported, not the mean, so one
#      scheduling hiccup on a shared machine does not move the answer.
#   3. The index build gets a fresh copy per repetition: building over an
#      existing index is a different operation, and a copied file is not warm
#      in the same way a just-built one is.
#   4. The bounding-box query runs against the index this crate just built,
#      and again with no index at all, so the figure has its own baseline
#      rather than being quoted alone.
#   5. The write arm reads the source into memory untimed, then times the
#      write. Timing a read and a write together produces a figure that says
#      nothing about either; that mistake was made once in this repo already
#      (see roadmap/benchmarks/2026-07-24-gdal-like-for-like.md). It runs twice,
#      with and without the index being built during the write, since those are
#      separate figures and the README quotes both.
#   6. Peak resident set size is reported for the columnar read, the index build
#      and the write: three arms whose working set depends on the data rather
#      than being a constant. These are extra invocations, so the timings do not
#      pay for the measurement, and they are repeated and taken as a median for
#      the same reason as the timings. The threaded columnar read's peak varies
#      by nearly a factor of two run to run, depending on how many batches its
#      reader threads hold at once, so a single run of it means little.
#
# What this does not control: the OS page cache. Every arm here is warm, which
# is the right choice for a repeated measurement but means these are not
# cold-start figures. The host is an ordinary desktop, so `uptime` is reported
# at both ends and the medians are what should be quoted.
#
# Usage:
#   scripts/bench_datasets.sh fetch [dir]         download and convert (~5 GB)
#   scripts/bench_datasets.sh run   [dir] [reps]  measure

set -euo pipefail

MODE="${1:-run}"
DIR="${2:-${GEOPACKAGE_BENCH_DIR:-$PWD/benchdata}}"
REPS="${3:-3}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BENCH="$ROOT/target/release/examples/dataset_bench"

# name | layer | gpkg | query box (minx miny maxx maxy)
# The boxes are real places, sized to return a fraction of a per cent of the
# layer: San Francisco, the Amazon basin, and western Europe respectively.
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

# The elapsed_ms line from one dataset_bench invocation.
timed() {
    "$BENCH" "$@" | sed -n 's/^elapsed_ms=//p'
}

# A named key from one dataset_bench invocation.
field() {
    local key="$1"; shift
    "$BENCH" "$@" | sed -n "s/^$key=//p"
}

# Peak resident set size of one dataset_bench invocation, in MB.
#
# The process is wrapped rather than instrumented: what a caller cares about is
# what the whole process holds, and this needs no memory-profiling dependency in
# the example. It is a separate invocation from the timed ones, so the timings
# are not paying for the measurement.
#
# Reported for the arms whose working set is a question rather than a constant:
# the columnar read holds whole batches (the reason the `admin` read figure
# moves with host memory), the index build holds an entry per row, and the write
# holds the batches it is writing. The scalar read holds one row at a time by
# construction, so it is not measured here.
#
# This needs repeating as much as the timings do, and for the same reason. The
# threaded columnar read was measured over five runs of one dataset at 642, 900,
# 1046, 1066, 1089 and 1190 MB: peak depends on how many batches the reader
# threads happen to hold at once, so a single run can be off by a factor of two.
# The caller repeats it and takes a median.
#
# macOS `time -l` reports bytes, GNU `time -v` kilobytes; both are normalised.
peak_rss_mb() {
    local err rss
    err="$(mktemp)"
    /usr/bin/time -l "$BENCH" "$@" >/dev/null 2>"$err" || true
    rss="$(awk '/maximum resident set size/ {print $1; exit}' "$err")"
    if [[ -z "$rss" ]]; then
        /usr/bin/time -v "$BENCH" "$@" >/dev/null 2>"$err" || true
        rss="$(awk '/Maximum resident set size/ {print $6; exit}' "$err")"
        [[ -n "$rss" ]] && rss=$((rss * 1024))
    fi
    rm -f "$err"
    if [[ -n "$rss" ]]; then
        awk -v b="$rss" 'BEGIN { printf "%.0f", b / 1048576 }'
    else
        echo "n/a"
    fi
}

run() {
    [[ -x "$BENCH" ]] || {
        echo "building the measurement tool"
        cargo build --release --quiet --manifest-path "$ROOT/Cargo.toml" \
            --features arrow --example dataset_bench
    }

    WORK="$(mktemp -d)"
    trap 'rm -rf "$WORK"' EXIT

    echo "environment"
    echo "  $(uname -sr), $(sysctl -n machdep.cpu.brand_string 2>/dev/null || echo 'unknown cpu')"
    echo "  reps=$REPS"
    echo "  host load at start:$(uptime | sed 's/.*load averages*://')"

    for entry in "${DATASETS[@]}"; do
        IFS='|' read -r name layer file box <<<"$entry"
        local_path="$DIR/$file"
        [[ -f "$local_path" ]] || { echo "missing $local_path; run the fetch step" >&2; exit 1; }

        echo
        echo "=== $name ($file, layer $layer)"
        "$BENCH" info "$local_path" "$layer" | sed 's/^/  /'

        # Reads, over the unindexed original.
        samples=()
        for _ in $(seq 1 "$REPS"); do samples+=("$(timed scan "$local_path" "$layer" geom 1)"); done
        echo "  scan_ms=$(median "${samples[@]}")"

        samples=()
        for _ in $(seq 1 "$REPS"); do samples+=("$(timed arrow "$local_path" "$layer" 1 0)"); done
        echo "  arrow_ms=$(median "${samples[@]}")"

        samples=()
        for _ in $(seq 1 "$REPS"); do samples+=("$(timed arrow "$local_path" "$layer" 1 1)"); done
        echo "  arrow_1thread_ms=$(median "${samples[@]}")"

        # The box query with no index, the baseline the index is measured
        # against. Read from the unindexed original.
        # shellcheck disable=SC2086
        echo "  bbox_scan_ms=$(timed bbox "$local_path" "$layer" $box 1)"

        # Index build: a fresh copy per repetition.
        samples=()
        for _ in $(seq 1 "$REPS"); do
            cp "$local_path" "$WORK/indexed.gpkg"
            samples+=("$(timed index "$WORK/indexed.gpkg" "$layer")")
        done
        echo "  index_build_ms=$(median "${samples[@]}")"

        # The same box against the index just built.
        samples=()
        # shellcheck disable=SC2086
        for _ in $(seq 1 "$REPS"); do samples+=("$(timed bbox "$WORK/indexed.gpkg" "$layer" $box 5)"); done
        # shellcheck disable=SC2086
        echo "  bbox_index_ms=$(median "${samples[@]}")"
        # shellcheck disable=SC2086
        echo "  bbox_hits=$(field hits bbox "$WORK/indexed.gpkg" "$layer" $box 1)"
        echo "  indexed_bytes=$(stat -f%z "$WORK/indexed.gpkg" 2>/dev/null || stat -c%s "$WORK/indexed.gpkg")"
        rm -f "$WORK/indexed.gpkg"

        # Write: source read into memory untimed, the write timed. Both arms,
        # because they answer different questions and the README quotes both:
        # what the write costs, and what building the index during it adds. The
        # indexed arm creates the index empty before the write, which is what
        # lets the bulk path fill it in one pass instead of through the
        # triggers.
        rm -f "$WORK/written.gpkg"
        echo "  write_ms=$(timed write "$local_path" "$layer" "$WORK/written.gpkg" no)"
        rm -f "$WORK/written.gpkg"
        echo "  write_with_index_ms=$(timed write "$local_path" "$layer" "$WORK/written.gpkg" yes)"
        rm -f "$WORK/written.gpkg"

        # Peak memory, on the arms whose working set is a question. Last, so a
        # failure here does not cost the timings, and repeated like them,
        # because peak RSS varies as much run to run as elapsed time does.
        samples=()
        for _ in $(seq 1 "$REPS"); do samples+=("$(peak_rss_mb arrow "$local_path" "$layer" 1 0)"); done
        echo "  arrow_peak_mb=$(median "${samples[@]}")"

        samples=()
        for _ in $(seq 1 "$REPS"); do
            cp "$local_path" "$WORK/rss.gpkg"
            samples+=("$(peak_rss_mb index "$WORK/rss.gpkg" "$layer")")
            rm -f "$WORK/rss.gpkg"
        done
        echo "  index_peak_mb=$(median "${samples[@]}")"

        samples=()
        for _ in $(seq 1 "$REPS"); do
            samples+=("$(peak_rss_mb write "$local_path" "$layer" "$WORK/written.gpkg" no)")
            rm -f "$WORK/written.gpkg"
        done
        echo "  write_peak_mb=$(median "${samples[@]}")"
    done

    echo
    echo "host load at end:$(uptime | sed 's/.*load averages*://')"
}

case "$MODE" in
    fetch) fetch ;;
    run) run ;;
    *) echo "usage: scripts/bench_datasets.sh <fetch|run> [dir] [reps]" >&2; exit 1 ;;
esac
