# geopackage-qgis

A native QGIS vector data provider over the `geopackage` crate, for QGIS 4
(Qt 6). It reads and edits. It started as a spike asking one question: can
a provider built on the crate match OGR's speed in QGIS? The Python
provider in `qgis-plugin` can't, as its README explains. This one can: on
every request with throughput behind it, it is faster than OGR (the figures
below).

The plugin uses it in place of its Python provider when it ships a build for
the running QGIS, under the same provider key, `geopackage_rs`. On QGIS 3,
or with no matching build, the Python provider serves that key instead, so a
project opens with whichever of the two the machine has.

## Why it's shaped like this

QGIS loads providers as C++ subclasses of `QgsVectorDataProvider`,
`QgsAbstractFeatureSource` and `QgsAbstractFeatureIterator`, and Rust can't
subclass a C++ class. So:

- **Rust does the work** (`src/lib.rs`): opening files, planning reads, and
  streaming Arrow batches from a reader thread with its own pooled read-only
  connection. Filters and projections are applied by the crate.
- **A C++ shim** (`src/provider.cpp`) inherits QGIS's classes and forwards
  to Rust through `cxx`. Its one piece of real work is the per-feature loop,
  which builds each `QgsFeature` straight from the Arrow arrays. OGR's
  provider runs the same loop.
- **The plugin loads the library** with `ctypes`
  (`gpkg_toolkit/native_provider.py`), checks
  `geopackage_qgis_qgis_version()` against the running QGIS, and calls
  `geopackage_qgis_register()`, which registers it as `geopackage_rs`.
  `geopackage_qgis_register_side_by_side()` registers it as
  `geopackage_rs_native` instead, beside the Python provider and OGR: the
  plugin's tests run against both providers that way.

**Editing** goes through one read-write connection, opened on the first
edit. Each of QGIS's edit calls (add, delete, change attributes, change
geometries) is one transaction, so a failed call leaves the file as it was.
Geometries are made multi, and given or stripped of z and m, to match the
column, as the Python provider does. Reads keep their own read-only
connections and see each commit from their next read on.

## Building

`cargo build --release` compiles both halves into one `libgeopackage_qgis.so`
against QGIS 4 and Qt 6. No Ubuntu release packages QGIS 4 yet; conda-forge
does, and CI builds from there (the `native-provider` job):

    conda create -n qgis -c conda-forge "qgis=4.2" pkg-config
    conda activate qgis
    cargo build --release

The build takes QGIS from `QGIS_PREFIX`, else the active conda environment,
else `/usr`, and Qt from `pkg-config`. For a Qt unpacked rather than
installed, set `QT_INCLUDE_DIR` and `QT_LIB_DIR`. The crate isn't a workspace
member, because the workspace's other CI jobs don't install QGIS.

A build works only in the QGIS minor version it was compiled against:
QGIS's C++ interface changes between minor versions, and the plugin checks.
On Linux it is tied tighter still, to the patch release, because
`libqgis_core`'s soname carries the full version (`libqgis_core.so.4.2.3`).
The release builds will have to settle that, by building per patch release
or by resolving QGIS's symbols from the running process.

## Results

200,000 random points in EPSG:4326, headless QGIS 3.34, measured before the
provider moved to QGIS 4, built against the crate with #207's bounding-box
fix. With #208 as well, a box 1.1 times the layer's extent, which is what a
zoomed-out canvas asks for, took 129 ms against OGR's 231 ms. Each figure is the best of several runs,
draining the iterator with `nextFeature` into one reused `QgsFeature`:

| Request | Features | OGR | Native | Native/OGR |
|---|---:|---:|---:|---:|
| Full scan | 200,000 | 282 ms | 141 ms | 0.50 |
| Bbox, 0.01% of area | 19 | 0.24 ms | 0.44 ms | 1.9 |
| Bbox, 1% | 1,989 | 6.1 ms | 4.9 ms | 0.80 |
| Bbox, 25% | 49,949 | 144 ms | 73 ms | 0.51 |
| Bbox, 81% | 161,982 | 485 ms | 155 ms | 0.32 |
| Whole extent, no attributes (what a render asks for) | 200,000 | 218 ms | 132 ms | 0.60 |
| No geometry, one attribute | 200,000 | 184 ms | 97 ms | 0.53 |
| One feature by id | 1 | 0.05 ms | 0.08 ms | 1.5 |
| 50 features by id | 50 | 0.27 ms | 0.18 ms | 0.66 |
| `QgsMapRendererParallelJob`, whole layer, 1024 px | 200,000 | 419 ms | 252 ms | 0.60 |
| The same, zoomed to 1% of the area | 1,989 | 13 ms | 9 ms | 0.69 |

For comparison, the Python provider takes 1,333 ms for the full scan.

**Correctness.** These checks all produce identical results to OGR:

- every feature, value and geometry of the four `gdal_multilayer_1_4`
  layers;
- 160 bounding boxes, from tiny to wider than the layer, with and without a
  subset string, including on a layer with NULL and empty geometries and a
  deliberately stale `gpkg_contents` extent;
- the rendered pixels.

Eight threads running 240 concurrent bbox reads on one provider finished
with no errors.

**What's left:** a small read pays a fixed cost of roughly 0.03 to 0.2 ms.
That's the reader thread, resolving the layer and preparing the statement,
against OGR's open, prepared connection. It's invisible in a render. Most
of it can be removed by resolving the layer once and reading small requests
on the calling thread.

## Found in the crate

**The Arrow bounding-box read degraded on scattered data.** It issued one
query per run of consecutive candidate keys, which on scattered data is
nearly one per row: 1,974 queries for 1,989 rows. The spike worked around
it. #207 fixed it in the crate, reading a page of candidates per query, and
the workaround is gone: the provider calls `read_arrow_in` directly.

**A box containing the whole layer took the filtered loop.** That was
125 ms on the `filtered` benchmark's diagonal, against 41 ms for an
unfiltered read. The spike treated such a box as a full scan, as OGR's
driver does. #208 moved that into the crate's Arrow read, which now
decides from the RTree's root node (widened by one `f32` step, because
SQLite stores a subnormal coordinate inward of its true value) and from
whether every row is indexed. The spike's own check missed that widening
and could return a row outside the box. It is gone: the provider calls the
crate's reads unchanged.

## Next

CI and release builds for Linux, macOS and Windows against QGIS 4, shipped
in the plugin as `ext-libs/<platform>/qgis-<major>.<minor>/`. conda-forge
has QGIS 4 for all three, which makes it the likely SDK on every runner. The
soname question above has to be settled first.
