#!/usr/bin/env bash
#
# Run GDAL's own GeoPackage validator against a file.
#
# The third validator alongside scripts/run_ets_gpkg12.sh (the OGC executable
# test suite) and scripts/run_pdok_validator.sh. It is worth having separately
# because it is the only one of the three that checks the gpkg_contents
# last_change format: the OGC ETS leaves the extent check commented out, and the
# PDOK validator mentions neither last_change nor the extent columns anywhere in
# its 27 checks. What GDAL checks here is still format and schema only. Nothing
# in any of the three validates that a recorded extent matches the data, which
# is why geopackage/src/extent.rs takes the position it does.
#
# It runs one of two ways, preferred in order:
#   1. `gdal driver gpkg validate`, the CLI wrapper added in GDAL 3.13.
#   2. `python3 -m osgeo_utils.samples.validate_gpkg`, the script that wrapper
#      embeds, which ships with the GDAL Python bindings and long predates it.
#      This is the route on any GDAL older than 3.13.
#
# The two spell the deeper check differently: `--full-check` on the 3.13
# command, `--extra` on the sample script. This takes `--full-check` and
# translates. Either way it adds row-level type conformance (every value against
# its declared column type) and reads the whole file, so it is opt-in.
#
# Known false positive, as of GDAL master (2026-07), on the default run as well
# as under --full-check:
#
#   Req 152: Inconsistent empty_flag vs geometry content
#
# on any layer containing an empty geometry. The check reads the GPB empty flag
# from bit 3 of the header flags byte:
#
#   empty_flag = ((flags >> 3) & 1) == 1
#
# but the spec puts the empty-geometry flag at bit 4 and the envelope indicator
# at bits 1 to 3, which the same function reads as `(flags >> 1) & 7`. So it
# tests the top bit of the envelope field and calls it the empty flag. Verified
# by running it against a file GDAL itself wrote: the GPKG driver encodes an
# empty geometry with flags 0x11 (bit 4 set, no envelope), this crate encodes
# the identical byte, and the check rejects both. Ignore that one line; every
# other finding is real.
#
# Usage: scripts/run_gdal_validator.sh <file.gpkg> [--full-check]

set -euo pipefail

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <file.gpkg> [--full-check]" >&2
  exit 2
fi

GPKG_FILE="$1"
shift
if [[ ! -f "$GPKG_FILE" ]]; then
  echo "no such file: $GPKG_FILE" >&2
  exit 2
fi

# Preferred: the 3.13 command, if this GDAL has it. `gdal driver gpkg` exists
# earlier with other subcommands, so probe for `validate` itself rather than for
# the binary.
if command -v gdal >/dev/null 2>&1 && gdal driver gpkg validate --help >/dev/null 2>&1; then
  echo "Running gdal driver gpkg validate against ${GPKG_FILE}" >&2
  exec gdal driver gpkg validate "$@" "$GPKG_FILE"
fi

# Fallback: the sample script the command wraps, which spells the deeper check
# `--extra`.
if python3 -c "import osgeo_utils.samples.validate_gpkg" >/dev/null 2>&1; then
  SAMPLE_ARGS=()
  for arg in "$@"; do
    if [[ "$arg" == "--full-check" ]]; then
      SAMPLE_ARGS+=("--extra")
    else
      SAMPLE_ARGS+=("$arg")
    fi
  done
  echo "Running osgeo_utils.samples.validate_gpkg against ${GPKG_FILE}" >&2
  echo "(GDAL 3.13 exposes the same checks as: gdal driver gpkg validate)" >&2
  # `${a[@]+"${a[@]}"}` rather than `"${a[@]}"`: under `set -u`, macOS's bash 3.2
  # treats an empty array expansion as an unbound variable.
  exec python3 -m osgeo_utils.samples.validate_gpkg \
    ${SAMPLE_ARGS[@]+"${SAMPLE_ARGS[@]}"} "$GPKG_FILE"
fi

echo "Found neither 'gdal driver gpkg validate' (GDAL 3.13+) nor the GDAL Python" >&2
echo "bindings providing osgeo_utils.samples.validate_gpkg. Install GDAL with its" >&2
echo "Python bindings, then re-run." >&2
exit 3
