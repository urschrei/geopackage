# geopackage-qgis

A native QGIS vector data provider over the `geopackage` crate. **This is a
spike:** read-only, Linux, QGIS 3.34. It exists to answer one question:
can a provider built on the crate match OGR's speed in QGIS? The Python
provider in `qgis-plugin` can't, as its README explains. It does: on every
request with throughput behind it, the spike is faster than OGR.

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
- **The plugin loads the library** with `ctypes` and calls
  `geopackage_qgis_register()`. That registers the provider as
  `geopackage_rs_native` beside OGR and the Python provider, so all three can
  be compared on the same file.

`cargo build --release` compiles both halves into one `libgeopackage_qgis.so`.
It needs QGIS's and Qt's development headers (`libqgis-dev`,
`qtbase5-dev`). `QGIS_PREFIX` points the build at a QGIS installed elsewhere.
The crate isn't a workspace member, because the workspace's CI legs don't
install QGIS.

## Results

200,000 random points in EPSG:4326, headless QGIS 3.34, built against the
crate with #207's bounding-box fix. Each figure is the best of several runs,
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

**A box containing the whole layer still takes the filtered loop** in the
crate. That's 125 ms on the `filtered` benchmark's diagonal, against 41 ms
for an unfiltered read. The provider treats such a box as a full scan, as
OGR's driver does. It decides this from the RTree's own bounds, because the
extent recorded in `gpkg_contents` can be stale. That shortcut is still for
the crate to take over.

## Next

1. Move the whole-layer-box shortcut into the crate's Arrow read.
2. Complete the provider: editing, the remaining capabilities, and a QGIS
   3.40+ and QGIS 4 (Qt 6) build. In the plugin, use the native provider
   when a build matches the running QGIS, and fall back to the Python one
   otherwise. Run the plugin's provider tests against both.
3. CI and release builds for Linux, macOS and Windows, against the current
   LTR and QGIS 4. Getting the QGIS SDK onto macOS runners is the open
   question.
