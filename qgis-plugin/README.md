# GeoPackage Toolkit for QGIS

A QGIS plugin built on the `geopackage` crate, through the
[`geopackage-rs`](../geopackage-py) bindings. It adds a vector data
provider, `geopackage_rs`, which reads and edits GeoPackage layers through
the crate instead of GDAL. It also adds Processing algorithms that validate
and repair files, and that read tiled gridded coverages.

It's experimental, as the crate is. Work on copies.

The user documentation is in the book:

- [How to install the QGIS plugin and add a layer through it](https://urschrei.github.io/geopackage/how-to/qgis-plugin.html)
- [QGIS plugin reference](https://urschrei.github.io/geopackage/qgis-plugin-reference.html):
  every algorithm's parameters and outputs, and how the provider serves
  QGIS's requests.

## Layout

| Path | Contents |
|---|---|
| `gpkg_toolkit/plugin.py` | Entry point: registers the provider, the Processing provider and the menu |
| `gpkg_toolkit/algorithms/` | The Processing algorithms |
| `gpkg_toolkit/data_provider.py` | The Python implementation of the `geopackage_rs` provider |
| `gpkg_toolkit/native.py` | Loads the `geopackage_rs` module from `ext-libs/<platform>/` |
| `gpkg_toolkit/native_provider.py` | Loads the native provider from `ext-libs/<platform>/qgis-<version>/` |
| `gpkg_toolkit/compat.py` | The enums that moved between QGIS 3 and 4 |
| `tests/` | Headless tests, run in CI under Ubuntu's QGIS 3.34 and conda-forge's QGIS 4.2 |

`ext-libs/` holds compiled parts, and the source tree has none. A release
zip has them for every platform, and `scripts/dev_qgis_plugin.py` builds
them for one.

## Speed

On 200,000 points the native provider reads in a half to a third
of OGR's time. The Python provider takes 3 to 4 times OGR's time for a full
scan, and about 3 times for a bounding-box read of 2,000 features. That cost
is building each `QgsFeature` in Python, not the crate: streaming the same
rows through the bindings alone takes about a quarter of the provider's time
(363 ms of 1,416 ms).

## Release

`.github/workflows/qgis-plugin.yml` builds the module and the native
provider for each platform when a release is published, packages them with
`scripts/package_qgis_plugin.py`, and attaches the zip to the release. Then
`.github/workflows/book.yml` rebuilds the book's site, with the plugin
repository QGIS reads, `qgis/plugins.xml`, which `scripts/plugins_xml.py`
writes from that zip.

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
