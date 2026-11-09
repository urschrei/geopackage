# geopackage-rs

Python bindings for the [`geopackage`](https://crates.io/crates/geopackage)
Rust crate: read, write, validate and repair OGC GeoPackage files. The
bindings are one abi3 wheel per platform for CPython 3.9 and later, with
SQLite vendored inside. This is the native half of the
[QGIS plugin](../qgis-plugin).

The same caveat applies as for the crate: this is a research preview, so
don't use it on data you can't lose.

```python
from geopackage_rs import GeoPackage

with GeoPackage("roads.gpkg", lenient=True) as gpkg:
    for finding in gpkg.validate():
        print(finding.severity, finding.message, finding.repair)

    roads = gpkg.layer("roads")
    print(roads.field_names, roads.geometry_column, roads.spatial_index_status())
    for fid, wkb, values in roads.features(bbox=(0, 0, 10, 10), where="lanes > ?1", params=[2]):
        ...

    dem = gpkg.coverage("dem")
    print(dem.values_at([(1.5, 52.1), (1.6, 52.2)], interpolation="bilinear"))
```

Geometry crosses the boundary as ISO WKB, so `shapely.from_wkb`,
`QgsGeometry.fromWkb` and `ogr.CreateGeometryFromWkb` all read it. Each
`features()` call streams from its own read-only connection on a background
thread, in chunks, with the GIL released while it waits.

## Building and testing

```sh
pip install maturin pytest
maturin develop -m geopackage-py/Cargo.toml
pytest geopackage-py/tests
```

`maturin build` makes a wheel. The vendored SQLite is the default
(`bundled`), because a host SQLite may lack RTree. The module exports only
`PyInit__geopackage_rs`, so its SQLite is private to it, and it is a second
copy in a process that also loads SQLite through Python's `sqlite3` or GDAL.
Two copies do not see each other's locks, and closing a file in one releases
the locks that the other holds on that file. So do not write to a file
through this module while the same process has it open through another
SQLite library.

The QGIS plugin's build links QGIS's SQLite instead, which QGIS, GDAL and
QGIS's Python share: `--no-default-features --auditwheel skip`, with the
settings that `scripts/host_sqlite.py` prints on macOS.

## Not yet bound

Arrow streams (`__arrow_c_stream__`), tile pyramid writes, metadata and
related tables, and schema changes, which the crate doesn't have yet.
