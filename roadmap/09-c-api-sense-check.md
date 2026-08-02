# The C API sense-check

Dated 2026-08-02. This is the output of the phase 10 sense-check item in
[07-m5-extensions-and-1.0.md](07-m5-extensions-and-1.0.md): the C surface
compared outward, against GDAL's C API and against what QGIS would need,
rather than inward against the Rust API it mirrors.

Method. The inventory below covers all 62 functions in
`geopackage-ffi/include/geopackage.h` as of the `ffi-tiles-enumeration`
branch. The GDAL side is the C API a GeoPackage consumer programs against as
of GDAL 3.12, vector and raster; the QGIS side is the
`QgsVectorDataProvider` and `QgsFeatureRequest` surface its GDAL provider
implements. Each row classifies as one of:

- **equivalent**: this ABI answers the same need, even where the shape
  differs;
- **omission**: deliberately absent, with the reasoning stated here so it is
  a decision rather than an accident;
- **gap**: absent without a recorded reason, numbered as a finding and
  settled in the decision section at the end.

## The vector surface against GDAL

| GDAL call | What a consumer uses it for | This ABI | Classification |
|---|---|---|---|
| `GDALOpenEx` | open, with update/read-only flags and open options | `gpkg_open`, `gpkg_open_read_only`, `gpkg_create` | equivalent; open options are thinner, see F7 |
| `GDALClose` | close, flushing | `gpkg_close`, refusing while children are live | equivalent |
| `GDALDatasetGetLayerCount` / `GetLayerByName` | layer discovery | `gpkg_layer_names_count`, `gpkg_layer_name_at`, `gpkg_layer_open`, `gpkg_attributes_open` | equivalent |
| `GDALDatasetCreateLayer` + `OGR_L_CreateField` | create a layer, then its fields | `gpkg_create_layer_from_arrow_schema` | equivalent; one call taking a schema rather than a call per field |
| `GDALDatasetDeleteLayer` | remove a layer | none, and the Rust crate has no equivalent either | gap, F6 |
| `GDALDatasetTestCapability` / `OGR_L_TestCapability` | probe what is supported and what is fast | none | omission: the capability set is fixed per ABI version, which `gpkg_version` names; the per-layer fast-path question is answered by `gpkg_layer_has_spatial_index` and `gpkg_layer_spatial_index_status` |
| `GDALDatasetStartTransaction` / `Commit` / `Rollback` | caller-scoped transactions | `gpkg_begin`, `gpkg_commit`, `gpkg_rollback`, `gpkg_in_transaction` | equivalent |
| `GDALDatasetExecuteSQL` | arbitrary SQL, both dialects | none | omission: the file is SQLite, so a C consumer with a query this surface cannot express already links `sqlite3` and can open the file with it directly; a typed surface should not also be a SQL shim |
| `OGR_L_GetName` / `GetGeomType` / `GetFIDColumn` | layer identity | `gpkg_layer_name`, `gpkg_layer_geometry_type`, `gpkg_layer_column_is_primary_key` | equivalent |
| `OGR_L_GetLayerDefn` + `OGR_FD_*` / `OGR_Fld_*` | field introspection | `gpkg_layer_column_count` / `_name` / `_type` / `_is_primary_key` | equivalent |
| `OGR_L_GetSpatialRef` | the layer's CRS, as a definition | `gpkg_layer_srs_id` only: the id, not the definition | gap, F3 |
| `OGR_L_GetExtent` / `GetFeatureCount` | extent and count | `gpkg_layer_extent`, `gpkg_layer_count` | equivalent |
| `OGR_L_ResetReading` / `GetNextFeature` + `OGR_F_*` | row-at-a-time reads | none; Arrow batches are the read plane | omission: decided in phase 9, and this review does not disturb it; a batch iterator serves the same loop |
| `OGR_L_GetArrowStream` | columnar reads | `gpkg_layer_read_arrow`, `gpkg_layer_read_arrow_in` | equivalent, and ours is the primary path rather than the alternative one |
| `OGR_L_SetSpatialFilter` | bounding-box filter | `gpkg_layer_read_arrow_in` | equivalent |
| `OGR_L_SetAttributeFilter` | attribute-filtered reads | none; Rust `Layer::select` exists on the row path only | gap, F1 |
| `OGR_L_GetFeature` (by FID) | random access | none, in either language | gap, folded into F1: `fid = ?` through the filtered read serves it |
| `OGR_L_SetIgnoredFields` | column projection | none; Rust `Layer::with_columns` and `without_geometry` exist and stop at the language boundary | gap, F2 |
| `OGR_L_CreateFeature` / `SetFeature` / `UpsertFeature` / `DeleteFeature` | row writes | `gpkg_writer_insert` / `_update` / `_update_column` / `_delete` | equivalent |
| `OGR_L_WriteArrowBatch` | columnar writes | `gpkg_layer_write_arrow` | equivalent |
| `OGR_L_CreateField` / `DeleteField` / `AlterFieldDefn` on a live layer | schema evolution | none, and the Rust crate has no equivalent either | gap, F5 |
| `OGR_L_SyncToDisk` | durability point | `gpkg_writer_commit`, `gpkg_commit` | equivalent |
| `GDALDatasetGetFieldDomain` (3.3+) | read `gpkg_schema` constraints | none; Rust reads and enforces them | omission for now: demand-driven, see F8 |
| `GDALDatasetGetRelationshipNames` (3.6+) | read Related Tables | none; Rust reads and writes them | omission for now: demand-driven, see F8 |
| GDAL metadata API over `gpkg_metadata` | read metadata | none; Rust reads and writes it | omission for now: demand-driven, see F8 |

