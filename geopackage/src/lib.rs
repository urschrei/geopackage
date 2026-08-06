//! Read and write [OGC GeoPackage](https://www.geopackage.org/spec140/) files.
//!
//! A GeoPackage is an SQLite database with a standardised schema for vector
//! features and raster tiles. [`GeoPackage::create`], [`GeoPackage::open`] and
//! [`GeoPackage::open_read_only`] open the container with pragma and schema
//! validation;
//! [`GeoPackage::layer`] returns a [`Layer`] handle for reading and writing
//! features, and [`GeoPackage::layers`] enumerates them.
//! [`GeoPackage::tiles`] returns a [`TilePyramid`] handle for the tile side,
//! described under [Tiles](#tiles) below.
//!
//! A command-line companion, `gpkg`, is built by the `geopackage-cli` crate:
//! `gpkg info`, `gpkg validate`, `gpkg index`, `gpkg repair`, `gpkg copy` and
//! `gpkg tiles` inspect, check and convert files without writing any code.
//! Install it with `cargo install geopackage-cli`; unlike this library it
//! vendors SQLite by default (see [Cargo features](#cargo-features)).
//!
//! # Quick start
//!
//! Create a file, define a point layer, write features, query by bounding box:
//!
//! ```
//! use geo_types::Point;
//! use geopackage::core::types::{ColumnType, GeometryType};
//! use geopackage::{
//!     BoundingBox, ColumnSpec, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder, Value,
//!     ValueRef,
//! };
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let dir = tempfile::tempdir()?;
//! # let path = dir.path().join("cities.gpkg");
//! let gpkg = GeoPackage::create(path)?;
//!
//! gpkg.create_layer(
//!     &TableSchemaBuilder::new("cities")
//!         .column(ColumnSpec::new("name", ColumnType::Text(None)))
//!         .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
//! )?;
//!
//! let layer = gpkg.layer("cities")?;
//! layer.write_all(
//!     vec![
//!         NewFeature::new(Point::new(-6.26, 53.35), vec![Value::Text("Dublin".into())]),
//!         NewFeature::new(Point::new(-0.13, 51.51), vec![Value::Text("London".into())]),
//!     ],
//!     0,
//! )?;
//!
//! // Served by the layer's RTree index, which `create_layer` builds unless
//! // `TableSchemaBuilder::spatial_index(false)` turns it off.
//! for feature in layer.features_in(BoundingBox::new(-7.0, 53.0, -6.0, 54.0))? {
//!     let feature = feature?;
//!     assert_eq!(feature.value("name"), Some(ValueRef::Text("Dublin")));
//! }
//! # Ok(()) }
//! ```
//!
//! # Features
//!
//! - [`Layer::features`]: every row as an owned [`Feature`]
//! - [`Layer::select`]: rows matching a caller-supplied raw SQL `WHERE` clause
//! - [`Layer::features_in`]: rows in a bounding box, served by the RTree index
//!   when one is present and a full scan otherwise, with identical results
//! - [`Layer::cursor`], [`Layer::cursor_select`], [`Layer::cursor_in`]:
//!   streaming counterparts that read one row at a time and hold no result set
//! - [`Layer::write_all`]: batch load
//! - [`Layer::writer`]: a transaction with per-row `insert`/`update`/`delete`
//!   ([`FeatureWriter`])
//! - [`GeoPackage::create_attributes_table`], [`GeoPackage::attributes`]: the
//!   same for non-spatial attribute tables
//! - [`GeoPackage::add_epsg_srs`]: registers an EPSG code in
//!   `gpkg_spatial_ref_sys`
//! - [`GeoPackage::open_lenient`]: accepts legacy and lightly malformed files,
//!   collecting [`OpenWarning`]s instead of failing
//!
//! # Geometry round trips
//!
//! [`Feature::geometry`] parses the stored blob into a
//! [`GpbGeometry`](core::GpbGeometry), a view over the row's bytes that
//! implements [`geo_traits::GeometryTrait`], and every write method accepts
//! any `impl GeometryTrait<T = f64>`. A geometry can therefore be streamed
//! out of one file, measured, and written into another without being
//! converted to a `geo-types` value in either direction: the analysis reads
//! coordinates from the stored encoding, and the writer encodes WKB from the
//! same view. What _does_ allocate is each row's blob, copied out of SQLite,
//! and the new blob the writer serialises; an algorithm that produces new
//! geometry also allocates its output.
//!
//! ```
//! use geo_traits::{CoordTrait, GeometryTrait, GeometryType as Kind, LineStringTrait};
//! use geopackage::core::types::{ColumnType, GeometryType};
//! use geopackage::{ColumnSpec, GeoPackage, GeometrySpec, TableSchemaBuilder, Value, ValueRef};
//!
//! /// Planar length, read from the trait: no geometry object is built.
//! fn length(geometry: &impl GeometryTrait<T = f64>) -> f64 {
//!     let Kind::LineString(line) = geometry.as_type() else {
//!         return 0.0;
//!     };
//!     let mut sum = 0.0;
//!     let mut prev: Option<(f64, f64)> = None;
//!     for coord in line.coords() {
//!         let (x, y) = (coord.x(), coord.y());
//!         if let Some((px, py)) = prev {
//!             sum += ((x - px).powi(2) + (y - py).powi(2)).sqrt();
//!         }
//!         prev = Some((x, y));
//!     }
//!     sum
//! }
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let dir = tempfile::tempdir()?;
//! # let src_path = dir.path().join("roads.gpkg");
//! # let dst_path = dir.path().join("measured.gpkg");
//! # {
//! #     let src = GeoPackage::create(&src_path)?;
//! #     src.create_layer(
//! #         &TableSchemaBuilder::new("roads")
//! #             .geometry(GeometrySpec::new(GeometryType::LineString, 4326)),
//! #     )?;
//! #     src.layer("roads")?.write_all(
//! #         vec![
//! #             geopackage::NewFeature::new(
//! #                 geo_types::LineString::from(vec![(0.0, 0.0), (3.0, 4.0)]),
//! #                 vec![],
//! #             ),
//! #             geopackage::NewFeature::new(
//! #                 geo_types::LineString::from(vec![(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)]),
//! #                 vec![],
//! #             ),
//! #         ],
//! #         0,
//! #     )?;
//! # }
//! let src = GeoPackage::open_read_only(&src_path)?;
//! let dst = GeoPackage::create(&dst_path)?;
//! dst.create_layer(
//!     &TableSchemaBuilder::new("measured")
//!         .column(ColumnSpec::new("length", ColumnType::Double))
//!         .geometry(GeometrySpec::new(GeometryType::LineString, 4326)),
//! )?;
//!
//! let roads = src.layer("roads")?;
//! let measured = dst.layer("measured")?;
//! let mut writer = measured.writer()?;
//! let mut cursor = roads.cursor()?;
//! for feature in cursor.features()? {
//!     let feature = feature?;
//!     if let Some(geometry) = feature.geometry()? {
//!         // `geometry` borrows the row's blob; `length` reads coordinates
//!         // from it, and `insert` encodes WKB from the same view.
//!         let l = length(&geometry);
//!         writer.insert(None, &geometry, &[ValueRef::Float(l)])?;
//!     }
//! }
//! writer.commit()?;
//!
//! let mut total = 0.0;
//! for feature in measured.features()? {
//!     if let Some(l) = feature?.value("length").and_then(|v| v.as_f64()) {
//!         total += l;
//!     }
//! }
//! assert_eq!(total, 7.0);
//! # Ok(()) }
//! ```
//!
//! # Tiles
//!
//! A tile pyramid is the container's other data type: pre-rendered raster
//! tiles, addressed by zoom level, column and row, with a
//! `gpkg_tile_matrix_set` row fixing the ground extent they are indexed
//! against and a `gpkg_tile_matrix` row per zoom level.
//! [`GeoPackage::create_tile_pyramid`] writes one (from a
//! [`TilePyramidBuilder`]), [`GeoPackage::tiles`] opens one, and
//! [`GeoPackage::tile_pyramids`] enumerates them.
//!
//! **Payloads are opaque.** This crate stores, indexes and validates tiles; it
//! decodes none of them, and depends on no image codec. It reads each
//! payload's *header*, which is how a tile written at the wrong pixel size, or
//! in a format the table may not contain, is rejected rather than stored. Turning
//! a tile into pixels, or a source raster into a pyramid, needs an image
//! library or GDAL on top of this one.
//!
//! Rows count from the **top** of the extent downwards, as WMTS and XYZ do and
//! TMS does not, and the indices are relative to the pyramid's own extent
//! rather than to a global grid. [`TileMatrix::flip_row`](core::tiles::TileMatrix::flip_row)
//! converts to and from the TMS sense, and
//! [`TileMatrixSet::xyz_to_tile`](core::tiles::TileMatrixSet::xyz_to_tile)
//! errors rather than mis-addressing when a pyramid is not the standard web
//! mercator quad.
//!
//! ```
//! use geopackage::core::tiles::{TileCoord, TileMatrixSet, ZoomLadder};
//! use geopackage::{GeoPackage, TilePyramidBuilder};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let dir = tempfile::tempdir()?;
//! # let path = dir.path().join("basemap.gpkg");
//! # let png = |w: u32, h: u32| {
//! #     let mut b = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
//! #     b.extend_from_slice(&13u32.to_be_bytes());
//! #     b.extend_from_slice(b"IHDR");
//! #     b.extend_from_slice(&w.to_be_bytes());
//! #     b.extend_from_slice(&h.to_be_bytes());
//! #     b.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
//! #     b
//! # };
//! let gpkg = GeoPackage::create(path)?;
//! gpkg.add_epsg_srs(3857)?;
//!
//! // The spec's default arrangement: each zoom level doubles the grid, with
//! // pixel sizes derived from the extent so they span it exactly.
//! let matrix_set = TileMatrixSet::web_mercator_quad();
//! let matrices = matrix_set.ladder(ZoomLadder::new(0, 4))?;
//! let tiles = gpkg.create_tile_pyramid(
//!     &TilePyramidBuilder::new("basemap", matrix_set).matrices(matrices),
//! )?;
//!
//! tiles.put_tile(TileCoord::new(1, 0, 0), &png(256, 256))?;
//! assert!(tiles.get_tile(TileCoord::new(1, 0, 0))?.is_some());
//!
//! // Streaming a pyramid borrows each payload from the row it was read
//! // from, so nothing is copied to walk one.
//! let mut cursor = tiles.cursor()?;
//! let mut stream = cursor.tiles()?;
//! while let Some(tile) = stream.next()? {
//!     assert_eq!(tile.data().len(), 33);
//! }
//! # Ok(()) }
//! ```
//!
//! # Columnar I/O
//!
//! Enabled by the `arrow` feature, [`Layer::read_arrow`] reads a layer as Arrow
//! record batches, multithreaded by default, and [`Layer::write_arrow`]
//! writes batches back through the same path as [`Layer::write_all`].
//! Geometry is a GeoArrow WKB column whose metadata includes the CRS as
//! PROJJSON. [`TableSchemaBuilder::from_arrow_schema`] is the layer
//! definition an Arrow schema implies, so a layer can be copied without its
//! schema being restated; the type mapping both directions share is
//! documented on the [`arrow`] module.
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # #[cfg(feature = "arrow")]
//! # {
//! # use geo_types::Point;
//! # use geopackage::core::types::{ColumnType, GeometryType};
//! # use geopackage::{ColumnSpec, GeoPackage, GeometrySpec, NewFeature, TableSchemaBuilder, Value};
//! # let dir = tempfile::tempdir()?;
//! # let src_path = dir.path().join("cities.gpkg");
//! # let dst_path = dir.path().join("copy.gpkg");
//! # {
//! #     let src = GeoPackage::create(&src_path)?;
//! #     src.create_layer(
//! #         &TableSchemaBuilder::new("cities")
//! #             .column(ColumnSpec::new("name", ColumnType::Text(None)))
//! #             .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
//! #     )?;
//! #     src.layer("cities")?.write_all(
//! #         vec![
//! #             NewFeature::new(Point::new(-6.26, 53.35), vec![Value::Text("Dublin".into())]),
//! #             NewFeature::new(Point::new(-0.13, 51.51), vec![Value::Text("London".into())]),
//! #         ],
//! #         0,
//! #     )?;
//! # }
//! use geopackage::arrow::ArrowReadOptions;
//!
//! let src = GeoPackage::open_read_only(&src_path)?;
//! let cities = src.layer("cities")?;
//!
//! let dst = GeoPackage::create(&dst_path)?;
//! let schema = cities.arrow_schema()?;
//! dst.create_layer(&TableSchemaBuilder::new("cities").from_arrow_schema(&schema)?)?;
//!
//! let batches = cities.read_arrow(ArrowReadOptions::default())?;
//! dst.layer("cities")?.write_arrow(batches, 0)?;
//! assert_eq!(dst.layer("cities")?.features()?.len(), 2);
//! # }
//! # Ok(()) }
//! ```
//!
//! # Extensions
//!
//! `gpkg_extensions` is where a file declares what it uses beyond the core
//! spec. [`GeoPackage::extensions`] reads that catalogue, and
//! [`Layer::extensions`] and [`TilePyramid::extensions`] narrow it to one
//! table. Every row identifies as an [`Extension`] and has an
//! [`ExtensionSupport`]: read and written here, identified and left alone,
//! removed from the standard in 2016 and accepted on read, or not recognised
//! at all.
//!
//! That last one is not only informational. Writing to a table covered by an
//! extension this crate cannot identify fails with
//! [`Error::UnsupportedExtension`], because such an extension may constrain
//! the rows, triggers or encodings of the table it covers, and writing beside
//! it could produce a file its own producer can no longer read. Reading never
//! fails for this reason. [`GeoPackage::blocking_extension`] asks the
//! question directly, and
//! [`OpenOptions::allow_unsupported_extension_writes`] disables the check.
//!
//! Two extensions are surfaced as part of the model rather than as catalogue
//! rows. `gpkg_crs_wkt` puts a WKT2 CRS definition and a coordinate epoch on
//! [`Srs`], which is how a CRS with no WKT1 form is represented at all.
//! `gpkg_schema` describes columns and constrains their values:
//! [`GeoPackage::data_columns`] and [`Column::data_column`] give the
//! descriptions, [`GeoPackage::column_constraint`] resolves what a column's
//! values are limited to, and [`GeoPackage::set_data_column`] and
//! [`GeoPackage::add_column_constraint`] write them.
//!
//! ```
//! use geopackage::{ColumnConstraint, ConstraintKind, DataColumn, GeoPackage, OpenOptions};
//! # use geopackage::core::types::{ColumnType, GeometryType};
//! # use geopackage::{ColumnSpec, GeometrySpec, TableSchemaBuilder};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let dir = tempfile::tempdir()?;
//! # let path = dir.path().join("sites.gpkg");
//! # {
//! # let gpkg = GeoPackage::create(&path)?;
//! # gpkg.create_layer(
//! #     &TableSchemaBuilder::new("sites")
//! #         .column(ColumnSpec::new("year", ColumnType::Integer))
//! #         .geometry(GeometrySpec::new(GeometryType::Point, 4326)),
//! # )?;
//! let gpkg = GeoPackage::open(&path)?;
//! gpkg.add_column_constraint(&ColumnConstraint {
//!     name: "years".into(),
//!     kind: ConstraintKind::Range {
//!         min: 1900.0,
//!         min_is_inclusive: true,
//!         max: 2000.0,
//!         max_is_inclusive: false,
//!     },
//!     description: None,
//! })?;
//! gpkg.set_data_column(
//!     "sites",
//!     &DataColumn {
//!         column_name: "year".into(),
//!         name: Some("Year surveyed".into()),
//!         title: None,
//!         description: None,
//!         mime_type: None,
//!         constraint_name: Some("years".into()),
//!     },
//! )?;
//! # }
//!
//! // The constraints are advisory in the format, so checking written values
//! // against them is opt-in rather than assumed.
//! let gpkg = OpenOptions::new()
//!     .enforce_column_constraints(true)
//!     .open(&path)?;
//! # let layer = gpkg.layer("sites")?;
//! # let mut writer = layer.writer()?;
//! # use geopackage::ValueRef;
//! # assert!(writer.insert(None, &geo_types::Point::new(0.0, 0.0), &[ValueRef::Integer(1850)]).is_err());
//! # Ok(()) }
//! ```
//!
//! # Cargo features
//!
//! - **`geo-types`** (on by default): forwards `geopackage-core`'s feature of
//!   the same name, which adds
//!   [`GpbGeometry::to_geo`](geopackage_core::geometry::GpbGeometry::to_geo).
//!   Disable it with `default-features = false`.
//! - **`arrow`** (off by default): the columnar paths above. It pulls in
//!   `arrow-array` and `arrow-schema`, which a caller using only the scalar API
//!   does not need.
//! - **`bundled`** (off by default): compile a vendored SQLite amalgamation
//!   and link it statically, instead of linking the system SQLite. The
//!   default, system-linked build requires the development files
//!   (`libsqlite3-dev` on Debian/Ubuntu; the macOS SDK suffices) and an
//!   `SQLITE_ENABLE_RTREE` build, which every open checks
//!   ([`Error::RtreeUnavailable`]). `bundled` needs a C compiler, always has
//!   the RTree module, and is the only option on Windows, which has no system
//!   SQLite.
//!
//! # Configuration
//!
//! The defaults are intended to support the common case: a single-file GeoPackage, an indexed
//! feature layer, and values read in keeping with other popular implementations.
//! Each of these types documents possible trade-off behind its defaults:
//!
//! - [`OpenOptions`]: the journal mode ([`JournalMode`], where
//!   [`JournalMode::Wal`] is opt-in), the `synchronous` durability level
//!   ([`Synchronous`]), and how long a statement waits for another
//!   connection's lock ([`OpenOptions::busy_timeout`], default
//!   [`DEFAULT_BUSY_TIMEOUT`], five seconds, against SQLite's own default of
//!   not waiting at all). Left unset, the file keeps SQLite's own defaults for
//!   the first two. A
//!   handle that opted into WAL resets the file to a single `DELETE`-journal
//!   file on close, so the `.gpkg` handed on has no sidecar files; see
//!   [`GeoPackage`].
//! - [`TableSchemaBuilder`]: a new layer's columns ([`ColumnSpec`]), primary
//!   key (default [`DEFAULT_PRIMARY_KEY`], `fid`), geometry column
//!   ([`GeometrySpec`], named [`DEFAULT_GEOMETRY_COLUMN`], `geom`, unless told
//!   otherwise), and whether it is indexed
//!   ([`TableSchemaBuilder::spatial_index`], default `true`).
//!   [`Layer::create_spatial_index`], [`Layer::drop_spatial_index`],
//!   [`Layer::repair_spatial_index`], [`Layer::audit_spatial_index`] and
//!   [`Layer::rebuild_spatial_index`] manage the index after creation.
//! - [`BulkIndexOptions`]: how an RTree index is built, for
//!   [`Layer::create_spatial_index_with`] and [`Layer::write_all_with`]: the
//!   row count at which the bulk build takes over from the per-row triggers
//!   ([`BulkIndexOptions::bulk_threshold`], default [`DEFAULT_BULK_THRESHOLD`],
//!   10,000 rows), how much of the result it checks before trusting it
//!   ([`BulkVerification`], default [`BulkVerification::None`]), and how full
//!   each node of the tree is
//!   packed ([`BulkIndexOptions::fill_factor`], default
//!   [`DEFAULT_FILL_FACTOR`], `1.0`).
//! - [`TilePyramidBuilder`]: a new pyramid's extent and spatial reference
//!   system ([`TileMatrixSet`](core::tiles::TileMatrixSet)), its zoom levels
//!   ([`TileMatrix`](core::tiles::TileMatrix), usually from
//!   [`TileMatrixSet::ladder`](core::tiles::TileMatrixSet::ladder)), and
//!   whether zoom levels that do not step by factors of two are allowed
//!   ([`TilePyramidBuilder::allow_zoom_other`], off by default, since that
//!   needs the `gpkg_zoom_other` extension registered).
//! - **Column projection**, through [`Layer::with_columns`] and
//!   [`Layer::without_geometry`]: which columns a read of that handle
//!   fetches. Everything, by default. Worth setting on a layer with large
//!   geometries when only the attributes are needed, since the geometry is
//!   otherwise fetched and copied into every row whether or not anything
//!   reads it.
//! - [`ConversionOptions`]: how stored values are read back, through
//!   [`Layer::with_conversion_options`]: which `DATETIME` text forms are
//!   accepted ([`DateTimeParsing`], default [`DateTimeParsing::Strict`]) and
//!   whether a value its declared type does not strictly permit is read or
//!   rejected ([`StorageStrictness`], default [`StorageStrictness::Lenient`]).
//!
//! Two settings are available outside the options types. [`Layer::write_all`] and
//! [`Layer::write_arrow`] take a `batch_size`, the number of rows sharing a
//! transaction, where `0` writes all of them in one.
//! [`Layer::with_geometry_type_validation`] checks each geometry against its
//! column's declared type while reading, and is **off** by default.
//!
//! Under the `arrow` feature, [`ArrowReadOptions`](arrow::ArrowReadOptions)
//! configures the columnar read: rows per batch
//! ([`batch_size`](arrow::ArrowReadOptions::batch_size), default
//! [`DEFAULT_BATCH_SIZE`](arrow::DEFAULT_BATCH_SIZE), 65,536), how many
//! threads may read at once ([`threads`](arrow::ArrowReadOptions::threads),
//! default `0`, meaning `min(4, available parallelism)`), and a ceiling on
//! the geometry bytes one batch may hold
//! ([`max_batch_bytes`](arrow::ArrowReadOptions::max_batch_bytes), default
//! [`default_max_batch_bytes`](arrow::default_max_batch_bytes),
//! `min(INT32_MAX, RAM / 4)`): the geometry column's Arrow offsets are
//! 32-bit, so no batch can address more than 2 GB of WKB, and a batch that
//! would cross the ceiling is emitted short so a layer of very large
//! geometries still reads. The columnar write has no options type of its own:
//! [`Layer::write_arrow_with`] takes the same [`BulkIndexOptions`] as
//! [`Layer::write_all_with`].
//!
//! Anything not covered here is reachable as SQL: [`GeoPackage::connection`]
//! returns the underlying rusqlite connection.
//!
//! # What writes, and when
//!
//! Most of this crate divides cleanly into reads and writes, but three calls do
//! not, so the whole surface is tabulated here rather than left to be inferred
//! from the names. [`Layer::extent`] records what it had to measure;
//! [`Layer::repair_spatial_index`] writes only when there is something to
//! repair; and [`Layer::writer`] opens without writing, because SQLite's
//! `BEGIN DEFERRED` takes no lock, so the first failure lands on the first row.
//!
//! | Call | Writes to the file | On a read-only connection |
//! |---|---|---|
//! | [`Layer::features`], [`Layer::cursor`], [`Layer::features_in`], [`Layer::select`] | never | works |
//! | [`Layer::spatial_index_status`], [`Layer::has_spatial_index`] | never | works |
//! | [`Layer::audit_spatial_index`] | never | works |
//! | [`GeoPackage::contents`] | never | works |
//! | [`Layer::extent`] | only where the recorded bounds are unusable | works: measures, returns, records nothing |
//! | [`Layer::repair_spatial_index`] | only where the trigger set is not current | works where there is nothing to repair |
//! | [`Layer::recompute_extent`] | always | fails |
//! | [`Layer::create_spatial_index`], [`Layer::drop_spatial_index`], [`Layer::rebuild_spatial_index`] | always | fails |
//! | [`Layer::writer`] | on its row methods and its commit, not on the call | opens; the first row written fails |
//! | [`Layer::write_all`], [`Layer::write_arrow`] | always | fails |
//! | [`GeoPackage::tiles`], [`TilePyramid::get_tile`], [`TilePyramid::cursor`] | never | works |
//! | [`TilePyramid::validate`] | never | works |
//! | [`TilePyramid::put_tile`], [`TilePyramid::delete_tile`], [`TilePyramid::write_all`] | always | fails |
//! | [`TilePyramid::writer`] | on its tile methods and its commit, not on the call | opens; the first tile written fails |
//!
//! Reading an extent therefore modifies the file when the recorded bounds are
//! unusable, which is deliberate: the file stops being wrong for every later
//! reader rather than only for this one. [`Layer::extent`] documents why, and
//! the two ways to avoid it.
//!
//! ## What can fail, and why
//!
//! - **A read-only connection**, per the table: [`Error::Sqlite`] with
//!   SQLite's `SQLITE_READONLY`. [`Layer::extent`] is the exception, since it
//!   has an answer either way.
//! - **Another connection holding the write lock**: the statement waits up to
//!   [`OpenOptions::busy_timeout`] and then fails with `SQLITE_BUSY`. Again
//!   [`Layer::extent`] is the exception: contention means the measurement
//!   describes a layer being changed underneath it, so the file keeps what it
//!   had and the measurement is returned rather than an error. Note that SQLite
//!   skips the wait entirely for a read-to-write upgrade that would deadlock
//!   under a rollback journal, and for a stale snapshot under WAL.
//! - **No spatial index**: [`Error::NoSpatialIndex`] from
//!   [`Layer::audit_spatial_index`] and [`Layer::rebuild_spatial_index`], and
//!   from [`Layer::repair_spatial_index`] when there is nothing there at all.
//! - **No geometry column**: [`Error::NoGeometryColumn`] from
//!   [`Layer::extent`], [`Layer::recompute_extent`], [`Layer::features_in`] and
//!   [`Layer::cursor_in`].
//! - **A store that cannot be written for any other reason**, an unwritable
//!   directory, a full disk, an I/O error: [`Error::ExtentPersist`] from
//!   [`Layer::extent`], which includes the measurement so the answer is not
//!   lost with the failure, and [`Error::Sqlite`] from everything else.
//!
//! # Reading untrusted files
//!
//! The `wkb` 0.9.2 reader this crate parses geometry with pre-allocates from
//! element counts read out of the blob without bounding them against the
//! buffer, so a malformed geometry declaring a `0xFFFFFFFF`-member collection
//! drives a multi-gigabyte allocation. The fix belongs upstream in
//! [georust/wkb](https://github.com/georust/wkb); until it lands and this crate
//! bumps its dependency, you should take care when parsing GeoPackage files from untrusted sources.

