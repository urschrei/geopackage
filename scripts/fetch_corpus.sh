#!/usr/bin/env bash
#
# Fetch the external GeoPackage soak corpus into ./corpus (git-ignored).
#
# These are larger, published sample GeoPackages written by other tools and at
# other spec versions than our committed fixtures -- material for the ignored
# corpus sweep in geopackage/tests/corpus_external.rs, which opens every file it
# finds under corpus/, enumerates the layers and iterates all features. The
# files are deliberately NOT committed (they are megabytes and third-party); this
# script downloads them on demand and verifies each against a pinned sha256, so a
# silently changed or truncated download is caught.
#
# All URLs were confirmed to resolve on 2026-07-24. Re-run any time; existing,
# verified files are left untouched.
#
# Usage: scripts/fetch_corpus.sh

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
DEST="${GEOPACKAGE_CORPUS_DIR:-$REPO_ROOT/corpus}"
mkdir -p "$DEST"

# filename | sha256 | url
CORPUS=(
  "gdal_sample_v1.0.gpkg|aa889c39253dab7f4ce8de2ede6a41a726eea006d6b00ac978541eb30d81633c|http://www.geopackage.org/data/gdal_sample.gpkg"
  "gdal_sample_v1.2_no_extensions.gpkg|5eea4859ccfde65d845d6a0252632e87ae3db5345fddf546a9c054482fdd40bf|http://www.geopackage.org/data/gdal_sample_v1.2_no_extensions.gpkg"
  "ogc_sample1_2.gpkg|d6bf8dc8972e94e08b017c018d773cf9f4a44403e652f105ca90d02b6ffba842|http://www.geopackage.org/data/sample1_2.gpkg"
  "nga_rivers.gpkg|3edf9efc4d5b29170ac30fbbc6a77d9feaca068c8fb399566041a681a49b5a9a|https://github.com/ngageoint/geopackage-js/raw/master/test/fixtures/rivers.gpkg"
)

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  else
    shasum -a 256 "$1" | cut -d' ' -f1
  fi
}

for entry in "${CORPUS[@]}"; do
  IFS='|' read -r name sha url <<<"$entry"
  path="$DEST/$name"

  if [[ -f "$path" ]] && [[ "$(sha256_of "$path")" == "$sha" ]]; then
    echo "ok (cached)   $name"
    continue
  fi

  echo "downloading   $name"
  if ! curl -fsSL --max-time 120 -o "$path" "$url"; then
    echo "  FAILED to download $url" >&2
    rm -f "$path"
    continue
  fi

  got="$(sha256_of "$path")"
  if [[ "$got" != "$sha" ]]; then
    echo "  SHA256 MISMATCH for $name" >&2
    echo "    expected $sha" >&2
    echo "    got      $got" >&2
    rm -f "$path"
    exit 1
  fi
  echo "ok            $name ($(wc -c <"$path" | tr -d ' ') bytes)"
done

echo
echo "Corpus in $DEST:"
ls -1 "$DEST"
echo
echo "Run the sweep with: cargo test -p geopackage --test corpus_external -- --ignored --nocapture"
