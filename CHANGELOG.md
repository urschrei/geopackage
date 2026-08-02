# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
While the version is below 1.0 the API may change in any release.

## [Unreleased]

### Added

- **`Layer::read_arrow_where` and `Layer::read_arrow_in_where`: filtered
  columnar reads.** The columnar counterparts of `select`, alone and composed
  with a bounding box, with `select`'s contract: the clause is raw SQL trusted
  from the caller, its placeholders are `?1` to `?N`, and parameters bind in
  slice order. The pagination and rtree bounds the read adds around the
  clause are numbered after `N`, so a clause written for `select` works
  unchanged. The clause runs inside SQLite, so the plain variant needs no
  client-side re-test; the composed variant keeps the bbox re-test and its
  page-advance accounting. Both decline the aggregate and threaded paths, as
  the bbox read does. Reading a single row is `fid = ?1`.

- **The Arrow reads honour a column projection.** `Layer::with_columns` and
  `without_geometry` narrowed only the row path; `arrow_schema()` and every
  Arrow read now narrow the same way: the primary key always, value columns
  as named, the geometry only when projected in. A bbox read on a handle
  whose projection excludes the geometry still re-tests each candidate
  exactly, through a hidden trailing column that reaches no batch. The
  threaded path declines on a projected layer, since its workers rebuild the
  layer by name and would read every column.

- **`geopackage-ffi`: the extensions catalogue.** `gpkg_extensions_count`
  and `gpkg_extension_at` walk the `gpkg_extensions` rows with the support
  level this library claims for each, which is what lets a C consumer fail
  fast instead of meeting an `UnsupportedExtension` refusal mid-write.

- **`geopackage-ffi`: pyramids can be created.** `gpkg_tiles_create` declares
  a pyramid over any extent and SRS with a zoom ladder (inclusive range,
  optional base grid and tile size, zeros taking the 1 by 1, 256-pixel
  defaults); `gpkg_tiles_create_web_mercator` fixes the extent and grid to
  the quad every XYZ basemap uses. Both return an ordinary pyramid handle,
  ready to fill. Until now a C consumer could only fill pyramids that
  already existed.

- **`geopackage-ffi`: a stored-tile cursor.** `gpkg_tiles_cursor`, `_at` and
  `_in` open a `gpkg_tile_cursor_t` that walks what a pyramid stores rather
  than probing the declared grid, O(stored) against O(grid) on a sparse
  pyramid. `gpkg_tile_cursor_next` lends each payload, valid until the next
  call, so nothing is allocated or copied per tile; the cursor counts
  against the container like an Arrow stream, and outlives the tiles handle
  it came from.

- **`geopackage-ffi`: `gpkg_validate`.** The library's file checks from C:
  an owned `gpkg_findings_t` holding the findings most severe first, walked
  with `gpkg_findings_count` and `gpkg_finding_at` (severity, description,
  repair advice where repair exists). The handle borrows nothing, so it does
  not block `gpkg_close` and outlives the container harmlessly.

- **`geopackage-ffi`: `gpkg_srs`.** The spatial reference system behind the
  id `gpkg_layer_srs_id` reports: name, organization and code, the WKT
  definition, the WKT2 definition where the CRS WKT extension carries one,
  and the coordinate epoch (NaN when absent). Every out-parameter may be
  NULL to skip it, and a failure writes none of them.

- **`geopackage-ffi`: projected opens.** `gpkg_layer_open_with_columns` and
  `gpkg_attributes_open_with_columns` open a handle that reads only the named
  columns, on `Layer::with_columns`'s terms: the feature id always, the
  geometry only if named, an unknown name refused at the open. The Arrow
  stream narrows to match, and a bounding-box read on a handle whose
  projection excludes the geometry still re-tests candidates exactly.

- **`geopackage-ffi`: `gpkg_layer_read_arrow_filtered`.** The general form of
  the Arrow readers: a bounding box (NULL or four doubles), a SQL `WHERE`
  clause (NULL or `select`'s raw-SQL contract, placeholders `?1` to `?N`
  bound from an array of the writer's `gpkg_value_t`), or both together.
  Reading one row by feature id is the clause `fid = ?1`. This closes the
  sense-check's largest gap (F1): attribute filters, subset strings and
  by-FID access over C are all this one entry point.

- **`geopackage-ffi`: tile pyramids can be enumerated.**
  `gpkg_tiles_names_count` and `gpkg_tiles_name_at` walk a file's pyramids by
  table name, mirroring the layer pair, so a C consumer no longer has to know
  a pyramid's name before opening it.

## [0.6.0] - 2026-07-29

### Added

- **Tile pyramids (M4).** A GeoPackage's second data type, alongside features
  and attributes. `GeoPackage::create_tile_pyramid` writes one from a
  `TilePyramidBuilder`, `GeoPackage::tiles` opens one and
  `GeoPackage::tile_pyramids` enumerates them; `TilePyramid` reads and writes
  tiles by address.

  Payloads stay opaque: this crate stores, indexes and validates tiles and
  decodes none of them, so it depends on no image codec and cannot produce
  pixels. What it does read is each payload's header, through the new
  `imagesize` dependency, which is how a tile of the wrong pixel size or in a
  format the table may not hold is rejected on write rather than stored.

  Reading: `get_tile` returns an owned payload, `get_tile_into` fills a buffer
  the caller reuses, and `cursor`/`cursor_at`/`cursor_in` stream tiles in matrix
  order through a lending cursor that hands out the bytes without copying them.
  There is deliberately no materialising iterator: one zoom level of a real
  pyramid is more payload than a `Vec` of them should hold. A bounding box
  becomes one range query over the table's uniqueness index rather than a loop
  of lookups.

  Writing: `put_tile`, `delete_tile`, a `TileWriter` owning its transaction, and
  a batched `write_all` taking anything that is `AsRef<[u8]>`, so a tile read
  from one pyramid reaches another's statement without a copy on either side.
  Every write is checked against the pyramid it lands in: the zoom level has to
  be declared, the column and row have to fall inside that level's grid, and the
  payload has to be a PNG or JPEG, or a WebP, which registers `gpkg_webp` as it
  lands.
- **`geopackage_core::tiles`**: the tile matrix model (`TileMatrixSet`,
  `TileMatrix`, `TileCoord`), the spec's consistency rules (Requirements 45 to
  53, checked on write and available as `TilePyramid::validate` for a file from
  elsewhere), the coordinate conventions, and the payload probe. Rows count from
  the top of the extent downwards, as WMTS and XYZ do and TMS does not;
  `TileMatrix::flip_row` converts between the two senses. GeoPackage tile
  indices are relative to a pyramid's own extent rather than to a global grid,
  so `TileMatrixSet::xyz_to_tile` checks that the pyramid is the standard web
  mercator quad and returns `NotAnXyzGrid` rather than addressing the wrong
  tile.
- **`TileMatrixSet::ladder`** builds the spec's default power-of-two zoom ladder
  over an extent, deriving pixel sizes from it so Requirement 45 holds by
  construction, with the web mercator quad as one option
  (`TileMatrixSet::web_mercator_quad`) rather than the only one. A ladder that
  does not double needs the `gpkg_zoom_other` extension, which
  `TilePyramidBuilder::allow_zoom_other` opts into: the omission is an error
  rather than a silent registration.