// `unsafe_code = "forbid"` and `missing_docs = "warn"` come from the
// workspace lints table (root Cargo.toml). This crate never uses `unsafe`; the
// `geopackage-ffi` crate is the sole exception, and opts out of the workspace
// lints rather than relaxing them here.

#[cfg(feature = "arrow")]
pub mod arrow;
#[cfg(doctest)]
mod book;
mod bulk;
mod create;
mod data_columns;
mod error;
pub mod extensions;
mod extent;
mod functions;
mod index;
mod layer;
mod metadata;
mod open;
mod options;
mod packed;
mod related;
mod schema;
mod srs;
mod tiles;
mod transaction;
mod validate;
mod value;
mod writer;

pub use bulk::{BulkIndexOptions, BulkVerification, DEFAULT_BULK_THRESHOLD, DEFAULT_FILL_FACTOR};
pub use create::{
    ColumnSpec, DEFAULT_GEOMETRY_COLUMN, DEFAULT_PRIMARY_KEY, GeometrySpec, TableSchemaBuilder,
};
pub use error::{Error, Result};
pub use extensions::ExtensionRow;
pub use geopackage_core as core;
pub use geopackage_core::GpkgVersion;
pub use geopackage_core::extensions::{Extension, ExtensionScope, ExtensionSupport};
pub use geopackage_core::metadata::{
    MetadataRecord, MetadataReference, MetadataScope, ReferenceScope,
};
pub use geopackage_core::related::{Relation, RelationName};
pub use geopackage_core::schema::{ColumnConstraint, ConstraintKind, DataColumn};
pub use index::{SpatialIndexAudit, SpatialIndexStatus};
pub use layer::{BoundingBox, Feature, FeatureCursor, FeatureStream, Features, Layer, LayerKind};
pub use metadata::{MetadataTarget, NewMetadata};
pub use open::OpenWarning;
pub use options::{DEFAULT_BUSY_TIMEOUT, JournalMode, OpenOptions, Synchronous};
pub use related::NewRelation;
pub use schema::{Column, GeometryColumn, TableSchema};
pub use srs::Srs;
pub use tiles::{Tile, TileCursor, TilePyramid, TilePyramidBuilder, TileStream, TileWriter};
pub use validate::{Finding, Severity};
pub use value::{ConversionOptions, DateTimeParsing, StorageStrictness, Value, ValueRef};
pub use writer::{FeatureWriter, NewFeature};

