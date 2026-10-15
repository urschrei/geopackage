//! Read and write [OGC GeoPackage](https://www.geopackage.org/spec140/) files.
//!
//! This crate reads and writes feature and attribute tables, tile pyramids and
//! tiled gridded coverages. It builds and repairs spatial indexes, validates
//! files against the specification, and moves features in and out as Apache
//! Arrow record batches.
//!
//! # Overview
//!
//! - **[`GeoPackage`]**: A file, from [`GeoPackage::create`],
//!   [`GeoPackage::open`] or [`GeoPackage::open_read_only`]
//! - **[`Layer`]**: A feature or attribute table, from [`GeoPackage::layer`] or
//!   [`GeoPackage::attributes`]
//! - **[`TilePyramid`]**: A table of raster tiles, from [`GeoPackage::tiles`]
//! - **[`Coverage`]**: A tiled gridded coverage, such as an elevation model,
//!   from [`GeoPackage::coverage`]
//! - **[`OpenOptions`]**: Settings for opening a file: journal mode, lock
//!   timeout, and checks applied to writes
//!
//! The `gpkg` command-line tool, from the `geopackage-cli` crate, handles
//! common tasks without any code: `info`, `validate`, `index`, `repair`,
//! `copy`, `tiles` and `coverage`. Each GitHub release has binaries of it,
//! which `cargo binstall geopackage-cli` installs.
//!
//! # Quick start
//!
//! Create a file, define a point layer, write features, and query by bounding
//! box:
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
//! // Uses the RTree index that `create_layer` builds by default.
//! for feature in layer.features_in(BoundingBox::new(-7.0, 53.0, -6.0, 54.0))? {
//!     let feature = feature?;
//!     assert_eq!(feature.value("name"), Some(ValueRef::Text("Dublin")));
//! }
//! # Ok(()) }
//! ```
//!
//! # Feature and attribute tables
//!
//! - **[`GeoPackage::create_layer`]**, **[`GeoPackage::create_attributes_table`]**:
//!   Create a table from a [`TableSchemaBuilder`]
//! - **[`Layer::features`]**: Read every row
//! - **[`Layer::features_in`]**: Read the rows that intersect a bounding box,
//!   using the spatial index if there is one
//! - **[`Layer::select`]**: Read the rows that match an SQL `WHERE` clause
//! - **[`Layer::cursor`]**, **[`Layer::cursor_in`]**, **[`Layer::cursor_select`]**:
//!   Stream rows one at a time instead of collecting them
//! - **[`Layer::write_all`]**: Insert many features in batched transactions
//! - **[`Layer::writer`]**: Insert, update and delete individual rows in one
//!   transaction
//! - **[`GeoPackage::add_epsg_srs`]**: Register an EPSG spatial reference
//!   system
//!
//! # Geometry
//!
//! [`Feature::geometry`] returns a [`GpbGeometry`](core::GpbGeometry), a view
//! over the stored bytes that implements [`geo_traits::GeometryTrait`]. Every
//! write method accepts any `impl GeometryTrait<T = f64>`, including all
//! `geo-types` geometries. A geometry can therefore be read from one file,
//! measured, and written to another without being converted: the only
//! allocations are the row's blob and the newly encoded output.
//!
//! ```
//! use geo_traits::{CoordTrait, GeometryTrait, GeometryType as Kind, LineStringTrait};
//! use geopackage::core::types::{ColumnType, GeometryType};
//! use geopackage::{ColumnSpec, GeoPackage, GeometrySpec, TableSchemaBuilder, Value, ValueRef};
//!
//! /// Planar length, computed from the stored coordinates.
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
//!         // Both calls read the same borrowed view.
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
//! A tile pyramid stores pre-rendered raster tiles by zoom level, column and
//! row. Create one with [`GeoPackage::create_tile_pyramid`], open one with
//! [`GeoPackage::tiles`], and list them with [`GeoPackage::tile_pyramids`].
//!
//! Tiles are stored as bytes and never decoded. Only the image header is read,
//! so that a tile with the wrong pixel size or a disallowed format is rejected.
//! Rendering tiles, or building a pyramid from a raster, requires an image
//! library or GDAL.
//!
//! Rows are numbered from the top of the pyramid's own extent, as in WMTS and
//! XYZ, not from the bottom as in TMS.
//! [`TileMatrix::flip_row`](core::tiles::TileMatrix::flip_row) converts between
//! the two, and [`TileMatrixSet::xyz_to_tile`](core::tiles::TileMatrixSet::xyz_to_tile)
//! returns an error if the pyramid isn't the standard Web Mercator grid.
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
//! // Zoom levels 0 to 4, each doubling the grid of the one above.
//! let matrix_set = TileMatrixSet::web_mercator_quad();
//! let matrices = matrix_set.ladder(ZoomLadder::new(0, 4))?;
//! let tiles = gpkg.create_tile_pyramid(
//!     &TilePyramidBuilder::new("basemap", matrix_set).matrices(matrices),
//! )?;
//!
//! tiles.put_tile(TileCoord::new(1, 0, 0), &png(256, 256))?;
//! assert!(tiles.get_tile(TileCoord::new(1, 0, 0))?.is_some());
//!
//! // The cursor lends out each payload without copying it.
//! let mut cursor = tiles.cursor()?;
//! let mut stream = cursor.tiles()?;
//! while let Some(tile) = stream.next()? {
//!     assert_eq!(tile.data().len(), 33);
//! }
//! # Ok(()) }
//! ```
//!
//! # Coverages
//!
//! A tiled gridded coverage is a tile pyramid whose tiles hold measurements,
//! such as elevations, instead of images. Create one with
//! [`GeoPackage::create_coverage`] and open one with [`GeoPackage::coverage`].
//! As with tiles, samples are never decoded: [`Coverage::value`] applies the
//! coverage's scale, offset and null value to a sample you have decoded
//! yourself.
//!
//! # Arrow
//!
//! With the `arrow` feature, [`Layer::read_arrow`] reads a layer as Arrow
//! record batches, using multiple threads by default, and [`Layer::write_arrow`]
//! writes them using the same code as [`Layer::write_all`]. Geometry is a
//! GeoArrow WKB column, with the CRS stored in its metadata as PROJJSON.
//! [`TableSchemaBuilder::from_arrow_schema`] defines a layer from an Arrow
//! schema, so a layer can be copied without restating its columns. The
//! [`arrow`] module documents the type mapping.
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
//! [`GeoPackage::extensions`] lists the extensions a file declares in
//! `gpkg_extensions`, and [`Layer::extensions`] and [`TilePyramid::extensions`]
//! list those that apply to one table. Each is identified as an [`Extension`],
//! with an [`ExtensionSupport`] level saying how this crate handles it.
//!
//! Writing to a table covered by an extension this crate doesn't recognise
//! fails with [`Error::UnsupportedExtension`], because the extension may
//! constrain that table in ways this crate can't respect. Reading is
//! unaffected. [`GeoPackage::blocking_extension`] checks a table in advance,
//! and [`OpenOptions::allow_unsupported_extension_writes`] turns the check off.
//!
//! These extensions have their own API:
//!
//! - **`gpkg_crs_wkt`**: WKT2 CRS definitions and coordinate epochs, on [`Srs`]
//! - **`gpkg_schema`**: Column descriptions and value constraints. See
//!   [`GeoPackage::data_columns`], [`GeoPackage::column_constraint`] and the
//!   example below
//! - **`gpkg_metadata`**: Metadata records attached to the file, a table, a
//!   column or a row. See [`GeoPackage::metadata`] and
//!   [`GeoPackage::add_metadata`]
//! - **Related Tables** (OGC 18-000): Relationships between tables. See
//!   [`GeoPackage::relations`] and [`GeoPackage::add_relation`]
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
//! // The format doesn't enforce constraints, so checking writes against them
//! // is opt-in.
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
//! # Validation
//!
//! [`GeoPackage::validate`] checks a file and returns a [`Finding`] for each
//! problem, with a [`Severity`] and, where possible, advice on how to fix it.
//! It doesn't modify the file. [`GeoPackage::open_lenient`] opens legacy and
//! slightly malformed files that [`GeoPackage::open`] rejects, and records
//! what it tolerated as [`OpenWarning`]s.
//!
//! # Configuration
//!
//! The defaults suit the common case: a single-file GeoPackage with indexed
//! layers, read the same way other popular implementations read it. Each
//! setting's documentation describes the trade-offs.
//!
//! | Setting | Default |
//! |---|---|
//! | [`OpenOptions::journal_mode`] | Unchanged; [`JournalMode::Wal`] is opt-in |
//! | [`OpenOptions::synchronous`] | Unchanged |
//! | [`OpenOptions::busy_timeout`] | [`DEFAULT_BUSY_TIMEOUT`] (5 seconds; SQLite's own default is not to wait) |
//! | [`TableSchemaBuilder::primary_key`] | [`DEFAULT_PRIMARY_KEY`] (`fid`) |
//! | [`GeometrySpec::column_name`] | [`DEFAULT_GEOMETRY_COLUMN`] (`geom`) |
//! | [`TableSchemaBuilder::spatial_index`] | `true` |
//! | [`BulkIndexOptions::bulk_threshold`] | [`DEFAULT_BULK_THRESHOLD`] (10,000 rows) |
//! | [`BulkIndexOptions::verification`] | [`BulkVerification::None`] |
//! | [`BulkIndexOptions::fill_factor`] | [`DEFAULT_FILL_FACTOR`] (`1.0`) |
//! | [`TilePyramidBuilder::allow_zoom_other`] | `false` |
//! | [`ConversionOptions::with_datetime`] | [`DateTimeParsing::Strict`] |
//! | [`ConversionOptions::with_storage`] | [`StorageStrictness::Lenient`] |
//! | [`Layer::with_geometry_type_validation`] | Off |
//! | [`Layer::with_columns`], [`Layer::without_geometry`] | All columns |
//! | [`ArrowReadOptions::batch_size`](arrow::ArrowReadOptions::batch_size) | [`DEFAULT_BATCH_SIZE`](arrow::DEFAULT_BATCH_SIZE) (65,536 rows) |
//! | [`ArrowReadOptions::threads`](arrow::ArrowReadOptions::threads) | `0`: `min(4, available parallelism)` |
//! | [`ArrowReadOptions::max_batch_bytes`](arrow::ArrowReadOptions::max_batch_bytes) | [`default_max_batch_bytes`](arrow::default_max_batch_bytes): `min(i32::MAX, RAM / 4)` |
//!
//! A few notes:
//!
//! - A handle opened in WAL mode switches the file back to a single
//!   `DELETE`-journal file when it closes, so the `.gpkg` has no sidecar files.
//!   See [`GeoPackage`].
//! - [`BulkIndexOptions`] controls how an RTree index is built, through
//!   [`Layer::create_spatial_index_with`], [`Layer::write_all_with`] and
//!   [`Layer::write_arrow_with`]. At or above the bulk threshold the index is
//!   brought up to date once, after the rows are written, instead of row by
//!   row through triggers.
//! - Leaving out the geometry with [`Layer::without_geometry`] saves copying
//!   it into every row when you only need attributes.
//! - [`Layer::write_all`] and [`Layer::write_arrow`] take a `batch_size`: the
//!   number of rows per transaction, or `0` for a single transaction.
//! - Arrow's 32-bit offsets limit a batch to 2 GB of WKB, so a batch that would
//!   exceed `max_batch_bytes` is emitted early instead.
//!
//! For anything else, [`GeoPackage::connection`] returns the underlying
//! `rusqlite` connection.
//!
//! # Which calls write
//!
//! Most calls either read or write, but three depend on the state of the
//! file. [`Layer::extent`] stores the extent it measures if the recorded one
//! is unusable. [`Layer::repair_spatial_index`] writes only if there is
//! something to repair. [`Layer::writer`] begins a deferred transaction,
//! which takes no lock until a row is written, so on a read-only connection
//! the first row fails rather than the call.
//!
//! | Call | Writes to the file | On a read-only connection |
//! |---|---|---|
//! | [`Layer::features`], [`Layer::cursor`], [`Layer::features_in`], [`Layer::select`] | Never | Works |
//! | [`Layer::spatial_index_status`], [`Layer::has_spatial_index`] | Never | Works |
//! | [`Layer::audit_spatial_index`] | Never | Works |
//! | [`GeoPackage::contents`] | Never | Works |
//! | [`Layer::extent`] | Only if the recorded extent is unusable | Works; the extent is returned but not stored |
//! | [`Layer::repair_spatial_index`] | Only if the triggers are out of date | Works if there is nothing to repair |
//! | [`Layer::recompute_extent`] | Always | Fails |
//! | [`Layer::create_spatial_index`], [`Layer::drop_spatial_index`], [`Layer::rebuild_spatial_index`] | Always | Fails |
//! | [`Layer::writer`] | On row operations and commit | Opens; the first row fails |
//! | [`Layer::write_all`], [`Layer::write_arrow`] | Always | Fails |
//! | [`GeoPackage::tiles`], [`TilePyramid::get_tile`], [`TilePyramid::cursor`] | Never | Works |
//! | [`TilePyramid::validate`] | Never | Works |
//! | [`TilePyramid::put_tile`], [`TilePyramid::delete_tile`], [`TilePyramid::write_all`] | Always | Fails |
//! | [`TilePyramid::writer`] | On tile operations and commit | Opens; the first tile fails |
//!
//! Storing the extent fixes the file for later readers as well. See
//! [`Layer::extent`] for how to avoid the write.
//!
//! ## Errors
//!
//! - **Read-only connection**: [`Error::Sqlite`] with `SQLITE_READONLY`.
//!   [`Layer::extent`] returns its measurement instead.
//! - **Write lock held by another connection**: The call waits for up to
//!   [`OpenOptions::busy_timeout`], then fails with `SQLITE_BUSY`.
//!   [`Layer::extent`] returns its measurement and leaves the file unchanged.
//!   SQLite doesn't wait when waiting can't help: a read-to-write upgrade
//!   under a rollback journal, or a stale snapshot under WAL.
//! - **No spatial index**: [`Error::NoSpatialIndex`] from
//!   [`Layer::audit_spatial_index`], [`Layer::rebuild_spatial_index`] and
//!   [`Layer::repair_spatial_index`].
//! - **No geometry column**: [`Error::NoGeometryColumn`] from
//!   [`Layer::extent`], [`Layer::recompute_extent`], [`Layer::features_in`] and
//!   [`Layer::cursor_in`].
//! - **Any other write failure**, such as a full disk: [`Error::ExtentPersist`]
//!   from [`Layer::extent`], which includes the measurement, and
//!   [`Error::Sqlite`] from everything else.
//!
//! # Cargo features
//!
//! - **`geo-types`** (default): Enables
//!   [`GpbGeometry::to_geo`](geopackage_core::geometry::GpbGeometry::to_geo),
//!   which converts a stored geometry to a `geo-types` geometry
//! - **`arrow`**: Arrow record batch I/O. Adds dependencies on `arrow-array`
//!   and `arrow-schema`
//! - **`bundled`**: Compile and statically link a vendored SQLite instead of
//!   linking the system library. Requires a C compiler. Required on Windows
//!
//!   Set these SQLite compile flags in your `.cargo/config.toml` when you
//!   enable `bundled`:
//!
//!   ```toml
//!   [env]
//!   LIBSQLITE3_FLAGS = "-USQLITE_ENABLE_MEMORY_MANAGEMENT -DSQLITE_DEFAULT_MEMSTATUS=0"
//!   ```
//!
//!   Without them, all connections in a process share one SQLite page cache
//!   behind one mutex. Threads that read from different connections, such as
//!   those of a threaded [`Layer::read_arrow`], then wait for each other. The
//!   system SQLite on Linux and macOS is built without
//!   `SQLITE_ENABLE_MEMORY_MANAGEMENT`.
//!
//! Without `bundled`, you need SQLite's development files (`libsqlite3-dev` on
//! Debian and Ubuntu; macOS includes them), and SQLite must be built with the
//! RTree module. Opening a file checks this, and returns
//! [`Error::RtreeUnavailable`] if the module is missing.

