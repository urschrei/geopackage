#!/usr/bin/env python3
"""Regenerate geopackage-core/src/srs/epsg_wkt.rs from the local GDAL/PROJ.

Extracts WKT1 definitions for the vendored EPSG subset using `gdalsrsinfo`
(GDAL CLI; no Python bindings required) and writes them as a Rust static
table. Run from the repository root:

    python3 scripts/generate_epsg_wkt.py

The output file is committed; this script only needs re-running to refresh
definitions against a newer EPSG dataset or to change the vendored subset.

EPSG dataset (c) IOGP, distributed via PROJ; see the module docs in the
generated file for attribution.
"""

import subprocess
import sys
from pathlib import Path

# The vendored subset: codes that cover the bulk of real-world GeoPackage
# traffic. WGS 84 UTM zones (32601-32660, 32701-32760) are synthesised in
# srs.rs from a template, not listed here; 32633/32733 appear only as test
# references for that synthesis.
CODES = [
    # Geographic 2D
    4326,  # WGS 84
    4258,  # ETRS89
    4269,  # NAD83
    4267,  # NAD27
    4283,  # GDA94
    7844,  # GDA2020
    4490,  # CGCS2000
    6668,  # JGD2011
    # Geographic 3D CRSs (e.g. 4979, WGS 84 3D) are deliberately absent:
    # they cannot be expressed in WKT1; the library registers them as WKT2
    # through the gpkg_crs_wkt extension instead.
    # Projected, global
    3857,  # WGS 84 / Pseudo-Mercator
    3395,  # WGS 84 / World Mercator
    3035,  # ETRS89-extended / LAEA Europe
    3031,  # WGS 84 / Antarctic Polar Stereographic
    3413,  # WGS 84 / NSIDC Sea Ice Polar Stereographic North
    # Projected, national/regional
    27700,  # OSGB36 / British National Grid
    2157,  # IRENET95 / Irish Transverse Mercator
    2154,  # RGF93 v1 / Lambert-93 (France)
    25830,  # ETRS89 / UTM zone 30N (Spain)
    25832,  # ETRS89 / UTM zone 32N (Germany et al.)
    25833,  # ETRS89 / UTM zone 33N
    28992,  # Amersfoort / RD New (Netherlands)
    31370,  # BD72 / Belgian Lambert 72
    2056,  # CH1903+ / LV95 (Switzerland)
    3006,  # SWEREF99 TM (Sweden)
    2193,  # NZGD2000 / New Zealand Transverse Mercator 2000
    5070,  # NAD83 / Conus Albers (USA)
    3577,  # GDA94 / Australian Albers
]

UTM_TEST_REFERENCES = [32633, 32733]


def wkt1(code: int) -> str:
    out = subprocess.run(
        ["gdalsrsinfo", "-o", "wkt1", "--single-line", f"EPSG:{code}"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    if not out.startswith(("PROJCS[", "GEOGCS[")):
        sys.exit(f"unexpected gdalsrsinfo output for EPSG:{code}: {out[:80]}")
    return out


def srs_name(wkt: str) -> str:
    # First quoted string is the PROJCS/GEOGCS name.
    start = wkt.index('"') + 1
    return wkt[start : wkt.index('"', start)]


def versions() -> str:
    gdal = subprocess.run(
        ["gdalsrsinfo", "--version"], check=True, capture_output=True, text=True
    ).stdout.strip()
    return gdal


def main() -> None:
    root = Path(__file__).resolve().parent.parent
    out_path = root / "geopackage-core" / "src" / "srs" / "epsg_wkt.rs"

    rows = []
    for code in CODES:
        w = wkt1(code)
        rows.append((code, srs_name(w), w))
        print(f"EPSG:{code}  {srs_name(w)}", file=sys.stderr)

    refs = [(code, wkt1(code)) for code in UTM_TEST_REFERENCES]

    lines = [
        "//! Vendored EPSG WKT1 definitions.",
        "//!",
        "//! GENERATED FILE - do not edit by hand. Regenerate with",
        "//! `python3 scripts/generate_epsg_wkt.py` (requires GDAL's",
        "//! `gdalsrsinfo` on PATH).",
        "//!",
        f"//! Extracted with: {versions()}",
        "//!",
        "//! The definitions derive from the EPSG dataset, (c) International",
        "//! Association of Oil & Gas Producers (IOGP), distributed via PROJ,",
        "//! and are used under the EPSG Terms of Use",
        "//! (<https://epsg.org/terms-of-use.html>).",
        "",
        "/// `(epsg_code, srs_name, wkt1_definition)` rows, ascending by code.",
        "pub(super) const EPSG_WKT1: &[(i32, &str, &str)] = &[",
    ]
    for code, name, w in sorted(rows):
        lines.append(f'    ({code}, "{name}", r#"{w}"#),')
    lines.append("];")
    lines.append("")
    lines.append("/// Reference WKT for UTM-zone synthesis tests.")
    lines.append("#[cfg(test)]")
    lines.append("pub(super) const UTM_TEST_REFERENCES: &[(i32, &str)] = &[")
    for code, w in refs:
        lines.append(f'    ({code}, r#"{w}"#),')
    lines.append("];")
    lines.append("")

    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_text("\n".join(lines))
    print(f"wrote {out_path}", file=sys.stderr)


if __name__ == "__main__":
    main()