Nothing in the last three rows blocks a reader: those tables are carried, not
required, and a consumer that needs them today reads them through SQLite
directly, as under the `ExecuteSQL` row.

One place this surface exceeds GDAL's is worth recording so it is not
accidentally lost: `gpkg_layer_repair_spatial_index` and the open-warning
enumeration have no OGR equivalent, and `GeoPackage::validate` has no GDAL
equivalent at all, though it also has no C representation yet, see F4.

## The tile surface against GDAL's raster model

GDAL models a pyramid as a raster dataset: bands, blocks, overviews, pixels.
That model decodes, and decoding is what this crate refuses to do (M4: no
image codec, payloads stay opaque). The comparison is therefore about which
needs survive the model difference, not about matching calls.

| GDAL | What a consumer uses it for | This ABI | Classification |
|---|---|---|---|
| `SUBDATASETS` metadata | list the pyramids in a file | `gpkg_tiles_names_count`, `gpkg_tiles_name_at` | equivalent, as of 2026-08-02 |
| dataset geotransform and size | georeference the grid | `gpkg_tiles_extent`, `gpkg_tiles_matrix_at` | equivalent |
| `GDALGetOverviewCount` / `GetOverview` | pick a zoom level | `gpkg_tiles_zoom_level_count`, `gpkg_tiles_matrix_at` | equivalent |
| `GDALRasterIO` | pixels | `gpkg_tiles_get` / `_get_into`: stored bytes, never pixels | omission: by design; a renderer decodes on its side of the boundary, and every consumer with an opinion about image formats already owns a decoder |
| `GDALCreateCopy` to write a pyramid | write tiles | `gpkg_tiles_put` / `_delete`, checked against the grid and the declared format | equivalent |
| walking what a sparse pyramid actually stores | copy or audit a pyramid | none: C probes the declared grid with `gpkg_tiles_has`, which is O(grid) where Rust's `TileCursor` is O(stored) | gap, F9 |

## What QGIS would need

The question the handle-lifetime benchmarks were already asked in phase 9:
could QGIS sit on this ABI in place of its GDAL provider. Its provider
interface, reduced to what it calls on the way to a rendered, editable
layer:

| Provider need | This ABI | Classification |
|---|---|---|
| `fields()`, `wkbType()`, `crs()`, `extent()`, `featureCount()`, primary key | schema calls, `gpkg_layer_srs_id`, `gpkg_layer_extent`, `gpkg_layer_count` | equivalent, except the CRS definition itself, F3 |
| `capabilities()` | none | omission, as under `TestCapability` above |
| `getFeatures(QgsFeatureRequest)` with `setFilterRect` | `gpkg_layer_read_arrow_in`, at interactive rates, which is what the phase 9 measurements sized | equivalent |
| `setSubsetString`, `setFilterExpression` | none | gap, F1, the same one |
| `setFilterFid`, `SelectAtId` | none | gap, folded into F1 |
| `setSubsetOfAttributes`, `NoGeometry` | none | gap, F2, the same one |
| `addFeatures`, `changeAttributeValues`, `changeGeometryValues`, `deleteFeatures` | the writer family, including `gpkg_writer_update_column` and WKB geometry updates | equivalent |
| `addAttributes` / `deleteAttributes` / `renameAttributes` | none anywhere in the workspace | gap, F5 |
| `uniqueValues`, `minimumValue`, `maximumValue`, `aggregate` | none; expressible through F1's filter only in part | omission for now: QGIS computes these client-side when a provider declines, and a filtered columnar read hands the column over cheaply; revisit only if a consumer measures it as a bottleneck |
| `QgsTransaction` | the transaction quartet | equivalent |
| rendering rasters | stored tiles plus a decoder on the QGIS side | omission, as under `GDALRasterIO` above |

