#!/usr/bin/env bash
#
# Compare `gpkg copy` against `ogr2ogr` copying the same GeoPackage.
#
# Both arms read one feature layer and write a new file with no spatial index.
# The `ogr2ogr` flags match what `gpkg copy` does: keep the source fids, commit
# every 10,000 rows, and build no index (`gpkg copy` builds one only where the
# source had one, and the benchmark datasets have none).
#
# Method, per dataset:
#
#   1. Wall time through hyperfine: one warm-up, then `reps` runs of each arm,
#      with the output deleted and the page cache synced before every run.
#   2. Peak resident memory and wall time through GNU time, two runs of each
#      arm.
#   3. The row count and file size of each arm's output, so that a fast arm
#      that wrote less is visible.
#   4. The write path that GDAL reports in its debug output.
#
# Each `ogr2ogr` argument is one GDAL arm. An arm from a conda environment
# gets `PROJ_DATA` and `GDAL_DATA` from its own prefix, as activating the
# environment would set them.
#
# Needs hyperfine, GNU time at /usr/bin/time, and GNU stat. The outputs are
# written under `$TMPDIR`, which must have room for two copies of the largest
# dataset. The last line of output is `exited <status>`, whatever the status.
#
# Usage: scripts/compare_gdal_copy.sh <datadir> <gpkg> [reps] [ogr2ogr...]

set -euo pipefail

DATA="${1:?usage: compare_gdal_copy.sh <datadir> <gpkg> [reps] [ogr2ogr...]}"
GPKG="${2:?usage: compare_gdal_copy.sh <datadir> <gpkg> [reps] [ogr2ogr...]}"
REPS="${3:-5}"
shift $(( $# < 3 ? $# : 3 ))
OGRS=("$@")
if [[ ${#OGRS[@]} -eq 0 ]]; then
    OGRS=("$(command -v ogr2ogr)")
fi
FLAGS=(-q -f GPKG -lco SPATIAL_INDEX=NO -preserve_fid -gt 10000)
DATASETS=("admin|gadm_noidx.gpkg" "rivers|hydrorivers.gpkg" "buildings|ca_buildings.gpkg")

for b in "$GPKG" "${OGRS[@]}" /usr/bin/time; do
    [[ -x "$b" ]] || { echo "not executable: $b" >&2; exit 1; }
done
command -v hyperfine >/dev/null || { echo "hyperfine not found on PATH" >&2; exit 1; }

WORK="$(mktemp -d)"
on_exit() {
    local status=$?
    rm -rf "$WORK"
    echo "exited $status $(date -u +%FT%TZ)"
}
trap on_exit EXIT

# The command line of one GDAL arm, with its environment.
ogr_cmd() {
    local bin="$1" prefix
    prefix="$(cd "$(dirname "$bin")/.." && pwd)"
    if [[ -d "$prefix/share/proj" && "$prefix" != /usr ]]; then
        echo "env PROJ_DATA=$prefix/share/proj GDAL_DATA=$prefix/share/gdal $bin"
    else
        echo "$bin"
    fi
}

# A short name for one GDAL arm: `ogr2ogr` and its version.
ogr_name() {
    echo "ogr2ogr $($(ogr_cmd "$1") --version | sed -E 's/^GDAL ([0-9.]+).*/\1/')"
}

echo "start $(date -u +%FT%TZ)"
echo "environment"
echo "  $("$GPKG" --version)"
for ogr in "${OGRS[@]}"; do
    echo "  $(ogr_name "$ogr"): $($(ogr_cmd "$ogr") --version)"
done
echo "  $(hyperfine --version), $(nproc) CPUs, reps=$REPS"
echo "  host load at start:$(uptime | sed 's/.*load averages*://')"

for ds in "${DATASETS[@]}"; do
    IFS='|' read -r name file <<<"$ds"
    src="$DATA/$file"
    [[ -s "$src" ]] || { echo "missing dataset: $src" >&2; exit 1; }

    echo
    echo "=== $name: wall time"
    args=(-n gpkg "$GPKG copy $src $WORK/gpkg.gpkg")
    for i in "${!OGRS[@]}"; do
        args+=(-n "$(ogr_name "${OGRS[$i]}")" "$(ogr_cmd "${OGRS[$i]}") ${FLAGS[*]} $WORK/ogr$i.gpkg $src")
    done
    hyperfine --warmup 1 --runs "$REPS" --export-json "$WORK/$name.json" \
        --prepare "rm -f $WORK/*.gpkg* && sync" "${args[@]}"

    echo
    echo "=== $name: peak memory (wall s, max RSS KiB)"
    for rep in 1 2; do
        arms=(gpkg "${!OGRS[@]}")
        for arm in "${arms[@]}"; do
            rm -f "$WORK"/*.gpkg*; sync
            if [[ $arm == gpkg ]]; then
                label=gpkg
                cmd="$GPKG copy $src $WORK/out.gpkg"
            else
                label="$(ogr_name "${OGRS[$arm]}")"
                cmd="$(ogr_cmd "${OGRS[$arm]}") ${FLAGS[*]} $WORK/out.gpkg $src"
            fi
            # Word splitting of `cmd` is intended: it is a command line.
            # shellcheck disable=SC2086
            /usr/bin/time -o "$WORK/time.txt" -f '%e %M' $cmd >/dev/null 2>"$WORK/err.txt" \
                || { echo "FAILED: $label"; cat "$WORK/err.txt"; }
            echo "  $label, run $rep: $(cat "$WORK/time.txt")"
            if [[ $rep == 1 ]]; then
                # Not piped: `gpkg` exits 141 when `grep -m1` closes the pipe.
                info="$("$GPKG" info "$WORK/out.gpkg")"
                rows="$(grep -m1 'rows:' <<<"$info" | tr -s ' ')"
                echo "    output:$rows, $(stat -c %s "$WORK/out.gpkg") bytes"
            fi
        done
    done

    echo
    echo "=== $name: GDAL write path"
    for ogr in "${OGRS[@]}"; do
        rm -f "$WORK"/*.gpkg*
        echo "  $(ogr_name "$ogr"):"
        # shellcheck disable=SC2046
        env CPL_DEBUG=ON $(ogr_cmd "$ogr" | sed 's/^env //') "${FLAGS[@]}" "$WORK/debug.gpkg" "$src" 2>&1 \
            | grep -E 'WriteArrowBatch|GPKG: Using [0-9]+ threads' | sort -u | sed 's/^/    /'
    done
    rm -f "$WORK"/*.gpkg*
done

echo
echo "host load at end:$(uptime | sed 's/.*load averages*://')"
echo "done $(date -u +%FT%TZ)"
