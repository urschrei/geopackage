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

`cargo build --release` compiles both halves into one library
(`libgeopackage_qgis.so`, `.dylib`, or `geopackage_qgis.dll`) against QGIS 4
and Qt 6. The crate isn't a workspace member, because the workspace's other
CI jobs don't install QGIS.

**What a build needs.** On Linux and macOS, only QGIS's and Qt's headers:
the library links neither and takes their symbols from the QGIS it loads
into (see `build.rs`). So one build serves every patch release of its QGIS
minor version, and any Qt at least as new as the headers it was built
with. Windows has no such late binding, so it also needs QGIS's and Qt's
import libraries. A build works only in the QGIS minor version it was
compiled against: QGIS's C++ interface changes between minor versions, and
the plugin checks.

`build.rs` takes QGIS's headers from `QGIS_INCLUDE_DIR`, else
`$QGIS_PREFIX/include/qgis`, where `QGIS_PREFIX` defaults to the active conda
environment and then `/usr`. It takes Qt from `QT_DIR`, a Qt prefix in any of
the usual layouts (including a folder of macOS frameworks), else
`pkg-config`. Qt's headers should be no newer than the Qt the library will
load into; QGIS 4.0 requires Qt 6.6, so 6.6's headers suit any QGIS 4.

**With conda**, as CI builds and tests it (the `native-provider` job):

    conda create -n qgis -c conda-forge "qgis=4.2" pytest
    conda activate qgis
    QT_DIR=$CONDA_PREFIX cargo build --release

**On macOS, against QGIS.app** (for example QGIS 4.0.2). Official app
bundles have shipped without headers, so make them from QGIS's source at the
same release, with `scripts/qgis_headers.py`. A comparison against
conda-forge's installed QGIS 4.2.3 headers preprocesses the shim to
identical code.

    git clone --depth 1 --branch final-4_0_2 --filter=blob:none --sparse \
        https://github.com/qgis/QGIS ../target/qgis-4.0.2
    git -C ../target/qgis-4.0.2 sparse-checkout set src/core cmake_templates external/nlohmann
    python3 scripts/qgis_headers.py ../target/qgis-4.0.2 ../target/qgis-4.0.2-include

Then Qt 6 headers no newer than the bundle's Qt. QGIS 4.0.2's bundle ships
Qt as plain libraries, whose names give the version
(`libQt6Core.6.9.3.dylib`):

    ls /Applications/QGIS*.app/Contents/Frameworks | grep libQt6Core

A bundle that ships Qt as frameworks gives it in
`QtCore.framework/Resources/Info.plist` instead.

Any Qt 6 at or below that version will do. `aqtinstall` downloads Qt's
own macOS build, and needs only `uv`; Qt's base module holds every header
the build uses:

    uvx --from aqtinstall aqt install-qt mac desktop 6.6.3 clang_64 \
        --outputdir ../target/qt --archives qtbase
    QGIS_INCLUDE_DIR=$PWD/../target/qgis-4.0.2-include \
        QT_DIR=$PWD/../target/qt/6.6.3/macos cargo build --release

conda-forge's Qt also works, with `QT_DIR=$CONDA_PREFIX`:

    conda create -n qt -c conda-forge "qt6-main=6.6"

If the bundle's frameworks do carry `Headers`, point at the bundle instead:
`QT_DIR=/Applications/QGIS-4.0.app/Contents/Frameworks`, and likewise
`QGIS_INCLUDE_DIR=.../qgis_core.framework/Headers`.

To use the build, the plugin must be installed in QGIS 4's profile. For
development, link the repository's copy and enable "GeoPackage toolkit" in
the Plugin Manager:

    ln -s $PWD/../qgis-plugin/gpkg_toolkit \
        ~/Library/Application\ Support/QGIS/QGIS4/profiles/default/python/plugins/gpkg_toolkit

Then either start QGIS from a terminal with `GPKG_TOOLKIT_NATIVE_PROVIDER`
set to the build, or copy the build into the plugin, as
`gpkg_toolkit/ext-libs/macos-universal2/qgis-4.0/libgeopackage_qgis.dylib`.
The executable in `Contents/MacOS` takes the bundle's name, which
`CFBundleExecutable` in `Contents/Info.plist` gives. For QGIS 4.0.2:

    GPKG_TOOLKIT_NATIVE_PROVIDER=$PWD/target/release/libgeopackage_qgis.dylib \
        /Applications/QGIS-final-4_0_2.app/Contents/MacOS/QGIS-final-4_0_2

QGIS's message log says when the plugin falls back to the Python provider,
and why.

The provider does not need the plugin's Python module, `geopackage_rs`, but
the plugin's menu actions and Processing algorithms do. To build it into the
plugin from `geopackage-py`:

    uvx maturin build --release --out ../target/wheels
    mkdir -p ../qgis-plugin/gpkg_toolkit/ext-libs/macos-universal2
    unzip -o ../target/wheels/geopackage_rs-*.whl 'geopackage_rs/*' \
        -d ../qgis-plugin/gpkg_toolkit/ext-libs/macos-universal2

**Release builds** (`.github/workflows/qgis-plugin.yml`) make the same header
kit from each supported release's tag and use Qt 6.6's headers, for Linux
(x86_64 and aarch64, in a manylinux_2_28 container, so glibc 2.28 or later),
macOS (universal2) and Windows (x86_64; QGIS 4.2 only, since its import
library comes from conda-forge, which has no QGIS 4.0).

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

A Windows build for QGIS 4.0, which needs QGIS 4.0's `qgis_core.lib` (from
OSGeo4W's development package), and builds for each new QGIS minor version
as it is released: add it to the `native` job's matrix.