use geopackage_core::{ddl, version};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::path::Path;

/// An open GeoPackage.
///
/// # Interchange-first close
///
/// A handle opened or created in [`JournalMode::Wal`] creates `-wal`/`-shm`
/// sidecar files while it is live. On [`GeoPackage::close`] and on drop such a
/// handle checkpoints the WAL and resets the file to [`JournalMode::Delete`], so
/// the resulting`.gpkg` is a single file. Prefer the explicit
/// [`GeoPackage::close`], which surfaces any error; the drop path is
/// best-effort and never panics. [`GeoPackage::into_connection`] opts out of
/// this guarantee: the returned connection keeps whatever journal mode it was
/// in.
pub struct GeoPackage {
    /// `Some` for the whole lifetime of the handle; taken only by
    /// [`Self::into_connection`], which is why the handle can implement `Drop`.
    conn: Option<Connection>,
    version: GpkgVersion,
    warnings: Vec<OpenWarning>,
    /// The journal mode this handle is responsible for finalising: `Wal` only
    /// when it opted into WAL and must reset the file to `Delete` on close/drop.
    journal_mode: JournalMode,
    /// Whether writes proceed to tables covered by an extension this crate
    /// cannot identify. See
    /// [`OpenOptions::allow_unsupported_extension_writes`].
    allow_unsupported_extension_writes: bool,
    /// Whether written values are checked against the `gpkg_schema`
    /// constraints their columns declare. See
    /// [`OpenOptions::enforce_column_constraints`].
    enforce_column_constraints: bool,
}