- **The extension catalogue is readable.** `GeoPackage::extensions` returns
  every `gpkg_extensions` row, `GeoPackage::table_extensions`,
  `Layer::extensions` and `TilePyramid::extensions` return the rows for one
  table. Until now the table could only be written, never read, so a caller had
  no way to ask what a file it had been handed actually declares.

  Each row carries an `ExtensionScope` (Requirement 64) and identifies as an
  `Extension`: the Annex F names, the two extensions the SWG removed on
  2016-08-15, and `gdal_aspatial`, with the historical spellings folded in, so
  `gpkg_elevation_tiles`, `2d_gridded_coverage` and `gpkg_2d_gridded_coverage`
  are one extension and `related_tables` and `gpkg_related_tables` are another.
  `ExtensionSupport` then says what this workspace does with it: reads and
  writes it, knows what it is and leaves it alone, tolerates it as removed from
  the standard, or does not recognise it at all. Every extension row in every
  committed fixture and in the fetched corpus is checked to classify, and the
  test fails on an unrecognised name rather than skipping it.
- **Writing to a table carrying an unidentified extension is refused**, with
  `Error::UnsupportedExtension` naming it. Requirement 64 makes every
  extension one a writer has to understand, so an extension we cannot name may
  constrain the rows, triggers or encodings of the table it covers, and
  writing beside it could produce a file its own producer can no longer read.
  This is the "fail fast" that clause 2.3.2 gives the catalogue as its purpose.
  Until now such a write went ahead silently.

  The refusal covers feature and tile writes, index builds, repairs and drops,
  and the creation of a table where the file itself carries such an extension.
  Reads are never refused: a `write-only` extension is one Requirement 64 says
  a reader may ignore, and refusing to read a file helps nobody.
  `GeoPackage::blocking_extension` and `TilePyramid::blocking_extension`
  answer the question directly, `open_lenient` reports
  `OpenWarning::UnsupportedExtension`, and
  `OpenOptions::allow_unsupported_extension_writes` turns the refusal off for
  a caller who knows the extension is harmless. Extensions this crate can
  name, implemented or not, never trigger it.
- **`Srs` carries the `gpkg_crs_wkt` extension's columns**: `definition_wkt2`
  (`definition_12_063`) and `epoch`, both `Option`. The extension could be
  written, by `add_epsg_srs` for a code with no WKT1 form, but not read: a file
  carrying a WKT2 definition read back as though it had none. `srs` and
  `srs_list` now select the columns where the file has them, and the spec's
  `undefined` reads back as `None` rather than as a definition.

  `add_srs` accepts both, adding the columns and registering the extension if
  the file lacks them, so a caller can supply a WKT2 definition for a CRS the
  EPSG registry does not describe. Design decision D3 says users may supply
  arbitrary definitions; until now that was true of WKT1 only.
- **The `gpkg_schema` extension**: column descriptions and value constraints.
  `GeoPackage::data_columns` returns a table's `gpkg_data_columns` rows,
  `column_constraint` and `column_constraints` return the constraints those
  rows point at, assembled from the rows sharing a name, since an `enum`
  occupies one row per member while a `range` or `glob` occupies one.
  `set_data_column` and `add_column_constraint` write them, creating both
  tables and registering the extension on first use.

  A column's description is attached to `Column::data_column`, so a caller
  reading a layer's schema sees the aliases and constraint names without a
  second lookup. Two pieces of leniency for files written elsewhere: the
  GeoPackage 1.0 spelling of the inclusivity columns (`minIsInclusive`) is read
  where a file uses it, and a constraint the spec's own rules rule out is an
  error when something asks what it allows rather than when the file is opened.

  `OpenOptions::enforce_column_constraints` checks written values against the
  constraints their columns declare, refusing a row that violates one. It is
  off by default because the format makes these constraints advisory, so a
  conforming file may hold values its own constraints forbid. It covers every
  write path, the columnar one included, and costs about 31% on a 200,000-row
  write with two constrained columns
  ([benchmark](roadmap/benchmarks/2026-07-27-constraint-enforcement.md)).

  The `glob` form is evaluated by SQLite, through a `SELECT ?1 GLOB ?2`
  prepared once per writer. Its pattern language has no definition beyond what
  SQLite does with it, and this crate bundles SQLite, so the engine holding the
  file is the authority on what its own constraints mean; a number also gets
  SQLite's text coercion rather than an approximation of it. It is the faster
  of the two as well, by 22% per call against a hand-rolled matcher.