// `unsafe_code = "forbid"` and `missing_docs = "warn"` come from the
// workspace lints table (root Cargo.toml). This crate never uses `unsafe`; the
// `geopackage-ffi` crate is the sole exception, and opts out of the workspace
// lints rather than relaxing them here.

#[cfg(feature = "arrow")]
pub mod arrow;
#[cfg(doctest)]
mod book;
mod bulk;
mod coverage;
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
pub use coverage::{Coverage, CoverageAncillary, CoverageBuilder, CoverageWriter, TileAncillary};
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
/// sidecar files while it is open. On [`GeoPackage::close`], and on drop, the
/// handle checkpoints the WAL and resets the file to
/// [`JournalMode::Delete`], so the resulting `.gpkg` is a single file. Prefer
/// [`GeoPackage::close`], which returns any error. Dropping the handle does
/// the same work on a best-effort basis and never panics.
/// [`GeoPackage::into_connection`] opts out of this guarantee: the returned
/// connection keeps whatever journal mode it was in.
pub struct GeoPackage {
    /// `Some` for the whole lifetime of the handle. Only
    /// [`Self::into_connection`] takes it, which is why the handle can
    /// implement `Drop`.
    conn: Option<Connection>,
    version: GpkgVersion,
    warnings: Vec<OpenWarning>,
    /// The journal mode this handle must finalise. It is `Wal` only when the
    /// handle opted into WAL and must reset the file to `Delete` on close or
    /// drop.
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
    /// Use [`OpenOptions`] to create the file in [`JournalMode::Wal`] or with
    /// an explicit [`Synchronous`] level.
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

