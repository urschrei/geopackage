#!/usr/bin/env bash
#
# Time the read, write, index-build, query and copy paths of this crate and
# of GDAL over subsets of three published GeoPackages.
#
#   buildings  Microsoft Building Footprints, California. 11.5M polygons of
#              four or five vertices.
#   rivers     HydroRIVERS v1.0, global. 8.5M linestrings with 16 attributes.
#   admin      GADM 4.1, global administrative areas. 356k multipolygons with
#              54 attributes.
#
# `fetch` downloads them and converts each to an unindexed GeoPackage with
# ogr2ogr (about 7 GB). `subset` copies the first rows of each, about 250 MB
# per dataset, into another directory. `run` measures over that directory.
#
# This crate's side is geopackage/examples/dataset_bench.rs and `gpkg copy`.
# GDAL's is scripts/gdal_dataset_bench.c, which has the subcommands of
# dataset_bench, and `ogr2ogr`. The script compiles the C program against the
# GDAL that `$GDAL_CONFIG` (default `gdal-config`) names.
#
# Method, per dataset and operation:
#
#   1. `reps` runs of each side, alternating which side runs first. The
#      script reports the median time and the median peak resident memory of
#      each side, both from the same runs. dataset_bench and
#      gdal_dataset_bench time the operation inside the process; the copy is
#      timed from outside.
#   2. The index build gets a fresh copy of the dataset for each run. The
#      indexed query then runs against the index that the same side built.
#   3. The write reads the source into memory as Arrow batches (not timed),
#      then times the write of those batches into a new file.
#   4. Writes on both sides use synchronous = OFF. Neither side syncs the
#      copy.
#   5. Each side's row and hit counts, so that a side that read less is
#      visible.
#
# GDAL's results are kept in `$GDAL_CACHE` (default `<dir>/gdal-cache`), in
# one file per GDAL version, under a key of the dataset's name, row count,
# query box and file size. A dataset whose results are in the file is
# measured on this crate's side only. Delete the file to measure GDAL again,
# and after moving to another machine type.
#
# The page cache is warm for every run. Needs /usr/bin/time.
#
# Usage:
#   scripts/bench_datasets.sh fetch  [dir]          download and convert
#   scripts/bench_datasets.sh subset [dir] <out>    make the subsets
#   scripts/bench_datasets.sh run    [dir] [reps]   measure

set -euo pipefail

MODE="${1:-run}"
DIR="${2:-${GEOPACKAGE_BENCH_DIR:-$PWD/benchdata}}"
REPS="${3:-3}"
GDAL_CONFIG="${GDAL_CONFIG:-gdal-config}"
GDAL_CACHE="${GDAL_CACHE:-$DIR/gdal-cache}"

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OURS="$ROOT/target/release/examples/dataset_bench"
GPKG="$ROOT/target/release/gpkg"

# name | layer | gpkg | rows in the subset | query box (minx miny maxx maxy)
# The boxes are San Francisco, part of East Africa, and western Europe, each
# inside its subset.
DATASETS=(
    "buildings|buildings|ca_buildings.gpkg|1154291|-122.52 37.70 -122.35 37.83"
    "rivers|rivers|hydrorivers.gpkg|847788|30 0 40 10"
    "admin|gadm|gadm_noidx.gpkg|15000|0 45 10 55"
)
OPS=(scan arrow arrow_1_thread bbox_unindexed index_build bbox_indexed write
    write_with_index copy)

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

# Copies the first rows of each dataset, keeping the fids, into `$1`.
subset() {
    local out="${1:?usage: bench_datasets.sh subset [dir] <out>}"
    mkdir -p "$out"
    command -v ogr2ogr >/dev/null || { echo "ogr2ogr not found on PATH" >&2; exit 1; }
    local entry name layer file rows box
    for entry in "${DATASETS[@]}"; do
        IFS='|' read -r name layer file rows box <<<"$entry"
        [[ -s "$out/$file" ]] && continue
        echo "copying the first $rows rows of $name"
        ogr2ogr -f GPKG -lco SPATIAL_INDEX=NO -preserve_fid -limit "$rows" \
            -nln "$layer" "$out/$file" "$DIR/$file" "$layer"
    done
}

median() {
    python3 -c "
import sys
xs = sorted(float(x) for x in sys.argv[1:])
print(f'{xs[len(xs)//2]:.1f}')
" "$@"
}

# The median of field `$1` of the space-separated samples that follow.
column_median() {
    local field="$1"; shift
    local values
    read -r -a values <<<"$(printf '%s\n' "$@" | cut -d' ' -f"$field" | tr '\n' ' ')"
    median "${values[@]}"
}

# Peak resident set size from a /usr/bin/time report, in MiB. macOS `time -l`
# reports bytes, GNU `time -v` KiB.
rss_mib() {
    awk '/maximum resident set size/ { printf "%.0f", $1 / 1048576; exit }
         /Maximum resident set size/ { printf "%.0f", $6 / 1024; exit }' "$1"
}

