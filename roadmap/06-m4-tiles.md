# M4: tiles → v0.3

Goal: full tiles requirement class. Bytes in/bytes out, with no image
decoding/encoding in core (PNG/JPEG/WebP payloads are opaque BLOBs; format
sniffing only, via magic bytes).

## Tasks

- [ ] Core DDL: `gpkg_tile_matrix_set`, `gpkg_tile_matrix`, user tile table
      template (`zoom_level, tile_column, tile_row, tile_data`, unique
      constraint), verbatim from Annex C into `geopackage-core::ddl`.
- [ ] `TileMatrixSet` model: single envelope + srs per pyramid;
      `TileMatrix` per zoom (matrix w/h, tile w/h, pixel sizes); validation
      of the spec's consistency rules (contents bbox vs matrix set, zoom
      monotonicity).
- [ ] `create_tile_pyramid(name, srs, bbox, matrices)`; helper for
      "zoom-times-two" ladders (the spec default) and for the common web
      mercator quad (EPSG:3857) without hardcoding it as the only option.
- [ ] Tile CRUD: `get_tile(z, col, row)`, `put_tile`, `delete_tile`,
      streaming iteration in matrix order; batch writer reusing M2
      transaction machinery.
- [ ] `gpkg_zoom_other` extension (non-power-of-two intervals): read always;
      write behind explicit opt-in that registers the extension row.
- [ ] `gpkg_webp` extension registration when WebP payload sniffed on write.
- [ ] Tile coordinate sanity: gpkg tile origin is top-left of the matrix;
      document and test conversions vs XYZ/TMS conventions (classic
      off-by-one-flip bug source).
- [ ] CLI: `gpkg tiles info`, `gpkg tiles get z/x/y --out tile.png`.
- [ ] Corpus: GDAL-generated raster gpkg, NGA sample tile files; round-trip
      vs `gdal_translate`.

## Acceptance criteria

1. ets-gpkg12 tiles conformance classes pass on files we create.
2. A pyramid written here renders correctly in QGIS (visual check scripted
   via QGIS headless render hash).
3. Tile read throughput benchmarked (tiles/sec sequential and random) vs
   GDAL.
4. Tag **v0.3.0**.

## Explicit non-goals here

Tiled gridded coverage (elevation): the extension is "under revision"
upstream; tracked in [07-m5-extensions-and-1.0.md](07-m5-extensions-and-1.0.md).
Vector tiles community extension: out of scope entirely for now.