    /// Wraps an open connection with default options. Checks that it is a
    /// GeoPackage and registers the required SQL functions.
    pub fn from_connection(conn: Connection) -> Result<Self> {
        // A caller-supplied connection is left in whatever journal mode it
        // already has (no journal pragma applied).
        Self::from_connection_configured(conn, OpenOptions::new(), false)
    }

    /// Implements `create`, applying `options` before seeding the schema.
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

    /// Implements `open` and `open_read_only`, applying `options` after
    /// opening.
    pub(crate) fn open_configured(
        path: &Path,
        flags: OpenFlags,
        options: OpenOptions,
    ) -> Result<Self> {
        let options = options.with_default_busy_timeout();
        // A `Connection` is not `Sync`, so SQLite's per-connection mutex
        // protects nothing. Without NO_MUTEX it locks around each column read.
        let conn = Connection::open_with_flags(path, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
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

    /// Returns the underlying connection, for full SQL access.
    pub fn connection(&self) -> &Connection {
        self.conn
            .as_ref()
            .expect("connection is present for the whole handle lifetime")
    }

    /// Consumes the handle and returns the underlying connection.
    ///
    /// This **opts out** of the interchange-first close guarantee. The returned
    /// connection keeps whatever journal mode it is in, so a handle in
    /// [`JournalMode::Wal`] returns a WAL connection with its `-wal`/`-shm`
    /// sidecars intact. Use [`Self::close`] instead if you are going to hand
    /// the file over.
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
    /// For a WAL handle, this checkpoints the WAL (`TRUNCATE`), resets the
    /// journal mode to `DELETE`, which removes the `-wal`/`-shm` sidecars, and
    /// then closes the connection. Any other handle is simply closed. Use this
    /// instead of dropping the handle if you want to see finalisation errors.
    /// Dropping does the same work on a best-effort basis and discards errors.
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

/// Applies `options` to a newly opened connection and returns the journal mode
/// that the resulting handle must finalise.
///
/// The synchronous level, if set, is always applied. The journal mode is
/// applied only if a mode is set and `apply_journal` is true. It is false for a
/// read-only connection, which can't change its journal mode. If no journal
/// mode is set, the file keeps its current one. The returned [`JournalMode`] is
/// [`JournalMode::Wal`] only if WAL was actually applied, so only then do close
/// and drop reset the file.
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
    if let Some(bytes) = options.mmap_size {
        // The pragma returns the size it set, so it is a query, not a plain
        // pragma_update. SQLite clamps the value to its compile-time ceiling.
        conn.query_row(&format!("PRAGMA mmap_size = {bytes}"), [], |_| Ok(()))?;
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
/// SQLite reports these fields sign-extended into an `i64`. The values are
/// 32-bit magic numbers, so the intended read takes the low 32 bits, which the
/// sign extension preserves, as unsigned.
#[expect(
    clippy::cast_sign_loss,
    reason = "application_id/user_version are 32-bit header magics; reading their low bits as unsigned is intentional, and preserves the bit pattern for any header value"
)]
pub(crate) fn read_header_u32(conn: &Connection, pragma: &str) -> rusqlite::Result<u32> {
    Ok(conn.pragma_query_value(None, pragma, |r| r.get::<_, i64>(0))? as u32)
}

/// Checks that the linked SQLite has the RTree module compiled in.
///
/// The GeoPackage spatial index is an `rtree` virtual table. Without
/// `SQLITE_ENABLE_RTREE`, every indexed layer fails on first use with SQLite's
/// own "no such module: rtree". The bundled build always includes the module,
/// but a system SQLite may not. This check runs once per connection, up front,
/// so that [`Error::RtreeUnavailable`] can name the remedy. A build with
/// `SQLITE_OMIT_COMPILEOPTION_DIAGS` can't report its options, so it is
/// assumed to have the module. Later operations then show whether it does.
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
/// exactly. This returns the name as SQLite stores it. For a well-formed file
/// it is identical to `name`. For the wrong-case files that
/// [`GeoPackage::open_lenient`] accepts, it differs only in case. Returns
/// `None` if no such table exists under any casing.
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
    /// Tiled gridded coverage, a tile pyramid whose tiles hold measurements
    /// instead of images (`2d-gridded-coverage`, OGC 17-066r2
    /// Requirement 5).
    ///
    /// Open it with [`GeoPackage::coverage`], not with [`GeoPackage::tiles`].
    Coverage,
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
            geopackage_core::coverage::COVERAGE_DATA_TYPE => Self::Coverage,
            "attributes" => Self::Attributes,
            other => Self::Other(other.to_owned()),
        }
    }
}