# Seconds since the epoch, to the microsecond. EPOCHREALTIME needs bash 5.
now() {
    if [[ -n ${EPOCHREALTIME:-} ]]; then echo "$EPOCHREALTIME"
    else python3 -c 'import time; print(time.time())'; fi
}

# Runs one command under /usr/bin/time and prints `<ms> <MiB>`: the
# `elapsed_ms` the command reports, or else the wall time, and the peak
# resident memory. Keeps the command's output in $WORK/last-<side>.txt.
timed() {
    local side="$1"; shift
    local out="$WORK/last-$side.txt" flag=-v start end ms
    [[ $(uname -s) == Darwin ]] && flag=-l
    start="$(now)"
    /usr/bin/time "$flag" "$@" >"$out" 2>"$WORK/time.txt" \
        || { echo "failed: $*" >&2; cat "$WORK/time.txt" >&2; exit 1; }
    end="$(now)"
    ms="$(sed -n 's/^elapsed_ms=//p' "$out")"
    if [[ -z $ms ]]; then
        ms="$(python3 -c "print(f'{($end - $start) * 1000:.1f}')")"
    fi
    echo "$ms $(rss_mib "$WORK/time.txt")"
}

# Runs dataset_bench or gdal_dataset_bench.
tool() {
    local side="$1"; shift
    if [[ $side == ours ]]; then timed ours "$OURS" "$@"; else timed gdal "$GDAL" "$@"; fi
}

# The operations. Each takes the side as its first argument.

op_scan() {
    if [[ $1 == ours ]]; then tool ours scan "$src" "$layer" geo 1
    else tool gdal scan "$src" "$layer"; fi
}

op_arrow() {
    if [[ $1 == ours ]]; then tool ours arrow "$src" "$layer" 1 0
    else tool gdal arrow "$src" "$layer"; fi
}

op_arrow_1_thread() {
    if [[ $1 == ours ]]; then tool ours arrow "$src" "$layer" 1 1
    else OGR_GPKG_NUM_THREADS=1 tool gdal arrow "$src" "$layer"; fi
}

op_index_build() {
    cp "$src" "$WORK/indexed-$1.gpkg"
    tool "$1" index "$WORK/indexed-$1.gpkg" "$layer"
}

# The query box against `$2`.
bbox() {
    # shellcheck disable=SC2086
    if [[ $1 == ours ]]; then tool ours bbox "$2" "$layer" $box 1
    else tool gdal bbox "$2" "$layer" $box; fi
}
op_bbox_unindexed() { bbox "$1" "$src"; }
op_bbox_indexed() { bbox "$1" "$WORK/indexed-$1.gpkg"; }

op_write() {
    rm -f "$WORK/written-$1.gpkg"
    tool "$1" write "$src" "$layer" "$WORK/written-$1.gpkg" no
}

op_write_with_index() {
    rm -f "$WORK/written-$1.gpkg"
    tool "$1" write "$src" "$layer" "$WORK/written-$1.gpkg" yes
}

# The `ogr2ogr` flags do what `gpkg copy` does with an unindexed source: keep
# the fids, commit every 10,000 rows, and build no index.
op_copy() {
    rm -f "$WORK/copy-$1.gpkg"
    if [[ $1 == ours ]]; then
        timed ours "$GPKG" copy "$src" "$WORK/copy-$1.gpkg"
    else
        timed gdal "$OGR2OGR" -q -f GPKG -lco SPATIAL_INDEX=NO -preserve_fid -gt 10000 \
            "$WORK/copy-$1.gpkg" "$src"
    fi
}

# Prints the counts one side's last run of `op` reported.
counts() {
    local side="$1" op="$2"
    case "$op" in
        copy) "$OURS" info "$WORK/copy-$side.gpkg" "$layer" | sed -n 's/^rows=/rows=/p' ;;
        index_build) echo "file_bytes=$(stat -c%s "$WORK/indexed-$side.gpkg" 2>/dev/null \
            || stat -f%z "$WORK/indexed-$side.gpkg")" ;;
        *) grep -E '^(rows|hits|geometries)=' "$WORK/last-$side.txt" || true ;;
    esac | tr '\n' ' '
}

# Runs `op` `reps` times on this crate's side, and on GDAL's unless `$2` is
# `cached`, alternating which side runs first. Prints one tab-separated
# line per side: side, op, median ms, median MiB, counts.
measure() {
    local op="$1" mode="$2" ours=() gdal=() rep
    for rep in $(seq 1 "$REPS"); do
        if [[ $mode == cached ]]; then
            ours+=("$("op_$op" ours)")
        elif (( rep % 2 )); then
            ours+=("$("op_$op" ours)"); gdal+=("$("op_$op" gdal)")
        else
            gdal+=("$("op_$op" gdal)"); ours+=("$("op_$op" ours)")
        fi
    done
    local side samples
    for side in ours gdal; do
        [[ $side == gdal && $mode == cached ]] && continue
        if [[ $side == ours ]]; then samples=("${ours[@]}"); else samples=("${gdal[@]}"); fi
        printf '%s\t%s\t%s\t%s\t%s\n' "$side" "$op" \
            "$(column_median 1 "${samples[@]}")" "$(column_median 2 "${samples[@]}")" \
            "$(counts "$side" "$op")"
    done
}

