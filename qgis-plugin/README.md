# GeoPackage Toolkit for QGIS

QGIS Processing algorithms built on the `geopackage` crate, through the
[`geopackage-rs`](../geopackage-py) bindings. QGIS already reads and writes
GeoPackage through GDAL, so the plugin covers what this crate does that GDAL
doesn't.

It's experimental, as the crate is. Work on copies.

| Algorithm | Group | What it does |
|---|---|---|
| Validate GeoPackage | Inspect and repair | `GeoPackage.validate`: findings, most severe first, each with its repair. Read-only. |
| Repair spatial indexes | Inspect and repair | Audits each RTree. Replaces legacy (pre-1.4 or mixed) and stale trigger sets, rebuilds indexes that are out of step with their table, and optionally builds missing ones. **Modifies the file.** |
| List extensions | Inspect and repair | `gpkg_extensions`, each row classified as implemented, known, removed or unrecognised. |
| Sample coverage at points | Coverages | Nearest or bilinear value lookups in a tiled gridded coverage, with points reprojected into the coverage's CRS. |
| Coverage tile statistics | Coverages | Tile footprints with their recorded statistics and the statistics calculated from the decoded samples. |
| Check coverage samples (Requirement 21) | Coverages | Decodes every tile and reports NaN or infinite samples, which a header-only validator can't see. |
| Export layer to GeoPackage | Import and export | Bulk-writes any vector layer through the crate's writer and bulk RTree build, into a new file or a new table in an existing one. |

The first three are also on the Plugins menu.

## Install

From a release: download `gpkg_toolkit-<version>.zip` and use *Plugins →
Manage and Install Plugins → Install from ZIP*. The zip holds the native
module for Linux x86_64 and aarch64, macOS (universal2) and Windows x86_64,
in `ext-libs/`.

Supports QGIS 3.28 and later, including QGIS 4. The `compat` module resolves
the enums that moved between them.

## Develop

```sh
# A Python that sees QGIS's bindings, with the native module built into it.
python3 -m venv --system-site-packages venv
venv/bin/pip install maturin pytest
venv/bin/maturin develop -m geopackage-py/Cargo.toml

# Headless tests: every algorithm, through Processing, including a GDAL read
# of what the export wrote.
QT_QPA_PLATFORM=offscreen venv/bin/python -m pytest qgis-plugin/tests

# A plugin zip for this platform.
venv/bin/maturin build --release -m geopackage-py/Cargo.toml -o dist
venv/bin/python scripts/package_qgis_plugin.py dist/*.whl
```

To run it in the desktop app, symlink `qgis-plugin/gpkg_toolkit` into your
profile's `python/plugins` directory. With the `maturin develop` module
importable from QGIS's Python, `ext-libs/` isn't needed.

## Next

- **A vector data provider** (`QgsVectorDataProvider` in Python) so QGIS
  renders and edits layers through the crate rather than OGR. The bindings
  already have what it needs: bbox, attribute and column-projected streaming
  reads, feature lookup by id, and batched inserts, updates and deletes. The
  gaps are on the crate side: schema changes (F5 in
  `roadmap/09-c-api-sense-check.md`) and layer deletion (F6).
- Coverage writing from a raster layer, once the crate has a TIFF encoder path
  exposed for float coverages.