## Findings

**F1: no attribute-filtered read crosses the C boundary.** The largest gap,
and it appears three times above: OGR's `SetAttributeFilter`, QGIS's subset
strings, and by-FID access are all the same missing entry point. Rust has
`Layer::select(where, params)` on the row path, but the C data plane is
Arrow, and there is no Arrow read taking a filter. This is phase 8b's shape
exactly: library work first (an Arrow read carrying a WHERE clause and
parameters, declining to the direct loop as the bbox read does), then one C
entry point. By-FID access is `fid = ?1` through the same call, which is why
it is not its own finding.

**F2: no column projection crosses the C boundary.** Rust callers project
with `Layer::with_columns` and `without_geometry` before reading; a C caller
always gets every column. Projection shipped in 0.5.0 because it measured as
the difference on geometry-heavy layers, and the C consumer is reading
through the same engine.

*Corrected while fixing it: the claim this finding first made, that the
library work already exists, was true of the row path only. The Arrow path,
which is the C data plane, ignored the projection entirely, so the C entry
point needed library work after all: `arrow_schema()` and the Arrow reads
now narrow to the projection, with the bbox re-test fed by a hidden column
when the projection excludes the geometry.*

**F3: the CRS stops at a number.** `gpkg_layer_srs_id` hands over an integer;
Rust callers get `Srs` with both definitions and the epoch. A C consumer
feeding a projection library needs the definition text. One entry point,
reading what `srs()` reads.

**F4: the fail-fast surface is Rust-only.** The extensions catalogue exists
precisely so a client can "fail fast" (clause 2.3.2), and this crate refuses
writes to tables under unknown extensions. A C caller meets that refusal as
an error mid-write rather than as a question it could have asked at open,
because the catalogue and `GeoPackage::validate` have no C representation.
Enumeration of extension rows with their support level, and validation
findings with severity and repair advice, are both straightforward to
express in the existing string-and-status idiom.

**F5: schema evolution is absent from the workspace.** Adding, dropping or
renaming a column on a live layer does not exist in Rust, so it cannot exist
in C. GDAL has it, QGIS's editing model leans on it, and in SQLite it is
`ALTER TABLE` plus bookkeeping across `gpkg_geometry_columns`,
`gpkg_data_columns` and the RTree triggers. It is also purely additive API.

**F6: layer deletion is absent from the workspace.** `DeleteLayer` in GDAL;
drop the table, deregister `gpkg_contents`, `gpkg_geometry_columns`,
`gpkg_extensions`, the RTree tables and any metadata references here.
Smaller than F5, same additive character.

**F7: open options are thinner over C.** No WAL opt-in, no
`enforce_column_constraints`, no `allow_unsupported_extension_writes`, no
lenient writable open. Each is one flag on an options struct the ABI does
not yet have.

**F8: the extension tables GDAL surfaces (field domains, relationships,
metadata) are Rust-only.** No consumer has asked, reading them from C is
already possible through SQLite, and F4 covers the one case where absence
has a correctness cost. Demand-driven.

**F9: a sparse pyramid cannot be walked economically from C.** Probing the
grid with `gpkg_tiles_has` visits every address; Rust's `TileCursor` visits
every stored tile. A pyramid cursor needs a handle type, for which the Arrow
stream is the precedent.

## Decision

Recorded here, reflected as items in
[07-m5-extensions-and-1.0.md](07-m5-extensions-and-1.0.md).

**Nothing found blocks the freeze.** Every finding is additive: new entry
points, new option structs, or new Rust API beside existing API. No existing
signature or type needs to change shape, so the API review can proceed
regardless of when the items below land. That is the sense-check's headline,
and it was not a foregone conclusion.

**Taken up as items, in rough order of consumer value:**

1. F1, the filtered Arrow read: Rust first, then `gpkg_layer_read_arrow_where`
   or equivalent. The one finding with a Rust prerequisite.
2. F2, projection over C.
3. F3, the SRS definition over C.
4. F4, extensions and validation over C: the fail-fast pair.
5. F9, a pyramid cursor over C.

**Recorded as deliberate omissions, unchanged until a consumer demonstrates
the need:** capability probing, `ExecuteSQL`, row-at-a-time reads, decoded
pixels, aggregates (all reasoned inline above), plus F7 and F8.

**Deferred past 1.0 as additive work:** F5 (schema evolution) and F6 (layer
deletion). Both are wanted by the GDAL comparison, neither is wanted by any
consumer this project actually has yet, and both can land in any minor
release without touching the freeze. F6 is the cheaper and likelier first.
