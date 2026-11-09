# GeoPackage Toolkit for QGIS

A vector data provider and Processing algorithms built on the `geopackage`
crate, through the [`geopackage-rs`](../geopackage-py) bindings. QGIS already
reads and writes GeoPackage through GDAL. The provider lets a layer use the
crate instead, and the algorithms cover what the crate does that GDAL
doesn't.

It's experimental, as the crate is. Work on copies.

| Algorithm | Group | What it does |
|---|---|---|
| Validate GeoPackage | Inspect and repair | `GeoPackage.validate`: findings, most severe first, each with its repair and the algorithm here that makes it. Read-only. |
| Repair spatial indexes | Inspect and repair | Audits each RTree. Replaces legacy (pre-1.4 or mixed) and stale trigger sets, rebuilds indexes that are out of step with their table, and optionally builds missing ones. **Modifies the file.** |
| Repair geometry and datetime encodings | Inspect and repair | Rewrites geometries whose WKB gives Z or M by a high-bit flag in a type code with ISO type codes, and `DATETIME` text in another ISO 8601 form (such as `2017-08-01T00:00:00.0Z`) in the GeoPackage form, where nothing is lost. **Modifies the file.** |
| List extensions | Inspect and repair | `gpkg_extensions`, each row classified as implemented, known, removed or unrecognised. |
| Sample coverage at points | Coverages | Nearest or bilinear value lookups in a tiled gridded coverage, with points reprojected into the coverage's CRS. |
| Coverage tile statistics | Coverages | Tile footprints with their recorded statistics and the statistics calculated from the decoded samples. |
| Check coverage samples (Requirement 21) | Coverages | Decodes every tile and reports NaN or infinite samples, which a header-only validator can't see. |
| Export layer to GeoPackage | Import and export | Bulk-writes any vector layer through the crate's writer and bulk RTree build, into a new file or a new table in an existing one. |

The first four are also on the Plugins menu.

## Data provider

The plugin registers a vector data provider, `geopackage_rs`. Its URIs are
OGR's, `/path/to/file.gpkg|layername=roads`, so a layer moves between the two
providers by changing only the provider key. To add a layer through it, use
*Plugins → GeoPackage Toolkit → Add GeoPackage layer (geopackage crate)*, or
from Python:

```python
QgsVectorLayer("/data/roads.gpkg|layername=roads", "roads", "geopackage_rs")
```

How it serves QGIS's requests:

| Request | How |
|---|---|
| Bounding box | The crate's RTree query, plus QGIS's exact-intersection test when asked |
| One feature id (Identify, attribute edits) | One synchronous lookup on the provider's connection |
| Several ids | `fid IN (...)` |
| Subset string | An SQLite `WHERE` clause, run by SQLite, as OGR's GeoPackage subset strings are. An invalid one is rejected when it's set |
| Attribute subset, no geometry | A column projection: unread columns and geometry blobs are never fetched |
| Filter expression | Evaluated by the provider on the rows it reads |
| Destination CRS, limit, order by, distance within | As QGIS's core providers do them |

Every read but the single-id lookup streams from its own read-only connection
on its own thread, so iterators are safe on QGIS's rendering threads. Edit
sessions can add, delete and change features and their geometries; a commit
writes through the crate, so the spatial index, the extent and the file's
validity stay right. Geometries are converted to the column's multi and Z/M
form on the way in. Adding, deleting and renaming fields aren't offered,
because the crate has no schema changes yet (F5 in
`roadmap/09-c-api-sense-check.md`).

**Two implementations, one key.** On QGIS 4, when the plugin ships a build
of the native provider (`geopackage-qgis`, Rust behind a C++ shim) for the
running QGIS minor version, that serves `geopackage_rs`. Otherwise the Python
provider here does. They behave the same, and the provider tests run against
both, so a project opens with either. QGIS's message log says when the
Python provider is used, and why. `GPKG_TOOLKIT_NATIVE_PROVIDER` names a
native build to load instead, or `0` turns it off.

**Speed.** On 200,000 points the native provider reads in a half to a third
of OGR's time. The Python provider takes 3 to 4 times OGR's time for a full
scan, and about 3 times for a bounding-box read of 2,000 features. That cost
is building each `QgsFeature` in Python, not the crate: streaming the same
rows through the bindings alone takes about a quarter of the provider's time
(363 ms of 1,416 ms).

## Install

From a release: download `gpkg_toolkit-<version>.zip` and use *Plugins →
Manage and Install Plugins → Install from ZIP*. The zip holds the native
module for Linux x86_64 and aarch64, macOS (universal2) and Windows x86_64,
in `ext-libs/`, and the native provider for QGIS 4.0 and 4.2 on each, in
`ext-libs/<platform>/qgis-<version>/`.

Supports QGIS 3.28 and later, including QGIS 4. The `compat` module resolves
the enums that moved between them.

## Develop

```sh
# A Python that sees QGIS's bindings, with the native module built into it.
# The module links QGIS's SQLite (on macOS, with the settings that
# scripts/host_sqlite.py prints; on Windows, omit the two flags).
python3 -m venv --system-site-packages venv
venv/bin/pip install maturin pytest
venv/bin/maturin develop -m geopackage-py/Cargo.toml --no-default-features

# Headless tests: every algorithm, through Processing, including a GDAL read
# of what the export wrote.
QT_QPA_PLATFORM=offscreen venv/bin/python -m pytest qgis-plugin/tests

# A plugin zip for this platform.
venv/bin/maturin build --release -m geopackage-py/Cargo.toml \
    --no-default-features --auditwheel skip -o dist
venv/bin/python scripts/package_qgis_plugin.py dist/*.whl
```

**One SQLite.** On Linux and macOS, the module and the native provider link
the SQLite that QGIS has loaded, which GDAL and QGIS's Python share. Two
copies of SQLite in one process do not see each other's locks: a copy of
their own would let the plugin write to a file while QGIS was writing to it.
`tests/test_one_sqlite.py` checks that a lock held through QGIS's SQLite
stops the plugin's writes. Windows keeps a lock for each file handle, so
there both keep their own SQLite.

To run it in the desktop app, symlink `qgis-plugin/gpkg_toolkit` into your
profile's `python/plugins` directory. With the `maturin develop` module
importable from QGIS's Python, `ext-libs/` isn't needed. Where it isn't, as
in QGIS.app on macOS, `scripts/dev_qgis_plugin.py` builds the module, and
the native provider for QGIS 4, into `ext-libs/`. Restart QGIS after each
build. `geopackage-qgis/README.md` has the steps.

## Next

- Schema edits in the provider (adding, deleting and renaming fields), once
  the crate has them.
- Coverage writing from a raster layer, once the crate has a TIFF encoder path
  exposed for float coverages.
- Native provider builds for each new QGIS minor version (see
  `geopackage-qgis/README.md`).