- **Non-linear geometry types (Annex F.1).** `CIRCULARSTRING`,
  `COMPOUNDCURVE`, `CURVEPOLYGON`, `MULTICURVE` and `MULTISURFACE` can now be
  written, indexed and queried by extent. `create_layer` accepts one and
  registers its `gpkg_geom_<TYPE>` row; the new
  `FeatureWriter::insert_wkb` writes a body as bytes, since `geo-traits` has no
  representation for a curve to pass through `insert`.

  The new `geopackage_core::curve` computes envelopes by walking the WKB
  structure itself rather than through the georust `wkb` reader, which cannot
  parse these types. This is what removes the previous limitation ([#5]): the
  blocker was never the index, it was having no envelope to put in it.

  Arc extents are exact. A circular arc bulges away from the chord between its
  endpoints and can bulge past its middle control point, so the box of the
  three points defining it is not its bounding box. `curve::arc_envelope`
  computes the true one, by the chord-side test PostGIS uses in
  `lw_arc_calculate_gbox_cartesian_2d`. A too-small envelope would be a silent
  correctness bug rather than a tuning matter: both the GPB header envelope and
  the rtree entry derive from it, and a reader trusting either would drop
  features it should return.

  Reading a curve back as a geometry object is still not possible: `geo-traits`
  cannot describe an arc, so `Feature::geometry` errors and
  `Feature::geometry_bytes` is how one is read. That is an upstream question,
  not a local one.

  `Error::ExtensionGeometryUnsupported` is removed, since nothing raises it.
- **`geopackage-cli`**, a `gpkg` binary over the library. `gpkg info` summarises
  a file: version, layers, schemas, spatial reference systems, index state, tile
  pyramids and registered extensions with the support level this workspace has
  for each. `gpkg validate` prints what `GeoPackage::validate` found, most
  severe first, with the repair advice each finding carries, and exits non-zero
  when a finding is an error, meaning a reader can get a wrong answer from the
  file; `--strict` promotes warnings too, so the command is usable as a gate in
  a script. `gpkg index` and `gpkg repair` build and put right spatial indexes,
  the first refusing where an index is present but broken rather than quietly
  repairing it, the second leaving an absent index absent. `gpkg tiles info` and
  `gpkg tiles get` describe a pyramid and write one tile's stored bytes out.
  `gpkg copy` copies feature and attribute layers into a new file.

  `copy` carries layers only, not tiles and not the extension tables, and names
  what it left behind rather than passing over it in silence. Geometry crosses
  as WKB rather than through `geo-types`, so the non-linear curve types survive
  a copy byte for byte instead of being lost to an encoding that cannot describe
  an arc.
- **`Layer::read_arrow_in`**, the columnar counterpart of `Layer::features_in`,
  returning the same rows as Arrow record batches. Single-threaded: the threaded
  reader assigns key windows to workers on the assumption that a window's span
  implies its row count, and a spatial filter voids that, since matching rows
  scatter through the key space.

  Candidates from the index are re-tested against their true `f64` envelope
  before being returned, because the index stores `f32` envelopes and is queried
  with outward-widened bounds, so its candidates are a superset. Without that a
  filtered columnar read would return rows `features_in` does not.
- **`geopackage-ffi`**, a C ABI over the library, built as a `cdylib` and
  `staticlib` and packaged with cargo-c, so `cargo cinstall` produces a
  versioned soname, a header and a pkg-config file. Opaque `gpkg_t`,
  `gpkg_layer_t` and `gpkg_tiles_t` handles; UTF-8 strings in both directions;
  failures through a `gpkg_error_t` out-parameter carrying a category code and
  a message. The data plane is the Arrow C Data Interface in both directions,
  including the bounding-box read, and a layer can be created from an Arrow
  schema, so a C consumer can copy a layer with no schema-description API of its
  own.

  Two rules a caller has to know. Handles belong to one thread, because
  `rusqlite::Connection` is `Send` and not `Sync`. And closing a container is
  refused while any handle taken from it is still alive, which is what makes the
  design sound rather than a matter of caller discipline.

  This is the only crate in the workspace containing `unsafe`; every other crate
  sets `unsafe_code = "forbid"`. It is checked by AddressSanitizer, by miri over
  the parts miri can reach, by a committed header that CI regenerates and diffs,
  and by two C programs compiled and run in CI.
- **`Layer::count`**, a `SELECT COUNT(*)` rather than counting by iterating,
  which materialises every feature.
- **`GeoPackage::open_read_only_lenient`**, read-only and tolerant at once. The
  files most worth inspecting are the ones something is wrong with, and
  inspecting one should not need write access to it.
- **`Display` for `Finding`, `Severity`, `GpkgVersion`, `OpenWarning`,
  `SpatialIndexStatus`, `LayerKind`, `ExtensionSupport` and `ExtensionScope`**,
  so a consumer printing one does not have to match on it. Each also gains an
  `as_str` where the rendering is a fixed word.
- **Row-at-a-time writes in the C ABI**: `gpkg_layer_writer` returns a
  `gpkg_writer_t` with `gpkg_writer_insert`, `gpkg_writer_update`,
  `gpkg_writer_update_column` and `gpkg_writer_delete`, finished by
  `gpkg_writer_commit` or discarded by `gpkg_writer_free`. Until now features
  crossed only as Arrow, which appends, so a C consumer could load a file but
  never edit one.

  Values cross as `gpkg_value_t`, a tag and a union mirroring `ValueRef`, with
  text and binary borrowed from the caller for the duration of the call. Dates
  cross as `gpkg_date_t` and `gpkg_datetime_t` rather than as text, so that an
  impossible date is refused at the boundary rather than binding as text and
  skipping the check the crate makes before writing one. Geometry crosses as
  WKB and is stored as it arrives, so a curve survives a write.

  A feature id is passed as a pointer, NULL to have one assigned, because every
  `int64_t` is a legal id and no sentinel would do.
- **`FeatureWriter::update_wkb`**, the counterpart of `insert_wkb`. `update`
  takes a `GeometryTrait`, which a curve has no representation for, so
  replacing a circular string previously meant deleting the row and inserting
  it again. This is also what the C ABI's update is built on.
- **`gpkg_begin`, `gpkg_commit` and `gpkg_rollback` in the C ABI**, with
  `gpkg_in_transaction` to ask the state without provoking an error. These were
  withheld until now because a C consumer who began a transaction and then wrote
  anything got "cannot start a transaction within a transaction" from the write.
  Each refuses the state it cannot honour, a second begin or an unbalanced
  commit or rollback, with `GPKG_STATUS_INVALID_ARGUMENT` and a message saying
  which.

### Changed

- **Every write path joins a transaction the caller has already begun**, rather
  than failing. SQLite does not nest transactions, so a caller who opened one on
  `GeoPackage::connection()` and then used the ordinary API previously got
  "cannot start a transaction within a transaction". `Layer::extent` has always
  inherited instead; every write path now does, which is what makes the C
  transaction calls above possible.

  Three consequences for a caller who does open one, and none at all for a
  caller who does not:

  - **`FeatureWriter::commit` and `TileWriter::commit` stage rather than
    commit.** They still flush `gpkg_contents` (`last_change`, and the bounding
    box when a geometry was written), and still report success, but the durable
    commit is the caller's to issue.
  - **Dropping a writer without committing rolls nothing back**, so an error
    part-way through leaves what preceded it staged for the caller to discard.
  - **`Layer::write_all`'s `batch_size` stops bounding transactions**, because
    every batch belongs to the caller's. An error part-way then leaves every row
    staged rather than some of them committed, which is the same all-or-nothing
    a `batch_size` of `0` gives.

  `GeoPackage::create` is the one write path that still opens its transaction
  outright, since it opens its own connection and no caller can hold a
  transaction on it.

- **A core-typed container holding a non-linear member says so**, rather than
  reporting a malformed geometry. A `GEOMETRYCOLLECTION` carrying a
  `CIRCULARSTRING` cannot be written: which reader a body takes is decided from
  its own type code, so a core-typed container reaches the `wkb` reader, which
  cannot read the member. The body is well formed, and the message used to say
  it was not. `GeometryError::NonLinearMember` now names both types. A body
  that is genuinely malformed still reports as one.

- **Writing a container geometry registers its member types**, not only the
  type its column declares. A `MULTICURVE` column holding `CIRCULARSTRING`s
  needs a `gpkg_geom_CIRCULARSTRING` row as well as a `gpkg_geom_MULTICURVE`
  one (Annex F.1 Requirement 67), and a member type is visible only in the
  bytes: `create_layer` is told the column's type and nothing else. GDAL writes
  both rows, and until now this crate wrote one, so a file it produced
  under-declared what it contained.

  The walk that computes a curve's envelope now records the non-linear types it
  passes at every nesting depth, so a `MULTISURFACE` holding a `CURVEPOLYGON`
  whose ring is a `CIRCULARSTRING` registers all three. `FeatureWriter`
  accumulates them across the rows it writes and registers what is missing when
  it flushes, so a large write costs one registration pass rather than one
  lookup per row, and a writer dropped without committing registers nothing.
  Only `insert_wkb` and `update_wkb` can contribute: a `GeometryTrait` has no
  non-linear representation to offer.

- **Tile, geometry and Arrow failures reach C as a category rather than as
  `GPKG_STATUS_OTHER`.** `Error::Tile`, `Error::Core` and `Error::Arrow` each
  carry an error enum of their own, and the status mapping stopped at the
  outermost variant, so everything underneath arrived uncategorised: a tile
  written off its grid, a malformed geometry, a bad GPB header, a stream that
  failed part-way. The classification now follows the wrapping down to the
  variant that says what happened, and every variant of `geopackage::Error` is
  now classified. The messages are unchanged.

  For tiles: an address outside the grid, bytes that are not a readable image
  and an unusable zoom range are `GPKG_STATUS_INVALID_ARGUMENT`; a pyramid that
  breaks one of the spec's consistency rules, and a payload whose dimensions are
  not the ones its zoom level declares, are `GPKG_STATUS_CONSTRAINT`; an XYZ
  conversion asked for on a grid that is not the web mercator quad is
  `GPKG_STATUS_UNSUPPORTED`.

  For geometries: a body that cannot be read as ISO WKB, a GPB blob that is
  truncated or carries the wrong magic, and an identifier that cannot be quoted
  are `GPKG_STATUS_INVALID_ARGUMENT`; a GPB version this library does not
  implement, and a well-formed body this library cannot write, are
  `GPKG_STATUS_UNSUPPORTED`. The distinction that matters is the last one: a
  `GEOMETRYCOLLECTION` holding a `CIRCULARSTRING` is refused for what it is
  rather than for anything wrong with its bytes, and reporting that as an
  invalid argument would send a caller looking for a fault that is not there.

  For Arrow: a schema that does not fit, a value that will not convert and a
  batch that does not survive the C Data Interface are
  `GPKG_STATUS_INVALID_ARGUMENT`. An `ArrowError` wrapping one of this library's
  own errors, which is how one survives a stream boundary, is classified by the
  error inside it, so piping a stream from this library into
  `gpkg_layer_write_arrow` does not flatten a category that was already known.
  `ArrowError` is not `#[non_exhaustive]`, so the match names only the variants
  these paths produce and leaves the rest to `GPKG_STATUS_OTHER` rather than
  breaking on an `arrow-rs` bump.

- **`gpkg_close`'s refusal names every kind of handle that can hold it open.**
  It counted layers, tile pyramids, writers and Arrow streams alike but said
  "layer handle(s)" and named only `gpkg_layer_free`, which left a caller
  holding a pyramid looking for a layer they had already freed. Message only.

- **`gpkg_zoom_other` and `gpkg_webp` are Annex F.6 and F.7**, not F.4 and
  F.5, which is what their constants' documentation claimed. Annex F numbers
  the extensions in the order the annex includes them, and the two removed in
  2016 still occupy their places in that sequence. Documentation only.

- **`gpkg_extensions` rows are written in one place**
  (`geopackage::extensions`), rather than by each extension's own module.
  Behaviour is unchanged.

## [0.5.0] - 2026-07-26

### Added

- **Column projection: `Layer::with_columns` and `Layer::without_geometry`.**
  A read used to fetch every column and copy the geometry into every row
  whether or not anything looked at it, which on a geometry-heavy layer is
  most of the cost of an attribute scan: 5,000 rows carrying 1,000-vertex
  linestrings, reading one integer, went from 15.7 ms to 4.2 ms, against a
  3.8 ms floor for the same query in raw SQL with no `Feature` built at all.
  Columns come back in the table's order however they are named, so this
  selects rather than reorders; the feature id is always present; and an
  unknown name is rejected by `with_columns` rather than quietly selecting
  nothing.

  A projection is a read concern. `Layer::writer` on a projected handle still
  writes the layer's whole column list, so a partial row cannot be inserted by
  accident, and a bounding-box query still reads each candidate's geometry to
  filter exactly, it simply does not carry it into the feature.
- **`Feature::has_column` and `Feature::has_geometry_column`**, and
  **`Error::GeometryNotProjected`**. `Feature::value` already distinguished a
  NULL cell (`Some(ValueRef::Null)`) from an absent one (`None`), but
  `Feature::geometry` could not tell a NULL geometry from one a projection
  dropped, since both would be `Ok(None)`. The projected case is now an error
  instead. A layer with no geometry column at all is unaffected: nothing was
  projected away there, so it answers `Ok(None)` as it always has.

## [0.4.0] - 2026-07-26

### Changed

- **Every connection this crate opens gets a five second busy timeout.** None
  was set before, so SQLite's default of zero applied and any lock another
  connection held turned an ordinary write into an immediate `SQLITE_BUSY`. For
  a format whose concurrency story is one writer and many readers, that made
  every write path fail on the first attempt against a cooperating process.
  Set through the new `OpenOptions::busy_timeout`, defaulting to
  `DEFAULT_BUSY_TIMEOUT`, and applied to read-only connections too, since a
  reader can meet a writer's lock under a rollback journal. A caller-supplied
  connection keeps whatever it has unless it asks, on the same principle that
  leaves its journal mode alone. This is the whole of the crate's retry policy,
  deliberately: SQLite bypasses the wait entirely for a read-to-write upgrade
  deadlock and for a stale WAL snapshot, so a retry loop above it would only
  spin before failing anyway.
- **`FeatureWriter` no longer records a bounding box it cannot vouch for.** A
  GeoPackage may carry a NULL `gpkg_contents` extent, which is spec-legal and
  which GDAL itself writes in some paths. Opening such a layer and inserting one
  feature used to record a box covering only that feature, excluding every row
  already there: an honest "unknown" replaced by a confidently wrong value, and
  a wrong extent is the worst of the three states, because GDAL and QGIS both
  return a well-ordered box verbatim and never recompute it. The test is not
  whether the recorded box is absent but whether the table already holds rows:
  absent over an empty table means the fold is the whole content and is exact,
  absent over a populated one means it is only a lower bound. An inverted box
  now reads as absent, as GDAL reads it, rather than being grown from.
- **`Layer::extent` records what it had to measure**, so reading the extent of a
  file whose recorded bounds are unusable changes that file's contents and
  modification time. This mirrors GDAL, whose `GetExtent` persists through
  `SaveExtent` on any dataset open for update, and it means a file improves by
  being read rather than staying wrong for every later reader. Nothing is
  written when the recorded box is usable, nor on a read-only connection, nor
  when there is nothing to measure and the bounds are already NULL. Open
  read-only, or read `GeoPackage::contents`, for the recorded values without
  measuring or writing anything.

### Added

- **`Layer::extent` and `Layer::recompute_extent`.** The first returns the
  recorded `gpkg_contents` bounds when they are usable and otherwise measures
  the geometries; the second measures unconditionally and records the result,
  the equivalent of GDAL's `RECOMPUTE EXTENT ON`. A layer with nothing to
  measure has its bounds set to NULL rather than to anything invented, which is
  what makes a reader compute the extent for itself. Both measure the
  geometries exactly rather than taking the RTree's outward-rounded word for
  what the layer contains.
- **`FeatureWriter::update_columns` and `update_column`**, which update named
  value columns and leave the rest of the row, and the geometry, untouched.
  `update_row` sets every value column, so a caller recomputing one had to
  restate the others. Modelled on GDAL's `OGRLayer::UpdateFeature` (RFC 93);
  the statement is held and keyed on the column names, so a loop recomputing
  the same column prepares once. Naming a column twice is
  `Error::DuplicateUpdateColumn` rather than SQLite's silent last-wins.
- **`Layer::audit_spatial_index`, `SpatialIndexAudit` and
  `Layer::rebuild_spatial_index`.** `spatial_index_status` derives everything
  from structure, whether the virtual table and the right triggers exist, and
  `repair_spatial_index` inherited that blind spot: it returned without reading
  a row whenever the structure was current. An index populated partially, not at
  all, or holding entries for since-deleted rows could therefore be neither
  detected nor fixed. The audit reads the geometries and reports how many rows
  should be indexed against how many entries are missing, extra, or present but
  not covering their geometry; the rebuild does unconditionally what the repair
  does only when the structure is wrong.
- **`OpenOptions::busy_timeout` and `DEFAULT_BUSY_TIMEOUT`**; see above.
- **`Error::ExtentPersist`**, when a measured extent could not be recorded for a
  reason other than another connection holding a lock. It carries the
  measurement, so the answer is not lost with the failure. Lock contention does
  not produce it: a concurrent writer means the measurement describes a layer
  changing underneath it, so the file keeps what it had and the measurement is
  returned.
- **A "What writes, and when" table in the crate documentation**, tabulating
  every call by whether it writes to the file and what it does on a read-only
  connection, with the failure causes beneath it. Three calls do not divide
  cleanly into reads and writes and the names do not say so: `Layer::extent`
  records what it had to measure, `repair_spatial_index` writes only when there
  is something to repair, and `Layer::writer` opens without writing at all,
  because `BEGIN DEFERRED` takes no lock, so its first failure lands on the
  first row. Pinned by tests.
- **`scripts/run_gdal_validator.sh`**, running `gdal driver gpkg validate`
  (GDAL 3.13+) or the `osgeo_utils.samples.validate_gpkg` script it wraps. It
  is the only one of the three validators this repo uses that checks the
  `gpkg_contents.last_change` format. Note the false positive documented in its
  header: the empty-geometry check reads the GPB empty flag from the wrong bit
  and rejects any layer holding an empty geometry, including files GDAL itself
  writes.

### Performance

Allocation counts are exact, from a counting global allocator over a 2,000-row
fixture.

- **Insert, update and delete cost no allocation a row**, at any column count.
  The writer built its `UPDATE` and `DELETE` statement text on every call, which
  scaled with the layer's width at 10 allocations a row on three value columns
  and 30 on twelve, and then looked the statement up in the connection's cache
  for one more. It now holds every statement it can issue, prepared once at
  construction from the connection rather than from its transaction, so it stays
  a plain struct rather than a self-referential one.
- **A scan that recomputes one column costs 2 allocations a row**, down from 12,
  the remainder being the owned `Feature` on the read side.

## [0.3.0] - 2026-07-26

### Changed

- **A layer's value columns no longer include the primary key.** The read path
  counted the key among a row's values while the write path excluded it, so the
  two sides of the API disagreed about what a row's values were: feeding a
  feature's values straight into a writer for an identically-shaped layer
  compiled and then failed at run time with `ValueCountMismatch`, and the caller
  had to know to strip the key by name. The key is reached through
  `Feature::fid`, as it always was, and GDAL draws the same line between a
  feature id and its fields. `Feature::value("fid")` is now `None`,
  `Feature::get` indices shift by one on a layer with a named primary key, and
  `Feature::columns` no longer lists it. As a side effect every query stopped
  selecting the key twice.
- **`Feature` hands out `ValueRef<'_>` rather than `&Value`.** A feature used to
  be a `Vec<Value>` plus a `String` or `Vec<u8>` for every text and blob cell:
  seven allocations a row on a thirteen-column layer with four text columns and
  a blob. Its geometry and every variable-length cell now sit end to end in one
  buffer with a range recorded per value, which is two allocations a row
  whatever the row's width, so there is no longer a `Value` inside a feature to
  lend out. `Feature::value`, `get`, `values` and `iter` return the new borrowed
  type; `values` returns an iterator rather than a slice. `ValueRef::to_value`,
  or `Value::from`, gives an owned value where one is needed.
- **The write path takes borrowed values.** `FeatureWriter::insert`,
  `insert_row`, `update` and `update_row`, and `Layer::select` and
  `cursor_select`, take `&[ValueRef<'_>]`. Nothing in the implementation needed
  ownership: bindings were already made by reference out of the `Value` they
  were given, so the owned signature only forced callers to build values they
  then handed straight over. A row read from one layer now binds into another
  without its text and blob cells being copied, and a literal parameter needs no
  allocation. `NewFeature` keeps `Vec<Value>`, because `write_all` consumes an
  iterator whose items must outlive any single call, which is the rule the API
  now follows: borrowed where a value need not outlive the call, owned where it
  must.

### Added

- **`ValueRef<'a>`**, the borrowed counterpart of `Value`, with `to_value`,
  `is_null`, and an accessor per variant (`as_str`, `as_blob`, `as_bool`,
  `as_i64`, `as_f64`, `as_date`, `as_datetime`). None of them converts between
  variants: what a cell reads as is driven by its column's declared type, not by
  its contents, so an `INTEGER` column holding `0` reads as `Integer` and stays
  that way under `as_bool`. `From` converts in both directions, and a `ValueRef`
  compares directly against a `Value`.
- **`gpb::header_len` and `gpb::encode_header_into`** in `geopackage-core`, so a
  caller can size a blob for a GPB header and its WKB body together and append
  both into one allocation. `encode_header` is unchanged.

### Performance

Allocation counts below are exact, from a counting global allocator over a
200,000-row fixture of thirteen attribute columns. Wall-clock effects on these
paths are mostly smaller than the run-to-run spread of the machine they were
measured on, so they are not quoted here; see
`roadmap/benchmarks/2026-07-25-real-datasets.md` for the dataset timings and
their caveats.

- **The materialising read is 40 allocations a row lighter.** `Layer::execute`
  rebuilt its per-row context inside the row loop, cloning the table name, the
  geometry column and the whole column list for every feature returned: 40
  allocations a row against 8 for the same work through `cursor`. It is built
  once per query, as the cursor path already did.
- **A feature is two allocations rather than seven** on that fixture, from the
  buffer described above, and one fewer again after `DATE` and `DATETIME` cells
  stopped being copied into a `String` only to be parsed and dropped.
- **The bulk index build no longer duplicates its entry set.** The gate built a
  `HashMap` of every `(fid, envelope)` pair to remove entries as it scanned;
  it now sorts and binary-searches the vector it already owns. The packer
  collected leaf cells and then drained them into a second keyed vector, and
  allocated a blob per node and a cell vector per leaf; those are one pass and
  two reused buffers. Transient bytes over 200,000 rows fall from 52 MB to
  25 MB, and peak resident memory over 11.5M rows from about 1246 MB to about
  1148 MB, medians of three runs each.
- **The write paths bind by reference.** Every insert and update deep-copied
  each text and blob cell before handing it to SQLite; each also collected a
  bindings vector per row, and the columnar path collected a second one. The
  scalar write is 4.08 allocations a row before and 2.04 after, the columnar
  7.08 and 5.04. Each GPB blob is now sized for its header and body together
  rather than being grown once per geometry, which removes a reallocation a row.

### Fixed

- **`Feature::geometry_bytes` and the value accessors no longer disagree with
  the writer about the primary key**, as described under Changed. The
  round-trip is now covered by a test.
- **The bulk index gate rejects an index whose stored bounds exclude the
  geometry they index.** The existing fault test deleted a row, which the row
  count caught before any bounds were compared, so the comparison itself was
  never exercised.

## [0.2.0] - 2026-07-25

### Added

- **A ceiling on the geometry bytes one Arrow batch may hold**
  (`ArrowReadOptions::max_batch_bytes`, with `DEFAULT_MAX_BATCH_BYTES` and
  `default_max_batch_bytes`). The geometry column is Arrow `Binary`, whose
  offsets are `i32`, so one batch cannot address more than 2 GB of WKB. At the
  default 65,536 rows that needs only 32 KB of geometry per feature, which large
  polygons exceed. A batch that would cross the ceiling is emitted short and the
  rows that did not fit begin the next one, on both the single-threaded and
  threaded paths. The default follows GDAL in taking `min(INT32_MAX, RAM / 4)`.
- **Columnar read and write through Apache Arrow, behind a new `arrow`
  feature.** `Layer::read_arrow` returns an `ArrowBatches`, which implements
  `RecordBatchReader`; `Layer::write_arrow` writes record batches back through
  the same batching, bulk-index decision and transaction handling as
  `write_all`. Attribute columns follow a documented type mapping, and the
  geometry column is WKB carrying the `geoarrow.wkb` extension name, which costs
  nothing because a GPB body already is ISO WKB.

  Reads are threaded by default: `min(4, available parallelism)` workers, each
  with its own read-only connection over a disjoint primary-key range, with
  batches still arriving in key order. 259.5ms at the default against 529.6ms
  pinned to one thread over the same file. The threaded path declines to the
  single-threaded one rather than failing when its conditions do not hold: an
  in-memory database, a primary key with gaps, or fewer than two batches of
  rows. `ArrowReadOptions` sets the rows per batch (`DEFAULT_BATCH_SIZE`,
  65,536) and the thread count, where `1` reads on the calling thread.

  Neither direction goes through `Feature` or `Value`. Arrays are built from the
  statement's column values inside a SQLite aggregate function, and a row being
  written binds straight out of the Arrow buffers. That is a constraint on the
  implementation rather than an optimisation of it: GDAL measured its generic
  Arrow path, which does materialise a row, as slower than the row API it wraps.

  `Layer::arrow_schema` and `TableSchemaBuilder::from_arrow_schema` are the two
  directions of the type mapping, so a layer can be copied into a new file
  without its schema being restated; `TableSchemaBuilder::primary_key_name` is
  public for the same reason. New error variants: `Error::Arrow`,
  `Error::ArrowValueMismatch` and `Error::UnsupportedArrowType`.

- **EPSG codes outside the vendored WKT1 subset now register.**
  `GeoPackage::add_epsg_srs` refused anything the vendored subset did not carry.
  It now falls back to the EPSG registry, and a code with no WKT1 form at all,
  such as the geographic 3D EPSG:4979, is written as WKT2 into the
  `definition_12_063` column of the `gpkg_crs_wkt_1_1` extension, with
  `definition` holding the literal `undefined`. That is what the spec and GDAL
  both do for these codes; GDAL reads such a layer back as geographic 3D and
  normalises the WKT2 to a string identical to its own.

  Adding the extension columns, backfilling WKT2 for rows already present and
  inserting the new row share one transaction, so a failure leaves the file as it
  was rather than half-carrying an extension. Only a code in neither the subset
  nor the registry is still `Error::UnknownEpsgCode` ([#23]).

- **`geopackage-core` primitives the columnar paths needed, useful on their
  own.** `gpb::body_offset` gives a blob's WKB body offset without decoding the
  envelope. `geometry::encode_gpb_from_wkb` and `EncodedGpb` put a GPB header in
  front of bytes that are already ISO WKB, rather than parsing a geometry and
  serialising it straight back out; the body is still parsed, because the
  envelope has to be computed and because parsing is what rejects a body that is
  not ISO WKB, such as PostGIS EWKB. `Date::days_since_epoch`,
  `DateTime::micros_since_epoch` and their inverses give callers the boundary
  conversion the `datetime` module docs recommend without their having to take a
  datetime crate ([#24]).

- **`StorageStrictness`, controlling the two `Value` conversion leniencies.** A
  `BOOLEAN` column holding an integer other than 0 or 1, and an integer reaching
  a `FLOAT`/`DOUBLE` column, are both readable as their declared type and both
  non-conformant. `ConversionOptions::storage` now decides which of those facts
  wins. `StorageStrictness::Lenient` is the default and behaves as before;
  `Strict` reports `Error::NonBooleanInteger` (a new variant) for the first and
  `Error::ValueTypeMismatch` for the second. `ConversionOptions::with_datetime`
  and `with_storage` set the two axes independently ([#1]).

### Changed

- **GeoArrow CRS metadata is PROJJSON**, which is the form that specification
  prefers; it says an authority code "should only be used as a last resort",
  because it leaves the reader to resolve the code against a registry it may not
  have. An `EPSG:<code>` string remains the fallback for a code the registry
  does not know.

  Reading it back is a JSON parse rather than a scan for the first code. A CRS
  object nests identifiers for its coordinate system, datum and ellipsoid, and
  in EPSG:4326 the first to appear is 6422, the ellipsoidal coordinate system;
  only the top-level identifier names the CRS. The reader therefore also accepts
  PROJJSON written by other producers, which the earlier substring match could
  not ([#23]).

- **Calendar arithmetic is deferred to [jiff](https://docs.rs/jiff).** `Date`
  validates against jiff's calendar instead of carrying its own
  `days_in_month`/`is_leap_year`, and the new epoch conversions replace four
  hand-rolled implementations that had accumulated in the columnar path. The
  0-9999 year bound stays, since it comes from the spec's four-digit text form
  rather than from the calendar.

  jiff is configured with no timezone database at all: a GeoPackage `DATETIME`
  is UTC by definition and this workspace transforms neither coordinates nor
  times, so none of the `tz-*` or `tzdb-*` features are wanted. Measured at
  about 1.2 KB on a release binary, because only the entry points used survive
  dead-code elimination. No jiff type appears in any signature, so a jiff major
  version is not a breaking change here, and which text forms are accepted on
  read and written back is unchanged ([#24]).

- **The scalar write path is faster.** `FeatureWriter` composed its `INSERT`
  statement on every call: for a fifteen-column table roughly seventeen
  allocations per row, to produce one of four fixed strings. The four are now
  built once per writer and indexed by whether the row carries an explicit id
  and whether it carries a geometry. 22.6% off an unindexed point write and
  16.0% off a bulk one, measured at the same row count. This cost has been in
  the released write path since that path existed; it was found by asking what
  remained once the columnar-specific candidates were gone.

- **`create_layer` now builds a spatial index by default** ([#26]). Previously a
  feature layer came back unindexed and the caller asked for an index
  separately. Decline it with `TableSchemaBuilder::spatial_index(false)`.

  This changes the bytes a file gets: an indexed layer carries the
  `rtree_<table>_<column>` virtual table, its shadow tables, the GeoPackage 1.4
  trigger set and a `gpkg_extensions` row. All of that is spec-legal and is what
  `ogr2ogr` produces, but it is a change rather than an improvement in disguise.

  The reasoning: every other implementation indexes by default, and without an
  index `Layer::features_in` still answers correctly by falling back to a full
  scan, so the absence is invisible until someone profiles it. Building it at
  layer-creation time also makes it empty, which is the state that lets a
  subsequent large `write_all` or `write_arrow` fill it in one bulk pass instead
  of row by row through the triggers, so the default arrangement is also the fast
  one.

  Attribute tables are unaffected, having no geometry column.

  The table and its index are created in one transaction, so a failure building
  the index leaves nothing behind rather than a registered feature table with no
  index for the caller to notice and clean up.

- **`ConversionOptions::strict()` is now strict on both axes**, so it is no
  longer the same value as `ConversionOptions::default()`. `default()` is
  unchanged in behaviour: strict `DATETIME` parsing with lenient value reading.
  A caller who wants only strict `DATETIME` parsing wants `default()`. `Layer`
  seeds itself from `default()`, so feature reads are unaffected ([#1]).

- **`write_all` reaches the bulk index path for streaming sources.** The size of
  a write was taken from `Iterator::size_hint`, whose lower bound is 0 for most
  iterators not backed by a collection, so such a write stayed on the per-row
  triggered path however large it turned out to be unless the caller passed
  `always_bulk`. Where the hint cannot settle the question, rows are now buffered
  up to `bulk_threshold` to settle it, which is bounded by the threshold and
  never by the length of the input. Sized sources are unaffected ([#17]).

- **A bulk `write_all` normalises the trigger set it reinstalls**, which it has
  always done but now does in more cases. The path drops whatever RTree triggers
  a file carries and reinstalls the GeoPackage 1.4 set, so a file written against
  a pre-1.4 trigger generation comes out with 1.4 triggers. That was previously
  reachable only for a fresh bulk load into an empty index; it now also follows a
  large write into a populated index, or one from a source that does not
  advertise its length ([#17]).

- **The index is rebuilt or appended to, decided after the write.** A bulk
  `write_all` used to choose between rebuilding the index and leaving it to the
  triggers before writing a row, from that same lower bound. It now writes the
  rows first and chooses with both counts exact. A write too small to be worth a
  rebuild adds its entries to the existing index directly, running the statement
  the `_insert` trigger would have run over the envelopes computed during
  encoding, rather than falling back to per-row trigger maintenance. The
  resulting index is identical to a triggered write's ([#17]).

## [0.1.2] - 2026-07-24

Performance across all three index and read paths, plus a streaming read. No
API is removed or changed; the additions are `Layer::cursor`, `cursor_in`,
`cursor_select` and the `FeatureCursor` / `FeatureStream` types they return.

### Added

- **Streaming reads.** `Layer::cursor`, `Layer::cursor_in` and
  `Layer::cursor_select` return a `FeatureCursor` whose `features()` yields a
  `FeatureStream`, holding one row at a time instead of materialising the result
  set. Over 100k features this reads in 18.8ms against 29.5ms for the
  materialising methods, with peak memory bounded by a row rather than by the
  query.

  It is two calls rather than one because rusqlite's row cursor borrows its
  `Statement`, so an iterator owning both would be self-referential, which
  `#![forbid(unsafe_code)]` rules out without a helper crate. The cursor owns
  the statement and the stream borrows it, which is the shape rusqlite itself
  uses.

  `features`, `features_in` and `select` are unchanged and remain the right
  default for layers small enough that the result set is not a problem. Both
  paths build the same `Feature`s through the same code, so they never differ
  in results ([#4]).

### Changed

- **`create_spatial_index` is about 12% faster** (1504ms to 1330ms at 1M
  points). Its entry set was built by asking SQLite for `ST_IsEmpty` plus the
  four `ST_Min`/`ST_Max` functions per row: six user-function dispatches, each
  re-fetching the blob and re-parsing the GPB header. It now reads each blob
  once and computes the envelope in Rust, borrowing the blob rather than copying
  it. Emptiness is still decided exactly as `ST_IsEmpty` decides it and the
  bounds still come from the header envelope when present, so the accumulated
  set is identical rather than merely close ([#22], thanks to @sayrer).
- **A large `write_all` into an already-populated spatial index now rebuilds it
  in bulk** rather than letting the triggers append row by row. A rebuild costs
  roughly 1.5us per row of the whole table where a triggered append costs 18 to
  40us per new row, so the rebuild wins once the new rows are more than about a
  tenth of the existing ones, which is where the threshold now sits. Appending
  100k rows to a 1M-row indexed layer goes from 2938ms to 1783ms ([#17]).

### Fixed

- The bulk build path is now covered for NULL, empty and envelope-less
  geometries. The existing test for that behaviour used default options on a
  three-row table, which is far below the bulk threshold, so it only ever
  exercised the triggered path.

## [0.1.1] - 2026-07-24

A performance and durability release. No API is removed or changed: the only
public additions are `StructuralCheck`, `DEFAULT_FILL_FACTOR` and two builder
methods on `BulkIndexOptions`, so upgrading from 0.1.0 needs no code changes.

It also carries metadata fixes that 0.1.0 could not: that release's crates.io
and docs.rs pages point at a repository URL that does not exist, and show no
README, neither of which can be corrected in place.

### Added

- `StructuralCheck` and `BulkIndexOptions::with_structural_check`, choosing how
  thoroughly the bulk-build gate checks database structure: `RtreeOnly` (the new
  default, `rtreecheck()` over the index just built) or `FullDatabase`
  (additionally a whole-database `PRAGMA integrity_check`, which is what 0.1.0
  always did).
- `DEFAULT_FILL_FACTOR` and `BulkIndexOptions::with_fill_factor`, setting the
  fraction of each RTree node filled when packing. The default of 1.0 gives the
  smallest tree and the best queries; lower it when a freshly built index will
  be appended to heavily.
- Runnable examples: `inspect`, `bulk_load`, `bbox_query` and `repair_index`.

### Changed

- **Bulk spatial-index builds are about 3.5x faster.** A 1M-point indexed
  `write_all` goes from 7.31s to roughly 2.08s. Four changes got there, in
  decreasing order of effect: the RTree is now constructed directly and its
  shadow tables written, rather than every entry being inserted into a scratch
  index and copied ([#20]); `%_rowid` rows are inserted in feature-id order
  rather than tree order, since that table is keyed by feature id and inserting
  in Hilbert order paid a page split per row; the gate uses `rtreecheck()` on
  the new index instead of a whole-database `integrity_check` ([#16]); and
  `write_all` reuses the envelopes it computed while encoding instead of
  re-deriving them with an `ST_*` scan.
- Measured like for like, both implementations building an index over the same
  rows of the same file, this is level with GDAL: 8% slower on uniformly spread
  points and 9% faster on clustered points, while running a verification pass
  GDAL has no equivalent of and producing a tree a third smaller for the same
  query latency. Queries are unaffected or slightly faster than 0.1.0.
- **A bulk `write_all` is now atomic.** Dropping the RTree triggers, every row
  insert, the `gpkg_contents` flush, the index rebuild and reinstalling the
  triggers all commit together, so a crash or error part-way through leaves the
  file exactly as it was. In 0.1.0 the rows committed before the index was
  rebuilt, and a crash in between left a `Stale` index needing
  `repair_spatial_index()`. That split was forced by an `ATTACH`ed scratch
  database, which is now gone ([#15]).
- The bulk-build gate is a large share of a build: roughly 45%, about 745ms of a
  1593ms build at 1M points, split between checking the written index against
  the input and `rtreecheck`. It is why the build is level with GDAL rather than
  ahead of it. The check stays, because the RTree is written by hand into a
  format SQLite does not document as an interface; whether it should become
  optional is a question for 1.0.
- Peak memory during a bulk build is lower: the tree is streamed into the shadow
  tables as it is built rather than assembled whole in memory first.

### Fixed

- Both crates now render their README on crates.io. 0.1.0 showed none.
- The `repository` URL now points at the real repository rather than at
  `github.com/georust/geopackage`, which does not exist.
- The crate documentation now states the untrusted-input limitation ([#3]) that
  0.1.0 shipped with but did not mention: the `wkb` 0.9.2 reader pre-allocates
  from unbounded element counts, so a malformed geometry can drive a very large
  allocation. Still unfixed upstream, so it remains a known limitation here: do
  not parse GeoPackage files from untrusted sources.

## [0.1.0] - 2026-07-24

First release: the GeoPackage 1.4 read path (M1) and write path with
spec-correct spatial indexing (M2), across the `geopackage-core` and
`geopackage` crates.

### Added

- **Container.** Create and open with pragma and schema validation
  (`GeoPackage::create`, `open`, `open_read_only`, `from_connection`);
  `open_lenient` tolerates legacy and lightly malformed files, collecting typed
  `OpenWarning`s instead of failing. The `ST_*` SQL functions required by the
  spatial-index triggers are registered on every connection.
- **Read path.** Layer enumeration and typed `Layer` handles
  (`layers`, `layer`, `attributes`); `Layer::features` iterating owned
  `Feature`s with by-name and by-index value access and lazy geometry parsing;
  `Layer::features_in` bounding-box queries, RTree-accelerated where an index
  exists and property-tested identical to a full scan; `Layer::select` for a
  caller-supplied `WHERE` clause.
- **Write path.** `TableSchemaBuilder` with `create_layer` /
  `create_attributes_table` emitting user-table DDL and catalogue rows in one
  transaction; `Layer::writer` returning a transaction-owning `FeatureWriter`
  with `insert`/`update`/`delete` over any `impl GeometryTrait<T = f64>`;
  batched `Layer::write_all`; `gpkg_contents` `last_change` and bounding-box
  maintenance on commit; DATETIME serialisation in the strict 1.4 format.
- **Spatial index.** `create_spatial_index`, `drop_spatial_index`,
  `has_spatial_index` and `repair_spatial_index` over the GeoPackage 1.4
  trigger set (`update5`/`update6`/`update7`), with pre-1.4 and mixed
  generations detected and repairable rather than silently mixed. The bulk
  shadow-table build (`BulkIndexOptions`) accelerates fresh index construction,
  gated by a bijection/containment check plus `PRAGMA integrity_check` with an
  automatic fallback to the triggered path on any anomaly.
  `Layer::spatial_index_status` reports `Absent`/`Current`/`Legacy`/`Stale`, and
  `repair_spatial_index` recovers a `Stale` index left by a crash mid-bulk-build.
- **Durability.** `OpenOptions` with typed `JournalMode` and `Synchronous`
  enums; WAL is opt-in, and a handle that opted into it checkpoints and resets
  the file to `DELETE` on `close` and on drop, so a handed-over `.gpkg` carries
  no `-wal`/`-shm` sidecars.
- **`geopackage-core`.** No-IO spec layer: GPB header codec, column and geometry
  type vocabulary, `DATE`/`DATETIME` parsing, normative DDL and
  `gpkg_spatial_ref_sys` seed rows, version-aware RTree trigger SQL,
  `application_id`/`user_version` handling, and SQL identifier quoting.

### Validation

- OGC `ets-gpkg12`: 40 passed, 71 not applicable, 1 failed. The failure's regex
  hard-codes the GeoPackage 1.2 `update1` trigger and rejects the correct 1.4
  set. No 1.3/1.4 ETS exists; the 1.4 trigger semantics are covered by a manual
  checklist.
- PDOK `geopackage-validator` 0.14.4: 21 checks, clean but for two advisory
  findings on deliberate choices (a mixed-SRS test file, an intentional Z layer).
- GDAL round-trip: geometry WKB bodies and every attribute value byte-identical
  after an `ogr2ogr` copy; `ogrinfo` reads all layers cleanly.
- Fixture corpus covering GDAL-written, QGIS-written and raw-SQLite files, plus
  property tests proving the RTree matches a full-scan rebuild after arbitrary
  write sequences on both build paths.

### Known limitations

- The `wkb` 0.9.2 reader pre-allocates from unbounded element counts, so a
  malformed geometry can drive a multi-gigabyte allocation; do not parse
  untrusted files with this release ([#3]).
- Bulk indexed writes are roughly 3-4x slower than an indexed `ogr2ogr` copy at
  1M rows ([#15], [#16]). The cost attributed here at release time was wrong:
  profiling afterwards put 4.61s of the 7.26s in the scratch RTree build, not in
  the gate's `integrity_check` (0.97s) or the `ST_*` envelope scan (0.26s). Much
  of this is addressed in 0.1.1; the remainder is [#20].
- Non-linear curve types cannot have envelopes computed and so cannot be
  inserted into an indexed table ([#5]).
- Feature iteration materialises the result set rather than streaming ([#4]).

[Unreleased]: https://github.com/urschrei/geopackage/compare/v0.6.0...HEAD
[0.6.0]: https://github.com/urschrei/geopackage/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/urschrei/geopackage/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/urschrei/geopackage/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/urschrei/geopackage/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/urschrei/geopackage/compare/v0.1.2...v0.2.0
[0.1.2]: https://github.com/urschrei/geopackage/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/urschrei/geopackage/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/urschrei/geopackage/releases/tag/v0.1.0
[#1]: https://github.com/urschrei/geopackage/issues/1
[#26]: https://github.com/urschrei/geopackage/issues/26
[#3]: https://github.com/urschrei/geopackage/issues/3
[#4]: https://github.com/urschrei/geopackage/issues/4
[#5]: https://github.com/urschrei/geopackage/issues/5
[#15]: https://github.com/urschrei/geopackage/issues/15
[#16]: https://github.com/urschrei/geopackage/issues/16
[#17]: https://github.com/urschrei/geopackage/issues/17
[#20]: https://github.com/urschrei/geopackage/issues/20
[#22]: https://github.com/urschrei/geopackage/pull/22
[#23]: https://github.com/urschrei/geopackage/issues/23
[#24]: https://github.com/urschrei/geopackage/issues/24
