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

200,000 random points in EPSG:4326, headless QGIS 3.34. Each figure is the
best of several runs, draining the iterator with `nextFeature` into one
reused `QgsFeature`:

| Request | Features | OGR | Native | Native/OGR |
|---|---:|---:|---:|---:|
| Full scan | 200,000 | 279 ms | 144 ms | 0.52 |
| Bbox, 0.01% of area | 19 | 0.18 ms | 0.37 ms | 2.0 |
| Bbox, 1% | 1,989 | 9.3 ms | 6.8 ms | 0.73 |
| Bbox, 25% | 49,949 | 164 ms | 82 ms | 0.50 |
| Bbox, 81% | 161,982 | 503 ms | 204 ms | 0.41 |
| Whole extent, no attributes (what a render asks for) | 200,000 | 219 ms | 137 ms | 0.63 |
| No geometry, one attribute | 200,000 | 180 ms | 100 ms | 0.55 |
| One feature by id | 1 | 0.05 ms | 0.09 ms | 1.8 |
| 50 features by id | 50 | 0.38 ms | 0.31 ms | 0.82 |
| `QgsMapRendererParallelJob`, whole layer, 1024 px | 200,000 | 350 ms | 260 ms | 0.74 |
| The same, zoomed to 1% of the area | 1,989 | 13 ms | 13 ms | 1.0 |

For comparison, the Python provider takes 1,333 ms for the full scan.

**Correctness.** These checks all produce identical results to OGR:

- every feature, value and geometry of the four `gdal_multilayer_1_4`
  layers;
- 160 bounding boxes, from tiny to wider than the layer, with and without a
  subset string, including on a layer with NULL and empty geometries and a
  deliberately stale `gpkg_contents` extent;
- 50 id lookups;
- the rendered pixels.

Eight threads running 240 concurrent bbox reads on one provider finished
with no errors.

**What's left:** a small read pays a fixed cost of roughly 0.05 to 0.2 ms.
That's the reader thread, resolving the layer and preparing the statement,
against OGR's open, prepared connection. It's invisible in a render. Most
of it can be removed by resolving the layer once and reading small requests
on the calling thread.

## Found in the crate

**The Arrow bounding-box read degrades on scattered data.** `read_arrow_in`
sorts the RTree candidates into runs of consecutive keys and issues one
query per run. Spatially scattered rows, the normal case, make nearly every
candidate its own run. A 1% box issued 1,974 queries for 1,989 rows and took
20 ms, where the row path takes 4 ms.

An `IN (SELECT id FROM rtree ...)` clause avoids that, but SQLite
re-evaluates it on every page. That makes the cost pages × candidates, and a
whole-layer box was then 3 times slower than OGR.

The spike reads the RTree once, sorts the candidates, and gives each page an
explicit chunk of ids through `json_each`, so the total cost is proportional
to the number of candidates. A box containing the RTree's own bounds becomes
a full scan, which can use the parallel reader. OGR's driver makes the same
shortcut. The spike uses the RTree's bounds rather than the recorded extent,
because the recorded extent can be stale.

Both belong in the crate's Arrow read, where every Arrow consumer benefits.
They are here for now only to prove the design.

## Next

1. Move the bounding-box read into the crate, with a regression benchmark on
   scattered data.
2. Complete the provider: editing, the remaining capabilities, and a QGIS
   3.40+ and QGIS 4 (Qt 6) build. In the plugin, use the native provider
   when a build matches the running QGIS, and fall back to the Python one
   otherwise. Run the plugin's provider tests against both.
3. CI and release builds for Linux, macOS and Windows, against the current
   LTR and QGIS 4. Getting the QGIS SDK onto macOS runners is the open
   question.