impl GeoPackage {
    /// Creates a new GeoPackage 1.4 file at `path` with default options
    /// ([`JournalMode::Delete`], SQLite's default `synchronous`).
    ///
    /// Use [`OpenOptions`] to create in [`JournalMode::Wal`] or with an
    /// explicit [`Synchronous`] level.
    ///
    /// # Errors
    ///
    /// - [`Error::AlreadyExists`] if `path` already exists and is non-empty.
    /// - [`Error::RtreeUnavailable`] if the linked SQLite lacks the RTree
    ///   module (possible only when linking a system SQLite; the `bundled`
    ///   feature always includes it). Every open path checks this.
    pub fn create<P: AsRef<Path>>(path: P) -> Result<Self> {
        OpenOptions::new().create(path)
    }

    /// Opens an existing GeoPackage read-write with default options.
    ///
    /// Use [`OpenOptions`] to select a journal mode or `synchronous` level.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        OpenOptions::new().open(path)
    }

    /// Opens an existing GeoPackage read-only with default options.
    pub fn open_read_only<P: AsRef<Path>>(path: P) -> Result<Self> {
        OpenOptions::new().open_read_only(path)
    }

    /// Wraps an already-open connection with default options, validating that
    /// it is a GeoPackage and registering the required SQL functions.
    pub fn from_connection(conn: Connection) -> Result<Self> {
        // A caller-supplied connection is left in whatever journal mode it
        // already has (no journal pragma applied).
        Self::from_connection_configured(conn, OpenOptions::new(), false)
    }

    /// The `create` core, applying `options` before seeding the schema.
    pub(crate) fn create_configured(path: &Path, options: OpenOptions) -> Result<Self> {
        if path.exists()
            && std::fs::metadata(path)
                .map(|m| m.len() > 0)
                .unwrap_or(false)
        {
            return Err(Error::AlreadyExists(path.to_owned()));
        }
        let options = options.with_default_busy_timeout();
        let allow_unsupported_extension_writes = options.allow_unsupported_extension_writes;
        let enforce_column_constraints = options.enforce_column_constraints;
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "application_id", version::APPLICATION_ID_GPKG)?;
        conn.pragma_update(
            None,
            "user_version",
            GpkgVersion::V1_4
                .user_version()
                .expect("1.4 has a user_version"),
        )?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let journal_mode = apply_open_options(&conn, options, true)?;
        // The one write path that opens its transaction outright rather than
        // through `WriteTransaction`. The connection was opened a few lines
        // above and has not left this function, so no caller can have a
        // transaction on it and there is nothing to inherit.
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(&format!(
            "{};\n{};",
            ddl::CREATE_GPKG_SPATIAL_REF_SYS,
            ddl::CREATE_GPKG_CONTENTS
        ))?;
        for stmt in ddl::SEED_SPATIAL_REF_SYS {
            tx.execute(stmt, [])?;
        }
        tx.commit()?;
        ensure_rtree_available(&conn)?;
        functions::register(&conn)?;
        Ok(Self {
            conn: Some(conn),
            version: GpkgVersion::V1_4,
            warnings: Vec::new(),
            journal_mode,
            allow_unsupported_extension_writes,
            enforce_column_constraints,
        })
    }

    /// The `open`/`open_read_only` core, applying `options` after opening.
    pub(crate) fn open_configured(
        path: &Path,
        flags: OpenFlags,
        options: OpenOptions,
    ) -> Result<Self> {
        let options = options.with_default_busy_timeout();
        let conn = Connection::open_with_flags(path, flags)?;
        // A read-only connection cannot change its journal mode.
        let apply_journal = flags.contains(OpenFlags::SQLITE_OPEN_READ_WRITE);
        Self::from_connection_configured(conn, options, apply_journal)
    }

    /// Validates a connection as a GeoPackage, applies `options`, and
    /// registers the SQL functions. `apply_journal` is false for a read-only or
    /// caller-supplied connection whose journal mode must be left untouched.
    pub(crate) fn from_connection_configured(
        conn: Connection,
        options: OpenOptions,
        apply_journal: bool,
    ) -> Result<Self> {
        let application_id = read_header_u32(&conn, "application_id")?;
        let user_version = read_header_u32(&conn, "user_version")?;
        let version = GpkgVersion::from_pragmas(application_id, user_version).ok_or(
            Error::NotAGeoPackage {
                reason: "unrecognized application_id/user_version",
                application_id,
                user_version,
            },
        )?;
        for required in ["gpkg_spatial_ref_sys", "gpkg_contents"] {
            if !table_exists(&conn, required)? {
                return Err(Error::NotAGeoPackage {
                    reason: "missing required core table",
                    application_id,
                    user_version,
                });
            }
        }
        let allow_unsupported_extension_writes = options.allow_unsupported_extension_writes;
        let enforce_column_constraints = options.enforce_column_constraints;
        // Collected before the pragmas are applied, so a warning describes the
        // file as it was found rather than as this handle left it.
        let warnings = if options.lenient {
            crate::open::collect_warnings(&conn, application_id, version)?
        } else {
            Vec::new()
        };
        let journal_mode = apply_open_options(&conn, options, apply_journal)?;
        ensure_rtree_available(&conn)?;
        functions::register(&conn)?;
        Ok(Self {
            conn: Some(conn),
            version,
            warnings,
            journal_mode,
            allow_unsupported_extension_writes,
            enforce_column_constraints,
        })
    }

    /// Returns the spec version declared by the file's pragmas.
    pub fn version(&self) -> GpkgVersion {
        self.version
    }

    /// Returns the rows of `gpkg_contents`.
    pub fn contents(&self) -> Result<Vec<ContentsEntry>> {
        let mut stmt = self.connection().prepare(
            "SELECT table_name, data_type, identifier, srs_id, min_x, min_y, max_x, max_y \
             FROM gpkg_contents ORDER BY table_name",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ContentsEntry {
                table_name: r.get(0)?,
                data_type: ContentsDataType::from_str(&r.get::<_, String>(1)?),
                identifier: r.get(2)?,
                srs_id: r.get(3)?,
                min_x: r.get(4)?,
                min_y: r.get(5)?,
                max_x: r.get(6)?,
                max_y: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Borrows the underlying connection (escape hatch: full SQL access).
    pub fn connection(&self) -> &Connection {
        self.conn
            .as_ref()
            .expect("connection is present for the whole handle lifetime")
    }

    /// Consumes the handle, returning the underlying connection.
    ///
    /// This **opts out** of the interchange-first close guarantee: the returned
    /// connection keeps whatever journal mode it is in, so a handle that was in
    /// [`JournalMode::Wal`] returns a WAL connection with its `-wal`/`-shm`
    /// sidecars intact. Use [`Self::close`] instead when the resulting file is
    /// to be handed over.
    pub fn into_connection(mut self) -> Connection {
        // Taking the connection leaves `self.conn == None`, so the `Drop` below
        // runs its finalise on nothing.
        self.conn
            .take()
            .expect("connection is present until into_connection consumes it")
    }

    /// Closes the GeoPackage, finalising a [`JournalMode::Wal`] file back to a
    /// single [`JournalMode::Delete`] file.
    ///
    /// For a WAL handle this checkpoints the WAL (`TRUNCATE`) and resets the
    /// journal mode to `DELETE`, removing the `-wal`/`-shm` sidecars, then
    /// closes the connection. For a non-WAL handle it is just a close. Prefer
    /// this to a plain drop when you want to observe any finalisation error; the
    /// drop path does the same work best-effort and swallows errors.
    pub fn close(mut self) -> Result<()> {
        if self.journal_mode == JournalMode::Wal
            && let Some(conn) = self.conn.as_ref()
        {
            finalize_wal_to_delete(conn)?;
            // Recorded so the `Drop` below sees a settled file and does nothing.
            self.journal_mode = JournalMode::Delete;
        }
        Ok(())
    }
}

impl Drop for GeoPackage {
    fn drop(&mut self) {
        // Interchange-first: a WAL handle resets the file to a single DELETE
        // file. Best-effort and must never panic: an un-checkpointed WAL file
        // is still valid and recovers on next open.
        if self.journal_mode == JournalMode::Wal
            && let Some(conn) = self.conn.as_ref()
            && finalize_wal_to_delete(conn).is_err()
        {
            // Deliberately ignored: nothing actionable in `Drop`.
        }
    }
}

/// Applies `options` to a freshly opened connection, returning the journal
/// mode the resulting handle is responsible for finalising.
///
/// The synchronous level, when set, is always applied. The journal mode is
/// applied only when `apply_journal` is true (false for a read-only connection,
/// which cannot change it) and a mode is set; an unset journal mode leaves the
/// file as it is. The returned [`JournalMode`] is [`JournalMode::Wal`] only when
/// WAL was actually applied, so only then do close/drop reset the file.
fn apply_open_options(
    conn: &Connection,
    options: OpenOptions,
    apply_journal: bool,
) -> Result<JournalMode> {
    // Set first, so that it covers the pragmas below as well as everything the
    // caller goes on to do. Applied to read-only connections too: a reader can
    // meet a writer's lock under a rollback journal. `None` only where the
    // caller supplied the connection and asked for nothing, so it keeps what it
    // has.
    if let Some(timeout) = options.busy_timeout {
        conn.busy_timeout(timeout)?;
    }
    if let Some(synchronous) = options.synchronous {
        conn.pragma_update(None, "synchronous", synchronous.code())?;
    }
    if apply_journal && let Some(mode) = options.journal_mode {
        // journal_mode returns the resulting mode as a row, so it is a query,
        // not a plain pragma_update; the keyword comes from a typed enum.
        conn.query_row(
            &format!("PRAGMA journal_mode = {}", mode.keyword()),
            [],
            |_| Ok(()),
        )?;
        return Ok(mode);
    }
    Ok(JournalMode::Delete)
}

/// Checkpoints a WAL database fully into the main file and resets it to the
/// `DELETE` rollback journal, removing the `-wal`/`-shm` sidecars.
fn finalize_wal_to_delete(conn: &Connection) -> Result<()> {
    // TRUNCATE flushes every committed frame into the main database and shrinks
    // the WAL to zero; the mode switch then removes the sidecar files.
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    conn.query_row("PRAGMA journal_mode = DELETE", [], |_| Ok(()))?;
    // Apple's system SQLite enables persist-WAL by default, which leaves the
    // sidecar files behind after the mode switch (stock SQLite deletes them
    // here). The switch above succeeds only when no other connection is still
    // using WAL, so removing a leftover is safe, and it is what a stock build
    // would already have done. rusqlite exposes no `sqlite3_file_control`, so
    // the flag itself cannot be turned off without `unsafe`, which this crate
    // forbids. Best-effort: a missing file is the common case.
    if let Some(path) = conn.path() {
        for suffix in ["-wal", "-shm"] {
            if std::fs::remove_file(format!("{path}{suffix}")).is_err() {
                // Already absent (every non-Apple build), or unremovable, in
                // which case the file is stale but harmless.
            }
        }
    }
    Ok(())
}

/// Reads a 32-bit SQLite database-header pragma (`application_id` or
/// `user_version`) as a `u32`.
///
/// SQLite reports these fields sign-extended into an `i64`; the values are
/// 32-bit magics, so reinterpreting the low 32 bits as unsigned (which is what
/// the sign-extension preserves) is the intended read.
#[expect(
    clippy::cast_sign_loss,
    reason = "application_id/user_version are 32-bit header magics; reading their low bits as unsigned is intentional, and preserves the bit pattern for any header value"
)]
pub(crate) fn read_header_u32(conn: &Connection, pragma: &str) -> rusqlite::Result<u32> {
    Ok(conn.pragma_query_value(None, pragma, |r| r.get::<_, i64>(0))? as u32)
}