run() {
    command -v "$GDAL_CONFIG" >/dev/null || { echo "$GDAL_CONFIG not found" >&2; exit 1; }
    [[ -x /usr/bin/time ]] || { echo "/usr/bin/time not found" >&2; exit 1; }

    WORK="$(mktemp -d)"
    trap 'rm -rf "$WORK"' EXIT

    echo "building both tools"
    cargo build --release --quiet --manifest-path "$ROOT/Cargo.toml" \
        -p geopackage --features arrow,bundled --example dataset_bench
    cargo build --release --quiet --manifest-path "$ROOT/Cargo.toml" -p geopackage-cli
    local prefix version cache
    prefix="$("$GDAL_CONFIG" --prefix)"
    version="$("$GDAL_CONFIG" --version)"
    GDAL="$WORK/gdal_dataset_bench"
    OGR2OGR="$prefix/bin/ogr2ogr"
    # shellcheck disable=SC2046
    cc -O2 -o "$GDAL" "$ROOT/scripts/gdal_dataset_bench.c" \
        $("$GDAL_CONFIG" --cflags) $("$GDAL_CONFIG" --libs) -Wl,-rpath,"$prefix/lib"
    # As activating a conda environment would set them.
    if [[ $prefix != /usr && -d $prefix/share/proj ]]; then
        export PROJ_DATA="$prefix/share/proj" GDAL_DATA="$prefix/share/gdal"
    fi
    mkdir -p "$GDAL_CACHE"
    cache="$GDAL_CACHE/gdal-$version.tsv"
    touch "$cache"

    echo
    echo "environment"
    echo "  $(uname -sr), $(nproc 2>/dev/null || sysctl -n hw.ncpu) CPUs"
    echo "  $("$GPKG" --version)"
    echo "  GDAL $version at $prefix; results kept in $cache"
    echo "  reps=$REPS"
    echo "  host load at start:$(uptime | sed 's/.*load averages*://')"

    local entry name file rows lines op
    for entry in "${DATASETS[@]}"; do
        IFS='|' read -r name layer file rows box <<<"$entry"
        src="$DIR/$file"
        [[ -f "$src" ]] || { echo "missing $src; run the subset step" >&2; exit 1; }

        echo
        echo "=== $name ($file, layer $layer)"
        "$OURS" info "$src" "$layer" | sed 's/^/  /'
        # A dataset's GDAL results are used only if every operation has one.
        local key cached=cached
        key="$name/$rows/${box// /,}/$(stat -c%s "$src" 2>/dev/null || stat -f%z "$src")"
        for op in "${OPS[@]}"; do
            grep -qF "$key	gdal	$op	" "$cache" || cached=measure
        done
        [[ $cached == cached ]] && echo "  GDAL: from $cache"
        lines=""
        for op in "${OPS[@]}"; do
            lines+="$(measure "$op" "$cached")"$'\n'
        done
        if [[ $cached == measure ]]; then
            grep -vF "$key	" "$cache" >"$WORK/cache" || true
            grep "^gdal	" <<<"$lines" | sed "s|^|$key	|" >>"$WORK/cache"
            cp "$WORK/cache" "$cache"
        fi
        lines+="$(grep -F "$key	gdal	" "$cache" | cut -f2-)"
        printf '  %-18s %10s %10s %9s %9s\n' "" "ours ms" "gdal ms" "ours MiB" "gdal MiB"
        for op in "${OPS[@]}"; do
            local o g
            o="$(grep -m1 "^ours	$op	" <<<"$lines")"
            g="$(grep -m1 "^gdal	$op	" <<<"$lines")"
            printf '  %-18s %10s %10s %9s %9s\n' "$op" \
                "$(cut -f3 <<<"$o")" "$(cut -f3 <<<"$g")" "$(cut -f4 <<<"$o")" "$(cut -f4 <<<"$g")"
            printf '    ours: %s\n    gdal: %s\n' "$(cut -f5 <<<"$o")" "$(cut -f5 <<<"$g")"
        done
        rm -f "$WORK"/*.gpkg
    done

    echo
    echo "host load at end:$(uptime | sed 's/.*load averages*://')"
}

case "$MODE" in
    fetch) fetch ;;
    subset) subset "${3:-}" ;;
    run) run ;;
    *) echo "usage: scripts/bench_datasets.sh <fetch|subset|run> [dir] [reps|out]" >&2; exit 1 ;;
esac
