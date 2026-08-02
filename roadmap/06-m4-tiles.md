# M4: tiles → v0.6

Goal: full tiles requirement class. Bytes in/bytes out, with no image
decoding/encoding in core (PNG/JPEG/WebP payloads are opaque BLOBs; format and
header inspection only, never a decode).

The milestone was written as "v0.3"; 0.4 and 0.5 shipped other work first, so
this lands as **0.6.0**.

## Tasks

- [x] Core DDL: `gpkg_tile_matrix_set`, `gpkg_tile_matrix`, user tile table
      template (`zoom_level, tile_column, tile_row, tile_data`, unique
      constraint), verbatim from Annex C into `geopackage-core::ddl` and
      `geopackage-core::tiles`.
- [x] `TileMatrixSet` model: single envelope + srs per pyramid;
      `TileMatrix` per zoom (matrix w/h, tile w/h, pixel sizes); validation
      of the spec's consistency rules (Requirements 45 to 53, plus the
      well-ordered extent of 144 and duplicate zoom levels). Requirement 45 is
      stated as an exact equality and compared with a documented relative
      tolerance, since a pixel size is almost always derived by division.
- [x] `create_tile_pyramid(&TilePyramidBuilder)`; `TileMatrixSet::ladder` for
      "zoom-times-two" ladders (the spec default) and
      `TileMatrixSet::web_mercator_quad` for the common web mercator case,
      without hardcoding it as the only option (`ZoomLadder::base_grid` covers
      the 2x1 geographic convention).
- [x] Tile CRUD: `get_tile(coord)`, `get_tile_into(coord, &mut Vec<u8>)`,
      `put_tile`, `delete_tile`, streaming iteration in matrix order through a
      lending cursor; `TileWriter` and a batched `write_all` reusing the M2
      transaction machinery.
- [x] `gpkg_zoom_other` extension (non-power-of-two intervals): read always;
      write behind explicit opt-in that registers the extension row.
- [x] `gpkg_webp` extension registration when WebP payload sniffed on write.
- [x] Tile coordinate sanity: gpkg tile origin is top-left of the matrix;
      documented and tested against XYZ/TMS conventions (classic
      off-by-one-flip bug source). `TileMatrix::flip_row` for TMS;
      `TileMatrixSet::xyz_to_tile`/`tile_to_xyz` refuse rather than mis-address
      when the pyramid is not the standard quad, since GeoPackage indices are
      relative to the matrix set extent rather than to a global grid.
- [ ] CLI: `gpkg tiles info`, `gpkg tiles get z/x/y --out tile.png`.
      **Deferred**: no CLI crate exists yet (an unticked M3 item), and building
      one is its own piece of work rather than a tile task.
- [x] Corpus: GDAL-generated raster gpkg (committed fixture
      `geopackage/tests/fixtures/gdal_tiles.gpkg`, written by
      `scripts/generate_fixtures.py`), NGA and GDAL sample tile files in the
      fetched corpus, walked tile by tile by `corpus_external.rs`; round trip
      against `gdal_translate`/`gdalinfo` in `gdal_interop.rs`.

## Acceptance criteria

1. [x] ets-gpkg12 tiles conformance classes pass on files we create: over a
   pyramid this crate wrote (web mercator quad, zoom 0 to 3, 85 PNG tiles), the
   Tiles class is 24 passed, 0 failed, 0 skipped, and Core is 17 passed, 0
   failed. The classes for features, attributes and every extension are
   skipped, as they should be for a tiles-only file. The `Tiles Encoding WebP`
   class is among those skipped: the fixture contains no WebP payload, so the
   `gpkg_webp` registration path is covered by this crate's own tests rather
   than by the ETS.
2. [ ] A pyramid written here renders correctly in QGIS. **Deferred** to the
   scheduled QGIS interop job, issue #6. Not checked, by hand or otherwise, for
   this milestone: what has been verified is that GDAL reads a pyramid we
   wrote, and QGIS reads rasters through GDAL.
3. [x] Tile read throughput benchmarked (tiles/sec sequential and random),
   recorded in [benchmarks/2026-07-27-tiles.md](benchmarks/2026-07-27-tiles.md).
   GDAL's figure for the same file is recorded beside it and is **not** a
   like-for-like comparison: GDAL returns pixels because it decodes, and this
   crate returns stored bytes because it cannot. Allocation behaviour on these
   paths is not covered by a wall-clock bench and is tracked in issue #31.
4. [ ] Tag **v0.6.0**.

## Explicit non-goals here

Tiled gridded coverage (elevation): the extension is "under revision"
upstream; tracked in [07-m5-extensions-and-1.0.md](07-m5-extensions-and-1.0.md).
A TIFF payload is therefore rejected on write, with an error that names the
extension it belongs to. Vector tiles community extension: out of scope
entirely for now. NGA's tile scaling extension: out of scope.