/// Verifies the linked SQLite has the RTree module compiled in.
///
/// The GeoPackage spatial index is an `rtree` virtual table; without
/// `SQLITE_ENABLE_RTREE`, every indexed layer fails at first use with
/// SQLite's own "no such module: rtree". The bundled build always includes
/// the module; a system SQLite may not, so this runs once per connection, up
/// front, where [`Error::RtreeUnavailable`] can name the remedy. A build
/// under `SQLITE_OMIT_COMPILEOPTION_DIAGS` cannot answer the question and is
/// taken as capable; the later operations then report the truth.
fn ensure_rtree_available(conn: &Connection) -> Result<()> {
    let enabled = conn
        .query_row(
            "SELECT sqlite_compileoption_used('SQLITE_ENABLE_RTREE')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .unwrap_or(true);
    if enabled {
        Ok(())
    } else {
        Err(Error::RtreeUnavailable)
    }
}

pub(crate) fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type IN ('table','view') AND name = ?1",
        [name],
        |_| Ok(()),
    )
    .optional()
    .map(|o| o.is_some())
}

/// Resolves `name` to the actual SQLite table (or view) name, matching
/// case-insensitively.
///
/// SQLite object names resolve case-insensitively, but joins between catalogue
/// tables (`gpkg_contents`, `gpkg_geometry_columns`) compare the stored strings
/// exactly. This returns the physical name as SQLite stores it, identical to
/// `name` for a well-formed file, differing only in case for the wrong-case
/// files [`GeoPackage::open_lenient`] accepts. `None` when no such table
/// exists under any casing.
pub(crate) fn resolve_table_name(
    conn: &Connection,
    name: &str,
) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT name FROM sqlite_master \
         WHERE type IN ('table','view') AND name = ?1 COLLATE NOCASE",
        [name],
        |r| r.get::<_, String>(0),
    )
    .optional()
}

/// A row of `gpkg_contents`.
#[derive(Debug, Clone, PartialEq)]
pub struct ContentsEntry {
    /// User data table name.
    pub table_name: String,
    /// Declared data type.
    pub data_type: ContentsDataType,
    /// Human-readable identifier.
    pub identifier: Option<String>,
    /// Spatial reference system of the table's contents.
    pub srs_id: Option<i32>,
    /// Bounding box minimum x (informational; may be NULL).
    pub min_x: Option<f64>,
    /// Bounding box minimum y.
    pub min_y: Option<f64>,
    /// Bounding box maximum x.
    pub max_x: Option<f64>,
    /// Bounding box maximum y.
    pub max_y: Option<f64>,
}

/// `gpkg_contents.data_type` values.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ContentsDataType {
    /// Vector features.
    Features,
    /// Tile pyramid.
    Tiles,
    /// Non-spatial attributes.
    Attributes,
    /// Any other (extension-defined) data type.
    Other(String),
}

impl ContentsDataType {
    fn from_str(s: &str) -> Self {
        match s {
            "features" => Self::Features,
            "tiles" => Self::Tiles,
            "attributes" => Self::Attributes,
            other => Self::Other(other.to_owned()),
        }
    }
}
