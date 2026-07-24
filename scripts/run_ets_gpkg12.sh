#!/usr/bin/env bash
#
# Run the OGC ETS (Executable Test Suite) ets-gpkg12 conformance tests against a
# GeoPackage file, for M2 acceptance criterion 1.
#
# ets-gpkg12 validates GeoPackage 1.2 semantics (no 1.3/1.4 ETS exists as of
# 2026-07); the 1.4-specific checks are covered separately by the manual
# checklist in geopackage/tests/gdal_interop.rs. This suite still exercises the
# large shared core (SQLite container, gpkg_contents, gpkg_geometry_columns,
# SRS, GPB headers, RTree extension registration) that a 1.4 file must also
# satisfy.
#
# The all-in-one (aio) jar bundles TEAM Engine and every dependency. It is ~34
# MB and is NOT committed: this script fetches it on demand into a cache
# directory and verifies it against the pinned sha256 below, so a changed or
# truncated download is caught.
#
# Requires: java (>= 17). Confirmed to resolve on 2026-07-24.
#
# Usage: scripts/run_ets_gpkg12.sh <file.gpkg> [output-dir]

set -euo pipefail

# ets-gpkg12 1.3 all-in-one jar (groupId org.opengis.cite), Maven Central.
JAR_VERSION="1.3"
JAR_URL="https://repo1.maven.org/maven2/org/opengis/cite/ets-gpkg12/${JAR_VERSION}/ets-gpkg12-${JAR_VERSION}-aio.jar"
JAR_SHA256="59b15a8e908f27565983e3751f417363048913832f9c66cc538ee58859351a77"

if [[ $# -lt 1 ]]; then
  echo "usage: $0 <file.gpkg> [output-dir]" >&2
  exit 2
fi

GPKG_FILE="$1"
OUT_DIR="${2:-$(mktemp -d)}"
CACHE_DIR="${ETS_GPKG12_CACHE:-${TMPDIR:-/tmp}/ets-gpkg12-cache}"
JAR_PATH="${CACHE_DIR}/ets-gpkg12-${JAR_VERSION}-aio.jar"

if ! command -v java >/dev/null 2>&1; then
  echo "java not found: skipping ets-gpkg12 (record this and skip)." >&2
  exit 3
fi

mkdir -p "$CACHE_DIR" "$OUT_DIR"

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

if [[ ! -f "$JAR_PATH" ]]; then
  echo "Fetching ets-gpkg12 ${JAR_VERSION} aio jar (~34 MB) ..." >&2
  curl -sSL -o "$JAR_PATH" "$JAR_URL"
fi
GOT="$(sha256_of "$JAR_PATH")"
if [[ "$GOT" != "$JAR_SHA256" ]]; then
  echo "sha256 mismatch for $JAR_PATH" >&2
  echo "  expected $JAR_SHA256" >&2
  echo "  got      $GOT" >&2
  exit 4
fi

# The TestNGController (Main-Class of the aio jar) takes a Java Properties XML
# test-run-arguments file whose "iut" entry is the file under test.
ABS_GPKG="$(cd "$(dirname "$GPKG_FILE")" && pwd)/$(basename "$GPKG_FILE")"
PROPS="${OUT_DIR}/test-run-props.xml"
cat >"$PROPS" <<XML
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE properties SYSTEM "http://java.sun.com/dtd/properties.dtd">
<properties version="1.0">
  <comment>ets-gpkg12 test run arguments</comment>
  <entry key="iut">${ABS_GPKG}</entry>
</properties>
XML

echo "Running ets-gpkg12 ${JAR_VERSION} against ${ABS_GPKG}" >&2
echo "Output dir: ${OUT_DIR}" >&2
# The controller writes a session directory (TestNG HTML + result XML) and
# prints its location. Tee everything so the summary is captured.
java -jar "$JAR_PATH" "$PROPS" 2>&1 | tee "${OUT_DIR}/ets-gpkg12-run.log"

echo >&2
echo "ets-gpkg12 run complete. Report + logs under: ${OUT_DIR}" >&2
