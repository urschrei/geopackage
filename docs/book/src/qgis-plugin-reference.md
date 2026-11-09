# QGIS plugin reference

The GeoPackage Toolkit plugin for QGIS: its data provider and its
Processing algorithms. To install it, see
[How to install the QGIS plugin and add a layer through it](how-to/qgis-plugin.md).

## Requirements

| | |
|---|---|
| QGIS | 3.28 or later |
| Plugin repository | `https://urschrei.github.io/geopackage/qgis/plugins.xml` |
| Release zips | `gpkg_toolkit-<version>.zip`, attached to each [GitHub release](https://github.com/urschrei/geopackage/releases) |
| Status | Experimental |

The zip holds compiled code for each platform it supports. A file is chosen
when the plugin loads.

| Platform | `geopackage_rs` module | Native provider |
|---|---|---|
| Linux x86_64 (glibc 2.28 or later) | yes | QGIS 4.0, 4.2 |
| Linux aarch64 (glibc 2.28 or later) | yes | QGIS 4.0, 4.2 |
| macOS (arm64 and x86_64) | yes | QGIS 4.0, 4.2 |
| Windows x86_64 | yes | QGIS 4.0, 4.2 |

On Linux and macOS, both use the SQLite that QGIS has loaded. On Windows,
each has its own.

## Data provider

| | |
|---|---|
| Provider key | `geopackage_rs` |
| URI | `/path/to/file.gpkg\|layername=<table>`, as OGR's. Without `layername`, the file's only layer or attribute table. |
| Layers | Feature layers and attribute tables |
| Opened | Leniently: `DATETIME` text in the common ISO 8601 forms other than GeoPackage's own is read |

### Implementations

The key is served by one of two implementations. They give the same
results.

| Implementation | Serves the key when |
|---|---|
| Native (Rust behind a C++ shim, `geopackage-qgis`) | QGIS 4, and the plugin has a build for the running minor version |
| Python (`gpkg_toolkit/data_provider.py`) | Otherwise, including every QGIS 3 |

The *GeoPackage Toolkit* tab of the *Log Messages* panel names the
implementation in use, and why the native one isn't used.

| Environment variable | Effect |
|---|---|
| `GPKG_TOOLKIT_NATIVE_PROVIDER=<path>` | Loads the native build at `<path>` instead of the one in the plugin |
| `GPKG_TOOLKIT_NATIVE_PROVIDER=0` | Uses the Python implementation |

### Reads

| QGIS request | How the provider serves it |
|---|---|
| Bounding box | The layer's RTree, then QGIS's exact-intersection test when asked |
| One feature id, or several | `fid = <id>`, or `fid IN (...)` |
| Subset string | An SQLite `WHERE` clause, as in OGR. An invalid one is rejected when it is set |
| Attributes without geometry | A column projection: unread columns and geometry are not fetched |
| Filter expression | Evaluated by QGIS on the rows read |
| Destination CRS, limit, order by, distance within | As QGIS's core providers serve them |

### Edits

| Capability | Supported |
|---|---|
| Add, delete features | Yes |
| Change attribute values, geometries | Yes |
| Add, delete, rename fields | No |

Each edit call is one transaction. A geometry is converted to the column's
type, as multi, and given or stripped of Z and M, on the way in. A commit
writes through the library, which keeps the spatial index and the extent
up to date.

## Algorithms

Each algorithm's identifier is `gpkgtoolkit:<name>`. An algorithm that
opens a GeoPackage opens it leniently.

### Validate GeoPackage (`validate`)

Group: Inspect and repair. Read-only.

| Parameter | Type | Description |
|---|---|---|
| `INPUT` | File (`.gpkg`) | The GeoPackage |
| `OUTPUT` | Table | Findings |

Fields of `OUTPUT`: `severity` (`error`, `warning` or `advisory`), `code`,
`table_name`, `message`, `repair`, `plugin_algorithm`, the name of the
algorithm here that makes the repair.

Outputs: `ERRORS`, `WARNINGS`, `ADVISORIES`, the number of findings of each
severity. The findings are this library's checks, not the OGC test suite's.

### Repair spatial indexes (`repairspatialindexes`)

Group: Inspect and repair. Modifies the file.

| Parameter | Type | Description |
|---|---|---|
| `INPUT` | File (`.gpkg`) | The GeoPackage |
| `LAYER` | String, optional | One layer; every feature layer if empty |
| `BUILD_MISSING` | Boolean, default false | Build an index for a layer that has none |
| `OUTPUT` | Table | Index report |

Fields of `OUTPUT`: `layer`, `status_before` and `status_after` (`current`,
`legacy`, `stale` or `absent`), `action`, `missing`, `extra`.

Output: `REPAIRED`, the number of layers changed.

A legacy (pre-1.4 or mixed) or stale trigger set is replaced, and an index
that is out of step with its table is rebuilt.

### Repair geometry and datetime encodings (`repairencodings`)

Group: Inspect and repair. Modifies the file.

| Parameter | Type | Description |
|---|---|---|
| `INPUT` | File (`.gpkg`) | The GeoPackage |
| `LAYER` | String, optional | One layer or attribute table; all if empty |
| `OUTPUT` | Table | Repair report |

Fields of `OUTPUT`: `layer`, `geometries` and `datetimes`, the number of
values rewritten.

Output: `REPAIRED`, the number of tables changed.

A geometry whose WKB gives Z or M by a high-bit flag in a type code is
rewritten with ISO type codes and a header envelope. `DATETIME` text in
another ISO 8601 form is rewritten in the GeoPackage form, unless that
would lose precision. See
[How to repair geometry and datetime encodings](how-to/repair-encodings.md).

### List extensions (`listextensions`)

Group: Inspect and repair. Read-only.

| Parameter | Type | Description |
|---|---|---|
| `INPUT` | File (`.gpkg`) | The GeoPackage |
| `OUTPUT` | Table | Extensions |

Fields of `OUTPUT`: `name`, `table_name`, `column_name`, `scope`, `support`,
`definition`. `support` is `implemented`, `known` (recognised, not
implemented; writes to a table it covers are refused when its scope is
`read-write`), `removed` (withdrawn from the standard) or `unrecognised`.

### Sample coverage at points (`samplecoverage`)

Group: Coverages. Read-only.

| Parameter | Type | Description |
|---|---|---|
| `INPUT` | File (`.gpkg`) | The GeoPackage |
| `COVERAGE` | String, optional | The coverage; the file's only one if empty |
| `POINTS` | Point layer | The points to sample, transformed into the coverage's CRS |
| `ZOOM` | Integer, optional | The zoom level; the most detailed if empty |
| `INTERPOLATION` | `nearest` (default) or `bilinear` | |
| `FIELD` | String, default `value` | The name of the new field |
| `OUTPUT` | Point layer | The points, with the new field |

A point outside the coverage, in a tile the coverage does not store, or on
a null sample gets NULL. A multipart or non-point geometry is sampled at
its centroid. On a grid-value-is-area coverage, `bilinear` gives the same
values as `nearest`.

### Coverage tile statistics (`coveragestatistics`)

Group: Coverages. Read-only.

| Parameter | Type | Description |
|---|---|---|
| `INPUT` | File (`.gpkg`) | The GeoPackage |
| `COVERAGE` | String, optional | The coverage; the file's only one if empty |
| `ZOOM` | Integer, optional | The zoom level; the most detailed if empty |
| `OUTPUT` | Polygon layer | One footprint per stored tile |

Fields of `OUTPUT`: `coverage`, `zoom`, `tile_column`, `tile_row`; the
statistics recorded in `gpkg_2d_gridded_tile_ancillary`, `rec_min`,
`rec_max`, `rec_mean`, `rec_std_dev`; the statistics calculated from the
decoded samples after scale and offset, without null samples, `min`, `max`,
`mean`, `std_dev`, `count`; and `error`, why a tile could not be decoded.

### Check coverage samples (Requirement 21) (`checkcoveragesamples`)

Group: Coverages. Read-only.

| Parameter | Type | Description |
|---|---|---|
| `INPUT` | File (`.gpkg`) | The GeoPackage |
| `COVERAGE` | String, optional | The coverage; every coverage if empty |
| `OUTPUT` | Polygon layer | One footprint per failing tile, in the coverage's CRS |

Fields of `OUTPUT`: `coverage`, `zoom`, `tile_column`, `tile_row`,
`message`.

Output: `FAILURES`, the number of failing tiles.

A tile fails when a sample is NaN or infinite (Requirement 21 of the tiled
gridded coverage extension), or when it cannot be decoded. *Validate
GeoPackage* does not make this check.

### Export layer to GeoPackage (`exportlayer`)

Group: Import and export.

| Parameter | Type | Description |
|---|---|---|
| `INPUT` | Vector layer | The layer to write |
| `OUTPUT` | File (`.gpkg`) | A new file, or an existing one to add a table to |
| `LAYER_NAME` | String, optional | The table's name; the input's name if empty |
| `SPATIAL_INDEX` | Boolean, default true | Build a spatial index |

Outputs: `OUTPUT_LAYER`, the written layer; `WRITTEN`, the number of rows.

The table is a GeoPackage 1.4 table with the 1.4 trigger set, and the index
is built in bulk. An integer field named `fid` gives the feature ids;
otherwise they are assigned. A field type with no GeoPackage equivalent is
written as `TEXT`. A single geometry in a multi layer is written as multi.
The CRS is added from the EPSG registry when it has an EPSG code, and from
its WKT otherwise.

## Menu

*Plugins → GeoPackage Toolkit*:

| Entry | Effect |
|---|---|
| Add GeoPackage layer (geopackage crate) | Asks for a file and a layer, and adds the layer through `geopackage_rs` |
| Validate GeoPackage, Repair spatial indexes, Repair geometry and datetime encodings, List extensions | Opens the algorithm's dialog |
