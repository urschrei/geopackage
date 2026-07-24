#!/usr/bin/env bash
#
# Run the PDOK geopackage-validator against a GeoPackage file (M2 acceptance
# criterion 1, advisory tier).
#
# The PDOK validator applies stricter naming/index/SRS/geometry rules than the
# OGC spec mandates; per roadmap/08-testing-conformance.md its findings are
# treated as advisory where they exceed the spec (e.g. RQ13 "single SRS across
# all geometry tables" — multiple SRS is spec-legal).
#
# It runs one of two ways, preferred in order:
#   1. A local `geopackage-validator` CLI on PATH (pip package
#      `pdok-geopackage-validator`, needs the osgeo/GDAL Python bindings —
#      trivial when system GDAL is installed:
#        python3 -m venv --system-site-packages env
#        env/bin/pip install pdok-geopackage-validator numpy
#      then env/bin/geopackage-validator ...).
#   2. The Docker image `pdok/geopackage-validator` (needs a running daemon).
#
# Usage: scripts/run_pdok_validator.sh <file.gpkg>

set -euo pipefail

IMAGE="${PDOK_VALIDATOR_IMAGE:-pdok/geopackage-validator:latest}"

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <file.gpkg>" >&2
  exit 2
fi

GPKG_FILE="$1"
if [[ ! -f "$GPKG_FILE" ]]; then
  echo "no such file: $GPKG_FILE" >&2
  exit 2
fi

# Preferred: a local CLI.
if command -v geopackage-validator >/dev/null 2>&1; then
  echo "Running local geopackage-validator against ${GPKG_FILE}" >&2
  exec geopackage-validator validate --gpkg-path "$GPKG_FILE"
fi

# Fallback: the Docker image.
if command -v docker >/dev/null 2>&1 && docker info >/dev/null 2>&1; then
  DIR="$(cd "$(dirname "$GPKG_FILE")" && pwd)"
  BASE="$(basename "$GPKG_FILE")"
  echo "Running PDOK geopackage-validator (${IMAGE}) against ${GPKG_FILE}" >&2
  exec docker run --rm -v "${DIR}:/data" "$IMAGE" \
    geopackage-validator validate --gpkg-path "/data/${BASE}"
fi

echo "Neither a local geopackage-validator CLI nor a running Docker daemon was" >&2
echo "found. Install the CLI (see header) or start Docker, then re-run." >&2
exit 3
